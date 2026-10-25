//! Fixtures shared by the integration tests.
//!
//! The transport fixtures run offline: endpoints use the `Minimal` preset with relays
//! disabled and dial each other over loopback, so no test depends on a relay or on the
//! internet. The server fixture registers the production protocol table through
//! `pirohxy::egress_router`, so what a test reaches is what `pirohxy egress` serves.
//!
//! The CLI fixtures at the end of this file are the exception, and deliberately so: they
//! run the real binary, which builds its node the way a deployment does. A CLI test
//! therefore exercises the transport a real user exercises, relay included.

// The fixtures are one module shared by several test binaries, and no single binary uses
// all of them, so the usual "never used" and "unreachable pub" lints only produce noise.
#![expect(
	dead_code,
	unreachable_pub,
	reason = "the fixtures are shared by several test binaries, and no one binary uses them all"
)]

extern crate alloc;

use alloc::sync::Arc;
use core::{net::SocketAddr, time::Duration};
use std::{
	collections::HashMap,
	ffi::{OsStr, OsString},
	fs,
	path::{Path, PathBuf},
	process::Stdio,
};

use color_eyre::eyre::{Context, Result, ensure, eyre};
use iroh::{
	Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, endpoint::presets, protocol::Router,
};
use pirohxy::{
	egress_router,
	protocol::{client::connect::Client, exec::ExecCommand},
	protoutils::{HostPort, alpn::Protocol},
};
use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	process::{Child, Command},
	task::JoinHandle,
};

/// How long one step of a test waits before it gives up.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// The bytes an [`EchoTarget`] answers every connection with.
pub const ECHO_PAYLOAD: &[u8] = b"hello-from-target\n";

/// The directory a test may write to, unique to `name` and recreated on every run.
///
/// # Errors
/// The directory could not be cleared or created.
pub fn work_dir(name: &str) -> Result<PathBuf> {
	let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
	if dir.exists() {
		fs::remove_dir_all(&dir).wrap_err_with(|| format!("clearing {}", dir.display()))?;
	}
	fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
	Ok(dir)
}

/// Binds an offline endpoint: `Minimal` preset, relays off, so dialing uses its bound address.
///
/// # Errors
/// The endpoint cannot be bound.
pub async fn test_endpoint(key: SecretKey) -> Result<Endpoint> {
	Ok(Endpoint::builder(presets::Minimal)
		.secret_key(key)
		.relay_mode(RelayMode::Disabled)
		.bind()
		.await?)
}

/// Which protocols each identity may use on a test server.
#[derive(Debug, Clone, Default)]
pub struct Grants {
	table: HashMap<EndpointId, Vec<Protocol>>,
}

impl Grants {
	/// A table that grants nothing.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Grants `protocols` to the identity `id`.
	#[must_use]
	pub fn allow(mut self, id: EndpointId, protocols: &[Protocol]) -> Self {
		self.table
			.entry(id)
			.or_default()
			.extend_from_slice(protocols);
		self
	}

	/// The closure `egress_router` gates every protocol with.
	pub fn authorizer(&self) -> impl Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static {
		let table = Arc::new(self.table.clone());
		move |id, protocol| {
			table
				.get(&id)
				.is_some_and(|grants| grants.contains(&protocol))
		}
	}
}

/// A server running the production protocol table under a grant table.
pub struct TestServer {
	endpoint: Endpoint,
	file_root: PathBuf,
	router: Router,
}

impl TestServer {
	/// Binds the server and registers every protocol, gated by `grants`.
	///
	/// # Errors
	/// The endpoint cannot be bound, or the file root cannot be created.
	pub async fn spawn(grants: &Grants, file_root: PathBuf) -> Result<Self> {
		tokio::fs::create_dir_all(&file_root).await?;
		let endpoint = test_endpoint(SecretKey::generate()).await?;
		let router = egress_router(
			endpoint.clone(),
			&file_root,
			None,
			Duration::from_secs(5),
			grants.authorizer(),
		)
		.spawn();
		Ok(Self {
			endpoint,
			file_root,
			router,
		})
	}

