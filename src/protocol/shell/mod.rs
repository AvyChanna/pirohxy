use alloc::sync::Arc;
use core::fmt::Debug;
use core::time::Duration;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use color_eyre::eyre::{Result, eyre};
use iroh::{
	endpoint::{Connection, RecvStream, SendStream},
	protocol::{AcceptError, ProtocolHandler},
};
use portable_pty::{
	Child, ChildKiller, CommandBuilder, MasterPty, PtySize, SlavePty, native_pty_system,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::protoutils::{
	accept::serve_bi_streams, interproto::spawn_interactive, stream::ReadStructExt, tasks::Tasks,
};

/// How many bytes are read from the pty at a time.
///
/// Far below [`MAX_STRUCT_FRAME_LEN`], which is what the frame cap allows; the assertion
/// below keeps the two from drifting apart.
///
/// [`MAX_STRUCT_FRAME_LEN`]: crate::protoutils::stream::MAX_STRUCT_FRAME_LEN
const PTY_READ_CHUNK: usize = 8192;

const _: () = assert!(PTY_READ_CHUNK < crate::protoutils::stream::MAX_STRUCT_FRAME_LEN);
/// Fallback terminal width when the client does not supply one.
const DEFAULT_COLS: u16 = 80;
/// Fallback terminal height when the client does not supply one.
const DEFAULT_ROWS: u16 = 24;
/// `ConPTY` opens a session by asking the terminal where its cursor is and stalls
/// every later write until that question is answered.
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
/// The reply written back to the pty so that the shell can start printing.
const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

/// Describes the session the server should start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellRequest {
	/// Command line to run inside the pty. Defaults to the server's login shell.
	pub cmd: Option<String>,
	/// Value for the `TERM` variable.
	pub term: String,
	/// Initial terminal width.
	pub cols: u16,
	/// Initial terminal height.
	pub rows: u16,
	/// Working directory on the server.
	pub cwd: Option<String>,
	/// Extra variables merged into the inherited environment.
	pub env: Vec<(String, String)>,
}

/// Messages sent from the client to the server hosting the pty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellInput {
	/// Keystrokes to write to the pty.
	Data(Vec<u8>),
	/// The client's terminal changed size.
	Resize {
		/// New terminal width.
		cols: u16,
		/// New terminal height.
		rows: u16,
	},
	/// Ask the server to terminate the session. The value is a POSIX signal number;
	/// servers that cannot deliver a specific signal fall back to killing the process.
	///
	/// This is not how Ctrl-C reaches a shell: an interrupt is the `0x03` byte written to the
	/// pty as [`ShellInput::Data`], which the server's line discipline turns into `SIGINT` for
	/// the program running there. `Signal` is the out-of-band request to end the session, used
	/// where a keystroke would not do - `exec`, for instance, sends `SIGKILL` here.
	Signal(u8),
	/// The client is done and the session should be closed.
	Close,
}

/// Messages sent from the server to the client hosting the pty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellOutput {
	/// A chunk of pty output.
	Data(Vec<u8>),
	/// The session ended with this exit code.
	Exit {
		/// Exit code reported by the child process.
		code: u32,
	},
	/// The session could not be started.
	Error(String),
}

/// A `Shell` protocol handler for Iroh that implements the `ProtocolHandler` trait.
/// It runs an interactive shell for the client on a pseudo terminal.
#[derive(Debug, Clone, Default)]
pub struct Shell {
	shell: Option<String>,
}

impl Shell {
	/// Creates a new `Shell` instance using the server's default shell.
	#[must_use]
	pub fn new() -> Self {
		Self { shell: None }
	}

	/// Overrides the shell used when the client does not request a command.
	#[must_use]
	pub fn with_shell(mut self, shell: Option<String>) -> Self {
		self.shell = shell;
		self
	}
}

impl ProtocolHandler for Shell {
	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		let default_shell = self.shell.clone();
		serve_bi_streams(&conn, "Shell", move |r, s| {
			handle_shell(r, s, default_shell.clone())
		})
		.await
	}
}

