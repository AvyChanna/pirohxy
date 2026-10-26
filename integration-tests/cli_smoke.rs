//! The CLI, driven the way a user drives it.
//!
//! Every test here runs the real `pirohxy` binary against a real `pirohxy egress`, which
//! covers what the in-process suite cannot: argument parsing, `init` and `conf`, the `auth/`
//! and `names/` directories, the exit codes a script sees, and the transport a real
//! deployment uses.
//!
//! These tests are not offline, and that is deliberate: the binary builds its node with the
//! production preset, so a CLI test needs the same relay a real user needs. The mux suite
//! next to this one is the offline half of the integration tests.

mod common;

use core::{net::SocketAddr, time::Duration};
use std::fs;

use color_eyre::eyre::{Context, Result, ensure, eyre};
use common::{
	ECHO_PAYLOAD, EchoTarget, Node, PRINT_ENV_LINE, Server, Task, read_line, ready_timeout,
	shell_args, start_bound,
};
use pirohxy::protoutils::alpn::Protocol;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt, BufReader},
	net::TcpStream,
};

/// The line the `file` tests upload.
const UPLOAD_PAYLOAD: &[u8] = b"uploaded-by-the-cli\n";
/// The line a tunnel, a reverse tunnel or a SOCKS5 proxy is expected to carry.
const ECHO_LINE: &str = "hello-from-target";

/// A command line that copies its standard input to its standard output.
#[cfg(windows)]
const COPY_STDIN_LINE: &str = "findstr .";

/// A command line that copies its standard input to its standard output.
#[cfg(not(windows))]
const COPY_STDIN_LINE: &str = "cat";

/// `exec <target> <program> <args...>`, the way the CLI takes a remote command.
fn exec_args(target: &str, line: &str) -> Vec<String> {
	let mut args = vec!["exec".to_owned(), target.to_owned()];
	args.extend(shell_args(line));
	args
}

/// `shell <target> --cmd <line>`, the way the CLI takes a remote session.
fn shell_cmd_args(target: &str, line: &str) -> Vec<String> {
	vec![
		"shell".to_owned(),
		target.to_owned(),
		"--cmd".to_owned(),
		line.to_owned(),
	]
}

/// A server and the one client allowed to use it, both freshly initialized.
async fn pair(name: &str) -> Result<(Node, Server)> {
	let client = Node::init(&format!("{name}_client")).await?;
	let server = Node::init(&format!("{name}_server")).await?;
	server.grant(client.id(), "all")?;
	let server = Server::start(server, &client).await?;
	Ok((client, server))
}

#[tokio::test]
async fn init_writes_an_identity_that_conf_reports() -> Result<()> {
	let node = Node::init("cli_init").await?;

	ensure!(
		node.dir().join("self.pub").is_file() && node.dir().join("self.priv").is_file(),
		"`init` did not leave a key pair in {}",
		node.dir().display()
	);

	let identity = node.run(&["conf", "identity"]).await?;
	identity.expect_success()?;
	ensure!(
		identity.stdout.trim() == node.id(),
		"`conf identity` printed {:?} but self.pub holds {:?}",
		identity.stdout.trim(),
		node.id()
	);

	let path = node.run(&["conf", "path"]).await?;
	path.expect_success()?;
	let expected = node
		.dir()
		.file_name()
		.unwrap_or_default()
		.to_string_lossy()
		.into_owned();
	ensure!(
		path.stdout.trim().ends_with(&expected),
		"`conf path` printed {:?}, which is not {}",
		path.stdout.trim(),
		node.dir().display()
	);

	// A second `init` must not quietly replace the identity the node is already known by.
	node.run(&["init"]).await?.expect_failure()?;

	Ok(())
}