	/// The address a client dials to reach this server.
	#[must_use]
	pub fn addr(&self) -> EndpointAddr {
		self.endpoint.addr()
	}

	/// The identity this server presents itself as.
	#[must_use]
	pub fn id(&self) -> EndpointId {
		self.endpoint.id()
	}

	/// The directory its `file` protocol is confined to.
	#[must_use]
	pub fn file_root(&self) -> &Path {
		&self.file_root
	}

	/// Stops the server and releases every handler it registered.
	pub async fn close(&self) {
		let _stopped = self.router.shutdown().await;
		self.endpoint.close().await;
	}
}

/// A client identity: a node with a throwaway key plus the master key it authenticates with.
///
/// The master key is what a server grants protocols to, exactly as in production, where the
/// node's own key is thrown away after every command.
pub struct TestClient {
	master: SecretKey,
	endpoint: Endpoint,
}

impl TestClient {
	/// Binds a node with a throwaway identity and a fresh master key.
	///
	/// # Errors
	/// The endpoint cannot be bound.
	pub async fn spawn() -> Result<Self> {
		Ok(Self {
			master: SecretKey::generate(),
			endpoint: test_endpoint(SecretKey::generate()).await?,
		})
	}

	/// The identity the server grants protocols to.
	#[must_use]
	pub fn id(&self) -> EndpointId {
		self.master.public()
	}

	/// The node this client dials from. Several protocols share it.
	#[must_use]
	pub fn endpoint(&self) -> &Endpoint {
		&self.endpoint
	}

	/// Opens an authenticated connection to `server` for `protocol`.
	///
	/// Keep the returned client alive for as long as its connection is used: dropping it
	/// closes the connection.
	///
	/// # Errors
	/// The server cannot be reached, or it refuses this identity for this protocol.
	pub async fn connect(&self, server: &TestServer, protocol: Protocol) -> Result<Client> {
		Client::connect_with(
			self.endpoint(),
			server.addr(),
			protocol,
			self.master.clone(),
		)
		.await
	}

	/// Closes the node and everything running over it.
	pub async fn close(&self) {
		self.endpoint.close().await;
	}
}

/// A TCP listener that answers every connection with `payload` and then closes.
pub struct EchoTarget {
	addr: SocketAddr,
	task: JoinHandle<()>,
}

impl EchoTarget {
	/// Binds the listener on a free loopback port.
	///
	/// # Errors
	/// No loopback port could be bound.
	pub async fn spawn(payload: &'static [u8]) -> Result<Self> {
		let listener = TcpListener::bind("127.0.0.1:0").await?;
		let addr = listener.local_addr()?;
		let task = tokio::spawn(async move {
			loop {
				let Ok((mut socket, _peer)) = listener.accept().await else {
					break;
				};
				let _writer = tokio::spawn(async move {
					let _written = socket.write_all(payload).await;
					let _closed = socket.shutdown().await;
				});
			}
		});
		Ok(Self { addr, task })
	}

	/// The address tunnels and proxies connect to.
	#[must_use]
	pub fn addr(&self) -> SocketAddr {
		self.addr
	}

	/// The same address in the form the tunnel protocols carry on the wire.
	#[must_use]
	pub fn host_port(&self) -> HostPort {
		HostPort::from(self.addr)
	}
}

impl Drop for EchoTarget {
	fn drop(&mut self) {
		self.task.abort();
	}
}

/// A loopback port held open so nothing else can be handed the same number.
///
/// A test that needs a fixed port cannot just ask the OS for a free one and let go of it:
/// between the release and the moment the process binds it, another test can take it, and
/// the process then fails for a reason that has nothing to do with the protocol under test.
/// Holding the port narrows that window; [`start_bound`] closes it by starting again on a
/// fresh port.
pub struct ReservedPort {
	listener: TcpListener,
	port: u16,
}

impl ReservedPort {
	/// Binds a loopback port and holds it until this guard is dropped or released.
	///
	/// # Errors
	/// No loopback port could be bound.
	pub async fn hold() -> Result<Self> {
		let listener = TcpListener::bind("127.0.0.1:0").await?;
		let port = listener.local_addr()?.port();
		Ok(Self { listener, port })
	}