#[expect(
	let_underscore_drop,
	reason = "Relaying the session outcome is best-effort: the peer may already be gone"
)]
async fn handle_shell(
	mut r: RecvStream,
	s: SendStream,
	default_shell: Option<String>,
) -> Result<()> {
	let mut request: ShellRequest = r.read_struct().await?;
	if request.cols == 0 {
		request.cols = DEFAULT_COLS;
	}
	if request.rows == 0 {
		request.rows = DEFAULT_ROWS;
	}
	trace!(
		"Shell request (term={}, {}x{}, cmd={:?})",
		request.term, request.cols, request.rows, request.cmd
	);

	let mut wire = spawn_interactive::<ShellInput, ShellOutput>(r, s).await;

	// The pty is driven on blocking threads, so output comes back over an unbounded
	// channel that any thread may push to.
	let (tx_output_from_pty, mut rx_output_from_pty) = unbounded_channel::<ShellOutput>();
	// Keystrokes for the shell. This channel has exactly one sender and it is held here,
	// which is what makes closing it a signal the blocking session can always notice -
	// even when this future is cancelled before it gets as far as sending `Close`.
	let (tx_input, rx_input) = channel::<ShellInput>();
	// The code the shell exited with, reported by the thread that waits on the child.
	// Only the teardown reads it, so a shell that will not die cannot hold a session open.
	let (tx_exit, rx_exit) = channel::<u32>();
	// Cancelled by that same thread. A shell that exits on its own has to end the session
	// without waiting to be asked, and this is the one event that says so.
	let shell_gone = CancellationToken::new();

	let session_shell_gone = shell_gone.clone();
	let session = tokio::task::spawn_blocking(move || {
		run_pty_session(
			&request,
			default_shell.as_deref(),
			&rx_input,
			tx_exit,
			&rx_exit,
			session_shell_gone,
			&tx_output_from_pty,
		)
	});

	// When the pty output ends, the session is over and the input loop should stop too.
	let session_done = CancellationToken::new();
	let relay_token = session_done.clone();

	let tx_output = wire.sender();
	let mut relay = Tasks::new("shell relay");
	relay
		.spawn(async move {
			while let Some(output) = rx_output_from_pty.recv().await {
				if tx_output.send(output).await.is_err() {
					break;
				}
			}
			relay_token.cancel();
		})
		.await;

	let end = session_done.cancelled();
	tokio::pin!(end);

	loop {
		let shell_gone = shell_gone.cancelled();
		tokio::select! {
			biased;
			() = &mut end => break,
			() = shell_gone => break,
			input = wire.recv() => {
				let is_close = matches!(input, Some(ShellInput::Close));
				match input {
					Some(input) => {
						if tx_input.send(input).is_err() {
							break;
						}
						if is_close {
							break;
						}
					}
					None => break,
				}
			}
		}
	}

	debug!("Shell input loop ended, closing the session");
	// Say plainly that the session is over, then let go of the only sender. The message
	// ends a session that is waiting on the next keystroke; closing the channel is what
	// ends one this future never reached, because the watcher that reports a dead shell
	// has its own channel now and cannot hold this one open.
	let _ = tx_input.send(ShellInput::Close);
	drop(tx_input);
	let _ = session.await;
	debug!("Shell session task returned");
	// Drained rather than aborted: the exit code is the last frame the session queued, and
	// aborting here can drop it before it ever reaches the wire. Letting the relay finish
	// on its own also releases its clone of the write handle, which `wire.close` needs
	// before it can finish the send stream.
	relay.drain().await;
	// Ends the interactive session: the queued output is written, the stream is finished
	// and both pumps are joined instead of left running.
	wire.close().await;

	Ok(())
}

/// The shell process, as the pty spawned it.
type ShellProcess = Box<dyn Child + Send + Sync>;

/// A reader for everything the terminal prints.
type TerminalReader = Box<dyn Read + Send>;