#[tokio::test]
async fn exec_runs_a_command_and_mirrors_its_exit_code() -> Result<()> {
	let (client, server) = pair("cli_exec").await?;
	let target = server.target();

	let hello = client.run(&exec_args(&target, "echo hello-exec")).await?;
	hello.expect_success()?;
	ensure!(
		hello.stdout.contains("hello-exec"),
		"exec printed {:?} instead of the command's output",
		hello.stdout
	);

	let failed = client.run(&exec_args(&target, "exit 7")).await?;
	failed.expect_failure()?;
	ensure!(
		failed.code == Some(7),
		"a remote `exit 7` was mirrored as {:?}",
		failed.code
	);

	// Everything after the server name belongs to the remote command, this side's flags
	// included, so `--cwd` and `--env` have to be read before the program starts.
	let cwd = server.node().dir().join("cwdtest");
	fs::create_dir_all(&cwd)?;
	let cwd = cwd.to_string_lossy().into_owned();
	let mut placed = vec![
		"exec".to_owned(),
		target.clone(),
		"--cwd".to_owned(),
		cwd.clone(),
		"--env".to_owned(),
		"GREETING=hello-from-env".to_owned(),
	];
	placed.extend(shell_args(PRINT_ENV_LINE));
	let placed = client.run(&placed).await?;
	placed.expect_success()?;
	ensure!(
		placed.stdout.contains("cwdtest"),
		"`--cwd` was not applied: {:?}",
		placed.stdout
	);
	ensure!(
		placed.stdout.contains("hello-from-env"),
		"`--env` was not applied: {:?}",
		placed.stdout
	);
	Ok(())
}

#[tokio::test]
async fn exec_carries_the_local_stdin_to_the_command() -> Result<()> {
	let (client, server) = pair("cli_exec_stdin").await?;
	let target = server.target();

	// A command that ends only when its input ends, so one run covers both directions: the
	// bytes this side pipes in, and the end of them reaching the server.
	let copied = client
		.run_with_stdin(&exec_args(&target, COPY_STDIN_LINE), b"piped-into-stdin\n")
		.await?;
	copied.expect_success()?;
	ensure!(
		copied.stdout.contains("piped-into-stdin"),
		"exec did not carry the local standard input to the command: {:?}",
		copied.stdout
	);

	Ok(())
}

/// Ctrl-C on this side ends the command on the server, and the code it died with comes back
/// as the code this side exits with.
///
/// Unix only: interrupting one specific process is what a terminal does on Unix, and a
/// Windows test has no equivalent that works.
#[cfg(unix)]
#[tokio::test]
#[expect(
	unused_results,
	reason = "the builder methods hand back the command they set up"
)]
async fn exec_ends_a_running_command_when_this_side_is_interrupted() -> Result<()> {
	use std::process::Stdio;

	let (client, server) = pair("cli_exec_interrupt").await?;
	let target = server.target();

	// `sleep 60` never ends inside a test, so the only way this command finishes is the
	// interrupt reaching it on the server. `echo started` is the proof that the command is
	// running and that this side is already forwarding its output, which is when the listener
	// for the interrupt is installed. `exec` hands the wait on to `sleep` so that the process
	// the server ends is the one holding the output pipes, not a shell that leaves it behind.
	let mut command = client.command();
	let args = exec_args(&target, "echo started; exec sleep 60");
	command.args(&args);
	command.stdin(Stdio::null());
	command.stdout(Stdio::piped());
	command.stderr(Stdio::piped());
	let mut running = command.spawn()?;

	let mut stdout = BufReader::new(
		running
			.stdout
			.take()
			.ok_or_else(|| eyre!("the command was started without a stdout pipe"))?,
	);
	let started = read_line(&mut stdout).await?;
	ensure!(
		started.contains("started"),
		"the remote command never reported itself: {started:?}"
	);

	let pid = running
		.id()
		.ok_or_else(|| eyre!("the CLI had no process id to interrupt"))?;
	let interrupted = tokio::process::Command::new("/bin/kill")
		.arg("-INT")
		.arg(pid.to_string())
		.output()
		.await?;
	ensure!(
		interrupted.status.success(),
		"`/bin/kill -INT {pid}` failed: {}",
		String::from_utf8_lossy(&interrupted.stderr)
	);

	let outcome = running.wait_with_output().await?;
	let code = outcome.status.code();
	ensure!(
		code == Some(128 + i32::from(pirohxy::protocol::exec::SIGKILL)),
		"an interrupted remote command made the CLI exit with {code:?}, not 128 + SIGKILL\n--- stderr ---\n{}",
		String::from_utf8_lossy(&outcome.stderr)
	);

	Ok(())
}