	/// The port. It is bound, so nothing else can use it yet.
	#[must_use]
	pub fn port(&self) -> u16 {
		self.port
	}

	/// Lets go of the port and hands back the number, so a process can bind it.
	#[must_use]
	pub fn release(self) -> u16 {
		let Self { listener, port } = self;
		drop(listener);
		port
	}
}

/// A port that was free at the time of the call, for a process that must bind a fixed address.
///
/// Prefer [`start_bound`] when that process is a `pirohxy` command: this lets go of the port
/// before the command binds it, so another test can still win the race in between.
///
/// # Errors
/// No loopback port could be bound.
pub async fn free_port() -> Result<u16> {
	Ok(ReservedPort::hold().await?.release())
}

/// The program and flags that run `line` through this platform's shell.
///
/// Tests that hand a command to the CLI as arguments use this directly; tests that build an
/// [`ExecCommand`] use [`shell_command`].
#[must_use]
pub fn shell_args(line: &str) -> Vec<String> {
	#[cfg(windows)]
	let (program, flag) = ("cmd.exe", "/C");
	#[cfg(not(windows))]
	let (program, flag) = ("sh", "-c");

	vec![program.to_owned(), flag.to_owned(), line.to_owned()]
}

/// A command line run through the platform's shell, so a test can use `echo` everywhere.
#[must_use]
pub fn shell_command(line: &str) -> ExecCommand {
	let mut args = shell_args(line).into_iter().map(OsString::from);
	let program = args.next().unwrap_or_default();

	ExecCommand {
		cmd: program,
		args: args.collect(),
		stdin: None,
		env: Vec::new(),
		cwd: None,
	}
}

/// A command line that prints both the working directory it was started in and the
/// `GREETING` variable the test asks the server to set.
#[cfg(windows)]
pub const PRINT_ENV_LINE: &str = "echo %CD% greeting=%GREETING%";

/// A command line that prints both the working directory it was started in and the
/// `GREETING` variable the test asks the server to set.
#[cfg(not(windows))]
pub const PRINT_ENV_LINE: &str = "echo $PWD greeting=$GREETING";

/// Runs `future` under a wall-clock limit, so a stuck test fails instead of hanging.
///
/// # Errors
/// The step did not finish within [`STEP_TIMEOUT`].
pub async fn step<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
	tokio::time::timeout(STEP_TIMEOUT, future)
		.await
		.wrap_err("step timed out")?
}

/// Reads one line from a stream, so a test can check what a channel relayed.
///
/// # Errors
/// The stream closed before a line arrived.
pub async fn read_line<R>(reader: &mut tokio::io::BufReader<R>) -> Result<String>
where
	R: tokio::io::AsyncRead + Unpin,
{
	let mut line = String::new();
	let read = reader.read_line(&mut line).await?;
	color_eyre::eyre::ensure!(read > 0, "the stream closed without a line");
	Ok(line)
}

/// The `pirohxy` binary cargo built for this test run.
#[must_use]
pub fn pirohxy_bin() -> &'static Path {
	Path::new(env!("CARGO_BIN_EXE_pirohxy"))
}

/// How long a test waits for a spawned `pirohxy` process, or a port it binds, to come up.
pub const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// [`READY_TIMEOUT`], overridable with `PIROHXY_IT_TIMEOUT_SECS` so a run can be cut short
/// while a fixture is being debugged.
#[must_use]
pub fn ready_timeout() -> Duration {
	std::env::var("PIROHXY_IT_TIMEOUT_SECS")
		.ok()
		.and_then(|value| value.parse::<u64>().ok())
		.map_or(READY_TIMEOUT, Duration::from_secs)
}

/// The line a readiness probe asks the far side to echo back.
pub const READY_MARKER: &str = "pirohxy-ready";

/// What one `pirohxy` command exited with and printed.
#[derive(Debug)]
pub struct Run {
	/// The arguments the command was given, so a failure says which one it was.
	pub args: String,
	pub code: Option<i32>,
	pub stdout: String,
	pub stderr: String,
}