/// A thread the session spawns, and may have to wait for.
type SessionThread = std::thread::JoinHandle<()>;

/// The code reported when the child never reports one itself.
const FALLBACK_EXIT_CODE: u32 = 255;

/// How long a killed shell gets to report its code before the session stops waiting.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// How long the terminal is kept open after the shell is gone so the reader can pull
/// what it printed.
///
/// Closing a pty discards whatever its emulator has not translated and the reader has
/// not pulled yet, and the exit code is the last thing a client waits for: reporting it
/// first is what makes a short command look like it printed nothing.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// The pty and the handles a session needs from it.
struct PtyTerminal {
	/// Held open so the terminal outlives the spawn; the child has its own handle.
	///
	/// Listed first because closing it is what releases a blocked master read.
	_slave: Box<dyn SlavePty + Send>,
	/// The terminal itself, kept for resizing it.
	master: Box<dyn MasterPty + Send>,
	/// Ends the child without waiting for it.
	killer: Box<dyn ChildKiller + Send + Sync>,
	/// Shared with the reader thread, which answers `ConPTY`'s cursor query through it.
	writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

/// Runs a shell on a pseudo terminal until the client closes the session or the shell
/// exits.
///
/// Reading the pty blocks, and so does waiting for the child, so both live on their own
/// threads. The feed loop in between sleeps in `recv()` on `rx_input`, whose only sender is
/// `handle_shell`; the watcher reports the code on `tx_exit` and cancels `shell_gone` once
/// the shell is gone, which is what ends a session nobody asked to close.
#[expect(
	let_underscore_drop,
	reason = "reporting the exit code is best-effort during teardown"
)]
fn run_pty_session(
	request: &ShellRequest,
	default_shell: Option<&str>,
	rx_input: &Receiver<ShellInput>,
	tx_exit: Sender<u32>,
	rx_exit: &Receiver<u32>,
	shell_gone: CancellationToken,
	tx_output: &UnboundedSender<ShellOutput>,
) -> Result<()> {
	let (terminal, child, reader) = open_pty(request, default_shell)?;
	let PtyTerminal {
		_slave: slave,
		master,
		mut killer,
		writer,
	} = terminal;

	let reader_thread = spawn_reader(reader, tx_output, Arc::clone(&writer));
	let _watcher_thread = spawn_watcher(child, tx_exit, shell_gone);

	feed_loop(rx_input, &*master, &writer);
	// Dropping the writer sends EOF to the slave side. The reader thread keeps its own
	// clone, which is what answers `ConPTY`'s cursor query.
	drop(writer);
	let code = reap(&mut *killer, rx_exit);

	// The reader pulls the terminal on its own thread, and closing the terminal discards
	// whatever it has not pulled yet, so the shell gets a moment to be read out before
	// its exit code is reported.
	std::thread::sleep(DRAIN_GRACE);

	// Close the slave before the master: the reader is blocked on a master read, and
	// that read only returns once nothing holds the slave open any more.
	drop(slave);
	drop(master);
	await_reader(reader_thread);

	trace!("Shell exited with code {code}");
	let _ = tx_output.send(ShellOutput::Exit { code });

	Ok(())
}