#[tokio::test]
async fn file_uploads_downloads_and_lists_files() -> Result<()> {
	let (client, server) = pair("cli_file").await?;
	let target = server.target();

	let local = client.dir().join("upload.txt");
	fs::write(&local, UPLOAD_PAYLOAD)?;
	let local = local.to_string_lossy().into_owned();

	client
		.run(&["file", &target, "mkdir", "work"])
		.await?
		.expect_success()?;
	client
		.run(&["file", &target, "put", &local, "work/uploaded.txt"])
		.await?
		.expect_success()?;

	let listed = client.run(&["file", &target, "ls", "work"]).await?;
	listed.expect_success()?;
	ensure!(
		listed.stdout.contains("uploaded.txt"),
		"`ls work` printed {:?}",
		listed.stdout
	);

	let stated = client
		.run(&["file", &target, "stat", "work/uploaded.txt"])
		.await?;
	stated.expect_success()?;
	ensure!(
		stated.stdout.contains("uploaded.txt")
			&& stated.stdout.contains(&UPLOAD_PAYLOAD.len().to_string()),
		"`stat` printed {:?}",
		stated.stdout
	);

	let downloaded = client
		.run(&["file", &target, "get", "work/uploaded.txt", "-"])
		.await?;
	downloaded.expect_success()?;
	ensure!(
		downloaded.stdout.as_bytes() == UPLOAD_PAYLOAD,
		"`get` returned {:?}",
		downloaded.stdout
	);

	client
		.run(&[
			"file",
			&target,
			"put",
			&local,
			"work/uploaded.txt",
			"--append",
		])
		.await?
		.expect_success()?;
	let appended = client
		.run(&["file", &target, "get", "work/uploaded.txt", "-"])
		.await?;
	appended.expect_success()?;
	ensure!(
		appended.stdout.as_bytes() == [UPLOAD_PAYLOAD, UPLOAD_PAYLOAD].concat(),
		"`put --append` produced {:?}",
		appended.stdout
	);

	client
		.run(&["file", &target, "mv", "work/uploaded.txt", "work/moved.txt"])
		.await?
		.expect_success()?;
	let moved = client.run(&["file", &target, "ls", "work"]).await?;
	moved.expect_success()?;
	ensure!(
		moved.stdout.contains("moved.txt") && !moved.stdout.contains("uploaded.txt"),
		"after `mv`, `ls work` printed {:?}",
		moved.stdout
	);

	client
		.run(&["file", &target, "rm", "work/moved.txt"])
		.await?
		.expect_success()?;
	let emptied = client.run(&["file", &target, "ls", "work"]).await?;
	emptied.expect_success()?;
	ensure!(
		emptied.stdout.trim().is_empty(),
		"after `rm`, `ls work` still printed {:?}",
		emptied.stdout
	);

	Ok(())
}

#[tokio::test]
async fn shell_runs_a_command_in_a_pty() -> Result<()> {
	let (client, server) = pair("cli_shell").await?;
	let target = server.target();

	let echoed = client
		.run(&shell_cmd_args(&target, "echo hello-from-pty"))
		.await?;
	echoed.expect_success()?;
	ensure!(
		echoed.stdout.contains("hello-from-pty"),
		"the session printed {:?}",
		echoed.stdout
	);

	let failed = client.run(&shell_cmd_args(&target, "exit 3")).await?;
	failed.expect_failure()?;
	ensure!(
		failed.code == Some(3),
		"a session that exited 3 was mirrored as {:?}",
		failed.code
	);

	let cwd = server.node().dir().join("ptycwd");
	fs::create_dir_all(&cwd)?;
	let placed = client
		.run(&[
			"shell",
			&target,
			"--cwd",
			&cwd.to_string_lossy(),
			"--env",
			"GREETING=hello-from-shell",
			"--cmd",
			PRINT_ENV_LINE,
		])
		.await?;
	placed.expect_success()?;
	ensure!(
		placed.stdout.contains("ptycwd"),
		"`--cwd` was not applied to the session: {:?}",
		placed.stdout
	);
	ensure!(
		placed.stdout.contains("hello-from-shell"),
		"`--env` was not applied to the session: {:?}",
		placed.stdout
	);

	Ok(())
}