impl Run {
	/// Fails unless the command exited zero.
	pub fn expect_success(&self) -> Result<()> {
		ensure!(
			self.code == Some(0),
			"`pirohxy {}` exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
			self.args,
			self.code,
			self.stdout,
			self.stderr
		);
		Ok(())
	}

	/// Fails unless the command refused to do what it was asked to do.
	pub fn expect_failure(&self) -> Result<()> {
		ensure!(
			self.code != Some(0),
			"`pirohxy {}` was expected to fail, but it succeeded\n--- stdout ---\n{}\n--- stderr ---\n{}",
			self.args,
			self.stdout,
			self.stderr
		);
		Ok(())
	}

	/// Whether either stream mentions `needle`.
	pub fn mentions(&self, needle: &str) -> bool {
		self.stdout.contains(needle) || self.stderr.contains(needle)
	}
}

/// One `pirohxy` node: a config directory of its own and the identity `init` wrote into it.
pub struct Node {
	dir: PathBuf,
	id: String,
}

impl Node {
	/// Runs `pirohxy init` in a fresh config directory named after the test.
	///
	/// # Errors
	/// `init` failed, or the identity it wrote cannot be read back.
	pub async fn init(name: &str) -> Result<Self> {
		let dir = work_dir(name)?;
		let output = Command::new(pirohxy_bin())
			.arg("--config")
			.arg(&dir)
			.arg("init")
			.stdin(Stdio::null())
			.output()
			.await?;
		ensure!(
			output.status.success(),
			"`init` failed: {}",
			String::from_utf8_lossy(&output.stderr)
		);
		let id = fs::read_to_string(dir.join("self.pub"))?.trim().to_owned();
		Ok(Self { dir, id })
	}

	/// The directory `--config` points at.
	pub fn dir(&self) -> &Path {
		&self.dir
	}

	/// This node's identity, as z32.
	pub fn id(&self) -> &str {
		&self.id
	}

	/// The directory the `file` protocol is confined to unless the server says otherwise.
	pub fn file_root(&self) -> PathBuf {
		self.dir.join("files")
	}

	/// This node as the CLI takes a server: `:<node id>`, the direct form.
	pub fn target(&self) -> String {
		format!(":{}", self.id)
	}

	/// A `pirohxy` command line rooted at this node's config directory.
	#[expect(
		unused_results,
		reason = "the builder methods hand back the command they set up"
	)]
	pub fn command(&self) -> Command {
		let mut command = Command::new(pirohxy_bin());
		command.arg("--config");
		command.arg(&self.dir);
		command
	}

	/// Writes `auth/<peer>/<protocol>`, which is what grants `peer` one protocol here.
	///
	/// # Errors
	/// The grant file cannot be written.
	pub fn grant(&self, peer: &str, protocol: &str) -> Result<()> {
		let entry = self.dir.join("auth").join(peer);
		fs::create_dir_all(&entry)?;
		Ok(fs::write(entry.join(protocol), "")?)
	}

	/// Removes every grant this node holds for `peer`.
	///
	/// # Errors
	/// The grant directory cannot be removed.
	pub fn revoke(&self, peer: &str) -> Result<()> {
		Ok(fs::remove_dir_all(self.dir.join("auth").join(peer))?)
	}

	/// Runs one command to completion and captures what it printed.
	///
	/// A non-zero exit is not an error here; it is what [`Run`] reports.
	///
	/// # Errors
	/// The binary could not be started.
	#[expect(
		unused_results,
		reason = "the builder methods hand back the command they set up"
	)]
	pub async fn run<T: AsRef<OsStr>>(&self, args: &[T]) -> Result<Run> {
		let label = args
			.iter()
			.map(|arg| arg.as_ref().to_string_lossy().into_owned())
			.collect::<Vec<String>>()
			.join(" ");
		let mut command = self.command();
		command.args(args);
		command.stdin(Stdio::null());
		let output = command.output().await?;
		Ok(Run {
			args: label,
			code: output.status.code(),
			stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
			stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
		})
	}

	/// Runs one command with `input` as its standard input, the way a pipe or a redirect
	/// gives it, and captures what it printed.
	///
	/// The pipe is closed once `input` is written, which is what a command that reads its
	/// input to the end - `cat`, a filter - needs before it will finish.
	///
	/// # Errors
	/// The binary could not be started, or its standard input cannot be written.
	#[expect(
		unused_results,
		reason = "the builder methods hand back the command they set up"
	)]
	pub async fn run_with_stdin<T: AsRef<OsStr>>(&self, args: &[T], input: &[u8]) -> Result<Run> {
		let label = args
			.iter()
			.map(|arg| arg.as_ref().to_string_lossy().into_owned())
			.collect::<Vec<String>>()
			.join(" ");
		let mut command = self.command();
		command.args(args);
		command.stdin(Stdio::piped());
		command.stdout(Stdio::piped());
		command.stderr(Stdio::piped());
		let mut child = command.spawn()?;
		// Taking the handle and dropping it is what reports the end of the input.
		if let Some(mut stdin) = child.stdin.take() {
			stdin.write_all(input).await?;
		}
		let output = child.wait_with_output().await?;
		Ok(Run {
			args: label,
			code: output.status.code(),
			stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
			stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
		})
	}
}