/// Opens a pty and starts the shell on it.
///
/// Returns the terminal handles, the child, and a reader for everything it prints.
fn open_pty(
	request: &ShellRequest,
	default_shell: Option<&str>,
) -> Result<(PtyTerminal, ShellProcess, TerminalReader)> {
	let pair = native_pty_system()
		.openpty(PtySize {
			rows: request.rows,
			cols: request.cols,
			pixel_width: 0,
			pixel_height: 0,
		})
		.map_err(|err| eyre!("failed to open a pty: {err}"))?;

	let (program, args) = resolve_shell(request, default_shell);
	let mut cmd = CommandBuilder::new(&program);
	cmd.args(args);
	cmd.env("TERM", &request.term);
	for (key, value) in &request.env {
		cmd.env(key, value);
	}
	if let Some(cwd) = &request.cwd {
		cmd.cwd(cwd);
	}

	let child = pair
		.slave
		.spawn_command(cmd)
		.map_err(|err| eyre!("failed to start {program}: {err}"))?;
	debug!(
		"Started {program} on a {}x{} pty",
		request.cols, request.rows
	);

	let reader = pair
		.master
		.try_clone_reader()
		.map_err(|err| eyre!("failed to read the pty: {err}"))?;
	let writer = Arc::new(Mutex::new(
		pair.master
			.take_writer()
			.map_err(|err| eyre!("failed to write to the pty: {err}"))?,
	));

	Ok((
		PtyTerminal {
			master: pair.master,
			_slave: pair.slave,
			killer: child.clone_killer(),
			writer,
		},
		child,
		reader,
	))
}

/// Copies everything the terminal prints to the client until the terminal ends.
///
/// `ConPTY`'s cursor query is answered here, on the way past, because this is the thread
/// that sees it asked.
///
/// The handle it returns is what lets the session wait for the last of the output before
/// it reports an exit code.
fn spawn_reader(
	mut reader: TerminalReader,
	tx_output: &UnboundedSender<ShellOutput>,
	pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
) -> SessionThread {
	let tx_output = tx_output.clone();
	std::thread::spawn(move || {
		let mut buffer = [0u8; PTY_READ_CHUNK];
		loop {
			match reader.read(&mut buffer) {
				Ok(0) | Err(_) => break,
				Ok(n) => {
					answer_cursor_query(&buffer[..n], &pty_writer);
					if tx_output
						.send(ShellOutput::Data(buffer[..n].to_vec()))
						.is_err()
					{
						break;
					}
				}
			}
		}
	})
}

/// Waits for the reader thread to finish, bounded so a terminal that will not let go
/// cannot hang the session.
///
/// The reader is the last sender of output the client has not seen yet, so the exit code
/// is only reported once it is done.
fn await_reader(reader: SessionThread) {
	let (tx_done, rx_done) = channel::<()>();
	let _joiner = std::thread::spawn(move || {
		if reader.join().is_err() {
			trace!("The reader thread panicked");
		}
		let _ = tx_done.send(());
	});
	let _ = rx_done.recv_timeout(DRAIN_GRACE);
}

/// Waits for the shell to exit, then reports its code and says that the shell is gone.
///
/// `wait()` blocks until the child is gone, which is what replaces the 100 ms poll this
/// session used to run: the thread sleeps in the kernel. The code it sends is what [`reap`]
/// waits for, and the token it cancels is what ends a session whose shell exited without
/// being asked to - on `ConPTY` that is the only way, because the reader there stays
/// blocked until the terminal is closed.
fn spawn_watcher(
	mut child: ShellProcess,
	tx_exit: Sender<u32>,
	shell_gone: CancellationToken,
) -> SessionThread {
	std::thread::spawn(move || {
		let code = match child.wait() {
			Ok(status) => status.exit_code(),
			Err(err) => {
				debug!("Failed to wait for the shell: {err}");
				FALLBACK_EXIT_CODE
			}
		};
		let _ = tx_exit.send(code);
		shell_gone.cancel();
	})
}