#[tokio::test]
async fn forward_serves_a_local_port() -> Result<()> {
	let (client, server) = pair("cli_forward").await?;
	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let server_target = server.target();

	let (mut task, bind) = start_bound(&client, |port| {
		vec![
			"forward".to_owned(),
			server_target.clone(),
			target.addr().to_string(),
			"--bind".to_owned(),
			format!("127.0.0.1:{port}"),
		]
	})
	.await?;

	let socket = TcpStream::connect(("127.0.0.1", bind)).await?;
	let mut reader = BufReader::new(socket);
	let line = read_line(&mut reader).await?;
	ensure!(
		line.trim() == ECHO_LINE,
		"the forward carried {:?} instead of the target's line\n--- forward log ---\n{}",
		line,
		task.log_text()
	);

	task.stop().await?;
	Ok(())
}

#[tokio::test]
async fn r_forward_opens_a_port_on_the_server() -> Result<()> {
	let (client, server) = pair("cli_rforward").await?;
	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let server_target = server.target();

	let (mut task, bind) = start_bound(&client, |port| {
		vec![
			"r-forward".to_owned(),
			server_target.clone(),
			format!("127.0.0.1:{port}"),
			target.addr().to_string(),
		]
	})
	.await?;

	let socket = TcpStream::connect(("127.0.0.1", bind)).await?;
	let mut reader = BufReader::new(socket);
	let line = read_line(&mut reader).await?;
	ensure!(
		line.trim() == ECHO_LINE,
		"the reverse forward carried {:?} instead of the target's line\n--- r_forward log ---\n{}",
		line,
		task.log_text()
	);

	task.stop().await?;
	Ok(())
}

/// Runs a SOCKS5 greeting and a CONNECT for `target` through the proxy on `bind`, and
/// returns the first line the proxy relayed back.
async fn socks5_relay(bind: u16, target: SocketAddr) -> Result<String> {
	let mut socket = TcpStream::connect(("127.0.0.1", bind)).await?;

	socket.write_all(&[5, 1, 0]).await?;
	socket.flush().await?;
	let mut greeting = [0u8; 2];
	let _greeting_read = socket.read_exact(&mut greeting).await?;
	ensure!(
		greeting == [5, 0],
		"the proxy answered the greeting with {greeting:?}"
	);

	let SocketAddr::V4(v4) = target else {
		return Err(eyre!("the echo target is not an IPv4 loopback address"));
	};
	let mut request = vec![5, 1, 0, 1];
	request.extend(v4.ip().octets());
	request.extend(v4.port().to_be_bytes());
	socket.write_all(&request).await?;
	socket.flush().await?;

	let mut reply = [0u8; 10];
	let _reply_read = socket.read_exact(&mut reply).await?;
	ensure!(
		reply[0] == 5 && reply[1] == 0,
		"the proxy refused the CONNECT with {:?}",
		&reply[..2]
	);

	let mut reader = BufReader::new(socket);
	read_line(&mut reader).await
}

/// Retries [`socks5_relay`] until the proxy carries a connection.
///
/// The SOCKS5 server binds its local port before the connection to the egress node is up,
/// so the first CONNECT can arrive too early. The retry stops the moment the proxy process
/// is gone: a command that has exited is not going to carry anything, and waiting for it is
/// how a fixture failure turns into a timeout.
async fn socks5_relay_until_ready(
	task: &mut Task,
	bind: u16,
	target: SocketAddr,
) -> Result<String> {
	let deadline = tokio::time::Instant::now() + ready_timeout();
	loop {
		match socks5_relay(bind, target).await {
			Ok(line) => return Ok(line),
			Err(err) => {
				task.check_alive()?;
				if tokio::time::Instant::now() > deadline {
					return Err(err).wrap_err("the SOCKS5 proxy never carried a connection");
				}
				tokio::time::sleep(Duration::from_millis(500)).await;
			}
		}
	}
}