/// A `pirohxy egress` process, stopped when the test that started it drops it.
pub struct Server {
	node: Node,
	child: Child,
	log: PathBuf,
}

impl Server {
	/// Starts `egress` for `node` and returns once `client` can run a command on it.
	///
	/// # Errors
	/// The process cannot be started, or it never answers the readiness probe.
	pub async fn start(node: Node, client: &Node) -> Result<Self> {
		let server = Self::spawn(node)?;
		server.wait_until_ready(client).await?;
		Ok(server)
	}

	/// Starts `egress` without waiting for it, for a test that expects it to fail.
	///
	/// # Errors
	/// The process cannot be started.
	pub fn spawn(node: Node) -> Result<Self> {
		let log = node.dir().join("egress.log");
		let file = fs::File::create(&log)?;
		let child = node
			.command()
			.arg("egress")
			.stdin(Stdio::null())
			.stdout(Stdio::from(file.try_clone()?))
			.stderr(Stdio::from(file))
			.kill_on_drop(true)
			.spawn()?;
		Ok(Self { node, child, log })
	}

	/// Retries a trivial `exec` until the server answers it.
	///
	/// Readiness is proven through the protocol stack rather than through a log line: the
	/// probe only passes once the node is reachable and the grant table lets this client
	/// run a command.
	///
	/// # Errors
	/// The probe never succeeded within [`READY_TIMEOUT`].
	async fn wait_until_ready(&self, client: &Node) -> Result<()> {
		let deadline = tokio::time::Instant::now() + ready_timeout();
		let mut args = vec!["exec".to_owned(), self.node.target()];
		args.extend(shell_args(&format!("echo {READY_MARKER}")));

		let mut last = None;
		loop {
			let run = client.run(&args).await?;
			if run.code == Some(0) && run.stdout.contains(READY_MARKER) {
				return Ok(());
			}
			if tokio::time::Instant::now() > deadline {
				let last = last.ok_or_else(|| eyre!("the server never answered a probe"))?;
				return Err(eyre!(
					"the server never answered a probe\n--- last probe ---\n{last:#?}\n--- egress log ---\n{}",
					self.log_text()?
				));
			}
			last = Some(run);
			tokio::time::sleep(Duration::from_millis(500)).await;
		}
	}

	/// The node this server runs as.
	pub fn node(&self) -> &Node {
		&self.node
	}

	/// The server argument a client uses for this node.
	pub fn target(&self) -> String {
		self.node.target()
	}

	/// Everything the server has logged so far.
	pub fn log_text(&self) -> Result<String> {
		Ok(fs::read_to_string(&self.log)?)
	}

	/// Stops the server and reaps it.
	pub async fn stop(&mut self) -> Result<()> {
		self.child.start_kill()?;
		let _status = self.child.wait().await?;
		Ok(())
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		let _killed = self.child.start_kill();
	}
}

/// A `pirohxy` client command that keeps running until the test stops it: `forward`,
/// `r-forward`, `socks5`.
pub struct Task {
	args: String,
	child: Child,
	log: PathBuf,
}