/// Writes client input to the terminal until the session ends.
///
/// The channel ends when `handle_shell` lets go of it, which covers every way a session can
/// be over: the client asked to close, the client disappeared, the shell died, or the
/// handler itself was cancelled. Nothing here has to notice the shell on its own.
fn feed_loop(
	rx_input: &Receiver<ShellInput>,
	master: &(dyn MasterPty + Send),
	pty_writer: &Arc<Mutex<Box<dyn Write + Send>>>,
) {
	loop {
		let Ok(input) = rx_input.recv() else {
			debug!("Shell input channel closed");
			return;
		};
		match input {
			ShellInput::Data(data) => {
				let Ok(mut writer) = pty_writer.lock() else {
					debug!("Failed to lock the pty writer, ending session");
					return;
				};
				if writer.write_all(&data).is_err() || writer.flush().is_err() {
					debug!("Failed to write to the pty, ending session");
					return;
				}
			}
			ShellInput::Resize { cols, rows } => {
				let size = PtySize {
					rows: rows.max(1),
					cols: cols.max(1),
					pixel_width: 0,
					pixel_height: 0,
				};
				if let Err(err) = master.resize(size) {
					debug!("Failed to resize the pty to {cols}x{rows}: {err}");
				}
			}
			ShellInput::Signal(signal) => {
				// An out-of-band request to end the session. Ctrl-C does not come here - it is
				// the `0x03` byte above, which the pty turns into `SIGINT` for the program - so
				// reaching this arm means the client really wants the session gone.
				debug!("Signal {signal} requested, terminating the session");
				return;
			}
			ShellInput::Close => return,
		}
	}
}

/// Ends the shell and reports the code it exited with.
#[expect(
	let_underscore_drop,
	reason = "killing the shell is best-effort during teardown"
)]
fn reap(killer: &mut (dyn ChildKiller + Send + Sync), rx_exit: &Receiver<u32>) -> u32 {
	// A shell that exited on its own has already reported its code, and a process that is
	// gone is not one the platform should be asked to kill a second time.
	if let Ok(code) = rx_exit.try_recv() {
		return code;
	}
	debug!("Terminating the shell and waiting for its code");
	let _ = killer.kill();
	// The watcher is blocked in `wait()`; killing the child is what makes it report. A
	// deadline bounds the wait instead of turning it back into a poll.
	if let Ok(code) = rx_exit.recv_timeout(REAP_GRACE) {
		code
	} else {
		debug!("The shell did not report an exit code");
		FALLBACK_EXIT_CODE
	}
}

/// `ConPTY` starts a session by asking the terminal for its cursor position and stalls
/// every write until that question is answered, so the server has to play the part of
/// the terminal here. Without the reply the child blocks on its first console write and
/// the session looks completely silent.
fn answer_cursor_query(chunk: &[u8], writer: &Mutex<Box<dyn Write + Send>>) {
	if !chunk
		.windows(CURSOR_POSITION_QUERY.len())
		.any(|window| window == CURSOR_POSITION_QUERY)
	{
		return;
	}
	let Ok(mut writer) = writer.lock() else {
		return;
	};
	if writer.write_all(CURSOR_POSITION_REPORT).is_err() || writer.flush().is_err() {
		debug!("Failed to answer the cursor position query");
	}
}

/// Picks the program and arguments to run inside the pty.
///
/// A requested command line is always handed to a shell so that pipelines, quoting and
/// redirection behave the way they do over SSH.
fn resolve_shell(request: &ShellRequest, default_shell: Option<&str>) -> (String, Vec<String>) {
	let shell = default_shell.map_or_else(default_shell_program, ToOwned::to_owned);
	match &request.cmd {
		Some(line) => run_through_shell(&shell, line),
		None => (shell, interactive_shell_args()),
	}
}

/// The platform's preferred interactive shell.
fn default_shell_program() -> String {
	#[cfg(unix)]
	{
		std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_owned())
	}
	#[cfg(not(unix))]
	{
		std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_owned())
	}
}

/// Arguments that make the shell interactive.
fn interactive_shell_args() -> Vec<String> {
	#[cfg(unix)]
	{
		vec!["-i".to_owned()]
	}
	#[cfg(not(unix))]
	{
		Vec::new()
	}
}

/// Runs a single command line through the shell.
fn run_through_shell(shell: &str, line: &str) -> (String, Vec<String>) {
	#[cfg(unix)]
	{
		(shell.to_owned(), vec!["-c".to_owned(), line.to_owned()])
	}
	#[cfg(not(unix))]
	{
		(shell.to_owned(), vec!["/C".to_owned(), line.to_owned()])
	}
}

#[cfg(test)]
mod it_tests;

#[cfg(test)]
mod ut_tests;