#[tokio::test]
async fn socks5_proxies_a_connection() -> Result<()> {
	let (client, server) = pair("cli_socks5").await?;
	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let server_target = server.target();

	let (mut task, bind) = start_bound(&client, |port| {
		vec![
			"socks5".to_owned(),
			server_target.clone(),
			"--bind".to_owned(),
			format!("127.0.0.1:{port}"),
		]
	})
	.await?;

	let line = socks5_relay_until_ready(&mut task, bind, target.addr()).await?;
	ensure!(
		line.trim() == ECHO_LINE,
		"the proxy relayed {:?} instead of the target's line\n--- socks5 log ---\n{}",
		line,
		task.log_text()
	);

	task.stop().await?;
	Ok(())
}

#[tokio::test]
async fn a_socks_grant_proxies_and_grants_nothing_else() -> Result<()> {
	let client = Node::init("cli_socks_grant_client").await?;
	let server = Node::init("cli_socks_grant_server").await?;

	// A grant is a file named after the protocol inside the peer's directory. Nothing
	// else follows from it.
	server.grant(client.id(), Protocol::Socks.name())?;
	let server = Server::spawn(server)?;

	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let server_target = server.target();
	let (mut task, bind) = start_bound(&client, |port| {
		vec![
			"socks5".to_owned(),
			server_target.clone(),
			"--bind".to_owned(),
			format!("127.0.0.1:{port}"),
		]
	})
	.await?;

	let line = socks5_relay_until_ready(&mut task, bind, target.addr()).await?;
	ensure!(
		line.trim() == ECHO_LINE,
		"the proxy relayed {:?} instead of the target's line\n--- socks5 log ---\n{}",
		line,
		task.log_text()
	);

	client
		.run(&["file", &server.target(), "ls"])
		.await?
		.expect_failure()?;
	client
		.run(&exec_args(&server.target(), "echo should-not-run"))
		.await?
		.expect_failure()?;

	task.stop().await?;
	Ok(())
}

#[tokio::test]
async fn grants_are_read_from_the_auth_directory_on_every_connection() -> Result<()> {
	let client = Node::init("cli_grants_client").await?;
	let server = Node::init("cli_grants_server").await?;
	server.grant(client.id(), Protocol::Exec.name())?;
	let server = Server::start(server, &client).await?;

	client
		.run(&exec_args(&server.target(), "echo hello-exec"))
		.await?
		.expect_success()?;
	client
		.run(&["file", &server.target(), "ls"])
		.await?
		.expect_failure()?;
	client
		.run(&shell_cmd_args(&server.target(), "echo should-not-run"))
		.await?
		.expect_failure()?;

	// Adding a grant while the server runs takes effect on the next connection.
	server.node().grant(client.id(), Protocol::Shell.name())?;
	client
		.run(&shell_cmd_args(&server.target(), "echo hello-from-pty"))
		.await?
		.expect_success()?;

	// So does taking one away.
	server.node().revoke(client.id())?;
	client
		.run(&exec_args(&server.target(), "echo should-not-run"))
		.await?
		.expect_failure()?;

	Ok(())
}

#[tokio::test]
async fn a_name_resolves_to_the_server_it_was_written_for() -> Result<()> {
	let (client, server) = pair("cli_names").await?;
	fs::write(
		client.dir().join("names").join("prod_1"),
		format!("{}\n", server.node().id()),
	)?;

	let resolved = client.run(&["conf", "identity", "prod_1"]).await?;
	resolved.expect_success()?;
	ensure!(
		resolved.stdout.trim() == server.node().id(),
		"`conf identity prod_1` printed {:?}",
		resolved.stdout.trim()
	);

	client
		.run(&exec_args("prod_1", "echo hello-named"))
		.await?
		.expect_success()?;

	Ok(())
}