impl Task {
	/// Starts `args` under `node`, sending its output to a log file the test can read.
	///
	/// # Errors
	/// The process cannot be started.
	pub fn start(node: &Node, args: &[&str]) -> Result<Self> {
		let label = args.join(" ");
		let name = args.first().unwrap_or(&"task").replace('-', "_");
		let log = node.dir().join(format!("{name}.log"));
		let file = fs::File::create(&log)?;
		let child = node
			.command()
			.args(args)
			.stdin(Stdio::null())
			.stdout(Stdio::from(file.try_clone()?))
			.stderr(Stdio::from(file))
			.kill_on_drop(true)
			.spawn()?;
		Ok(Self {
			args: label,
			child,
			log,
		})
	}

	/// Fails as soon as the command has exited, reporting its status and everything it
	/// logged.
	///
	/// A test that waits for something a long-running command is meant to do should check
	/// this while it waits: a process that is already gone is not going to do it, and saying
	/// so at once is what keeps a fixture failure readable instead of a timeout.
	///
	/// # Errors
	/// The command has already exited.
	pub fn check_alive(&mut self) -> Result<()> {
		if let Some(status) = self.child.try_wait()? {
			return Err(eyre!(
				"`pirohxy {}` exited with {status}\n--- log ---\n{}",
				self.args,
				self.log_text()
			));
		}
		Ok(())
	}

	/// Waits until something accepts a connection on the loopback `port`, giving up the
	/// moment the command exits.
	///
	/// A forward, a reverse forward and a SOCKS server each bind their port once their
	/// connection to the far side is up, so a test that connects too early has to retry
	/// rather than fail - but only while the command is alive to bind the port at all.
	///
	/// # Errors
	/// The command exited, or nothing accepted a connection on `port` within
	/// [`ready_timeout()`].
	pub async fn wait_until_listening(&mut self, port: u16) -> Result<()> {
		let deadline = tokio::time::Instant::now() + ready_timeout();
		loop {
			if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
				return Ok(());
			}
			self.check_alive()?;
			if tokio::time::Instant::now() > deadline {
				return Err(eyre!(
					"`pirohxy {}` never accepted a connection on port {port}\n--- log ---\n{}",
					self.args,
					self.log_text()
				));
			}
			tokio::time::sleep(Duration::from_millis(250)).await;
		}
	}

	/// Stops the command and reaps it.
	pub async fn stop(&mut self) -> Result<()> {
		self.child.start_kill()?;
		let _status = self.child.wait().await?;
		Ok(())
	}

	/// Everything the command has logged so far.
	pub fn log_text(&self) -> String {
		fs::read_to_string(&self.log).unwrap_or_default()
	}
}

impl Drop for Task {
	fn drop(&mut self) {
		let _killed = self.child.start_kill();
	}
}

/// How many ports [`start_bound`] tries before it gives up.
const BIND_ATTEMPTS: u8 = 3;

/// Starts a `pirohxy` command that binds a loopback port, and returns once that port
/// answers.
///
/// `make_args` builds the command line for a given port, which lets the fixture reserve the
/// port, hand it over, and start again on a fresh one if another test won it first. A
/// command that dies is reported with its own log at once, rather than waited on for the
/// whole readiness timeout.
///
/// # Errors
/// No port could be made to answer. Every attempt is reported.
pub async fn start_bound<F>(node: &Node, make_args: F) -> Result<(Task, u16)>
where
	F: Fn(u16) -> Vec<String>,
{
	let mut attempts = Vec::new();
	for attempt in 1..=BIND_ATTEMPTS {
		let port = ReservedPort::hold().await?.release();
		let args = make_args(port);
		let borrowed = args.iter().map(String::as_str).collect::<Vec<&str>>();
		let mut task = Task::start(node, &borrowed)?;
		match task.wait_until_listening(port).await {
			Ok(()) => return Ok((task, port)),
			Err(err) => attempts.push(format!("attempt {attempt}, port {port}: {err}")),
		}
	}
	Err(eyre!(
		"no port could be made to answer\n{}",
		attempts.join("\n")
	))
}
