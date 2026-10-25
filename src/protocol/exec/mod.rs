use core::fmt::Debug;
use std::{
	ffi::OsString,
	process::{ExitStatus, Stdio},
};

use color_eyre::eyre::{Result, eyre};
use iroh::{
	endpoint::{Connection, RecvStream, SendStream},
	protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::{
	io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
	process::{ChildStdin, Command},
	sync::mpsc::Sender,
};
use tracing::{debug, trace};

use crate::protoutils::{
	accept::serve_bi_streams, interproto::spawn_interactive, stream::ReadStructExt, tasks::Tasks,
};

/// Size of the chunks stdout and stderr are streamed in.
///
/// Far below [`MAX_STRUCT_FRAME_LEN`], which is what the frame cap allows; the assertion
/// below keeps the two from drifting apart.
///
/// [`MAX_STRUCT_FRAME_LEN`]: crate::protoutils::stream::MAX_STRUCT_FRAME_LEN
const EXEC_CHUNK_LEN: usize = 8192;

const _: () = assert!(EXEC_CHUNK_LEN < crate::protoutils::stream::MAX_STRUCT_FRAME_LEN);

/// What a shell reports for a command that a signal ended: this base plus the signal number.
///
/// Only Unix can tell that a signal rather than an exit ended a process, so only there is it
/// needed.
#[cfg(unix)]
const SIGNAL_EXIT_BASE: i32 = 128;
/// Reported when a process ends in a way that carries no code at all.
const UNKNOWN_EXIT_CODE: i32 = -1;

/// The signal a client asks for when it wants a command ended now: `SIGKILL`.
///
/// It is the only signal that cannot be caught, blocked or ignored, so it is the only number
/// certain to end a command on a machine this side knows nothing about.
pub const SIGKILL: u8 = 9;

/// Messages sent from the server to the client running a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecOutput {
	/// A chunk of the command's standard output.
	Stdout(Vec<u8>),
	/// A chunk of the command's standard error.
	Stderr(Vec<u8>),
	/// The command finished with this exit code.
	Code(i32),
	/// The command could not be started at all.
	RunError(String),
}

/// Messages sent from the client to the server running a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecInput {
	/// A chunk to write to the command's standard input.
	Stdin(Vec<u8>),
	/// End the running command, whatever the number says.
	///
	/// The number is the POSIX signal the client meant, and clients use `SIGKILL` (9): a
	/// gentler signal can be caught, blocked or ignored by the command, which then keeps
	/// running with nobody left waiting for it. `SIGKILL` cannot be caught, and it is what
	/// the server uses on every platform.
	Signal(u8),
	/// The client has no more input; close the command's standard input.
	Close,
}

/// The command the server should run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCommand {
	/// Program to run.
	pub cmd: OsString,
	/// Arguments passed to the program.
	pub args: Vec<OsString>,
	/// Standard input written before any interactive input. When present, standard
	/// input is closed right after it so non-interactive commands can finish.
	pub stdin: Option<Vec<u8>>,
	/// Environment the command runs with. The parent environment is cleared first.
	pub env: Vec<(OsString, OsString)>,
	/// Working directory on the server.
	pub cwd: Option<String>,
}

/// A Exec protocol handler for Iroh that implements the `ProtocolHandler` trait.
/// It handles incoming connections and execs those commands.
#[derive(Debug, Copy, Clone, Default)]
pub struct Exec;

impl Exec {
	/// Creates a new `Exec` instance.
	#[must_use]
	pub fn new() -> Self {
		Self {}
	}
}

impl ProtocolHandler for Exec {
	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		serve_bi_streams(&conn, "Exec", handle_exec).await
	}
}

/// Builds the command the client asked for.
///
/// The environment is the one that came with the request and never the server's, and all
/// three standard streams are piped so the session below can feed and read them.
///
/// `PATH` is the one variable the server supplies. The program named in the request is found
/// by the server's own search, but the command then runs in an environment with no path in it
/// at all, so anything it looks for by name - a script's second program, a pipeline, a tool
/// invoking another tool - would fail to be found. A request that carries its own `PATH` keeps
/// it.
#[expect(
	unused_results,
	reason = "Setting up the command with potential side effects, but we handle errors on spawn"
)]
fn command_for(cmd_vals: &ExecCommand) -> Command {
	let mut cmd = Command::new(&cmd_vals.cmd);
	cmd.args(&cmd_vals.args).env_clear();

	if !cmd_vals.env.iter().any(|(key, _)| key == "PATH")
		&& let Some(path) = std::env::var_os("PATH")
	{
		cmd.env("PATH", path);
	}

	cmd.envs(cmd_vals.env.iter().cloned())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.stdin(Stdio::piped());

	if let Some(cwd) = &cmd_vals.cwd {
		cmd.current_dir(cwd);
	}
	cmd
}

#[expect(
	let_underscore_drop,
	reason = "Feeding stdin and reporting failures is best-effort: the process or stream may already be gone"
)]
async fn handle_exec(mut r: RecvStream, s: SendStream) -> Result<()> {
	// Read the command first
	let cmd_vals: ExecCommand = r.read_struct().await?;

	// Set up the interactive channel before spawning, so a spawn failure can be
	// reported back to the client instead of just dropping the stream.
	let mut wire = spawn_interactive::<ExecInput, ExecOutput>(r, s).await;

	let mut child = match command_for(&cmd_vals).spawn() {
		Ok(child) => child,
		Err(err) => {
			let message = format!("cannot run {}: {err}", cmd_vals.cmd.display());
			debug!("{message}");
			let _ = wire.send(ExecOutput::RunError(message)).await;
			wire.close().await;
			return Ok(());
		}
	};

	// The write handle the output pumps share. The send stream is only finished once the
	// last sender is gone, so every clone of it has to be dropped before `wire` is closed.
	let tx_output = wire.sender();

	let stdout = child
		.stdout
		.take()
		.ok_or_else(|| eyre!("spawned command has no stdout pipe"))?;
	let stderr = child
		.stderr
		.take()
		.ok_or_else(|| eyre!("spawned command has no stderr pipe"))?;
	// The handle is optional because closing standard input means dropping it:
	// `shutdown` alone only flushes, so a command blocked on stdin would never see the
	// end of the stream.
	let mut stdin = Some(
		child
			.stdin
			.take()
			.ok_or_else(|| eyre!("spawned command has no stdin pipe"))?,
	);

	let mut pumps = Tasks::new("exec output");
	// Boxed: each pump holds a read buffer, and the task that carries them should not hold
	// it on its own frame.
	pumps
		.spawn(Box::pin(pump_output(
			stdout,
			tx_output.clone(),
			ExecOutput::Stdout,
		)))
		.await;
	pumps
		.spawn(Box::pin(pump_output(
			stderr,
			tx_output.clone(),
			ExecOutput::Stderr,
		)))
		.await;

	// Non-interactive input is supplied up front and closes stdin, so commands that
	// block reading stdin still terminate.
	if let Some(data) = &cmd_vals.stdin {
		if let Some(stdin) = stdin.as_mut() {
			if stdin.write_all(data).await.is_err() {
				debug!("Failed to write the initial stdin for {:?}", cmd_vals.cmd);
			}
			let _ = stdin.shutdown().await;
		}
		drop(stdin.take());
	}

	// The channel is read until it ends, not until stdin ends: a client that has run out of
	// input can still ask for the command to be interrupted.
	let mut channel_open = true;
	let status = loop {
		let input = tokio::select! {
			biased;
			res = child.wait() => break res?,
			msg = wire.recv(), if channel_open => msg,
		};

		// The select is over, so the future that borrowed the child is gone and the child can
		// be written to and signalled from here.
		match input {
			Some(ExecInput::Stdin(data)) => {
				let mut failed = stdin.is_none();
				if let Some(stdin) = stdin.as_mut() {
					failed = stdin.write_all(&data).await.is_err();
				}
				if failed {
					debug!("Failed to write stdin for {:?}", cmd_vals.cmd);
				}
			}
			Some(ExecInput::Signal(signal)) => {
				// `kill` is `SIGKILL` on Unix and `TerminateProcess` on Windows, which is the
				// point: the command cannot catch it, so an interrupted command really ends.
				debug!("Signal {signal} for {:?}: ending the command", cmd_vals.cmd);
				if let Err(err) = child.kill().await {
					debug!("Failed to end {:?}: {err}", cmd_vals.cmd);
				}
			}
			Some(ExecInput::Close) => close_stdin(&mut stdin).await,
			None => {
				channel_open = false;
				close_stdin(&mut stdin).await;
			}
		}
	};

	// The command is gone: release our handle on its stdin, then let the pumps hand over
	// whatever it managed to write before the exit code goes out.
	drop(stdin);
	pumps.drain().await;
	// Every clone of the output sender has to be gone before the session is closed, or the
	// send stream is never finished.
	drop(tx_output);

	let exit_code = exit_code_for(status);
	trace!("{:?} exited with code {exit_code}", cmd_vals.cmd);
	let _ = wire.send(ExecOutput::Code(exit_code)).await;
	// Ends the session for good: the queued output is written, the stream is finished
	// and both pumps are joined instead of left running.
	wire.close().await;

	Ok(())
}

/// Closes the command's standard input, which is how a command that reads to EOF - `cat`, a
/// filter - learns that nothing more is coming. Dropping the handle is what ends the pipe;
/// `shutdown` on its own only flushes it.
async fn close_stdin(stdin: &mut Option<ChildStdin>) {
	if let Some(mut stdin) = stdin.take() {
		let _ignored = stdin.shutdown().await;
	}
}

/// The code to report for a command that has finished.
///
/// A command that a signal ended has no exit code of its own, so it gets `128 + signal`,
/// which is what every shell reports for it: an interrupted command arrives as a code naming
/// the signal that ended it rather than as a failure with no reason attached.
fn exit_code_for(status: ExitStatus) -> i32 {
	if let Some(code) = status.code() {
		return code;
	}

	#[cfg(unix)]
	{
		use std::os::unix::process::ExitStatusExt;
		if let Some(signal) = status.signal() {
			return SIGNAL_EXIT_BASE + signal;
		}
	}

	UNKNOWN_EXIT_CODE
}

/// Reads `reader` to end and forwards each chunk to the client as `make(chunk)`.
async fn pump_output<R>(mut reader: R, tx: Sender<ExecOutput>, make: fn(Vec<u8>) -> ExecOutput)
where
	R: AsyncRead + Unpin,
{
	let mut buffer = [0u8; EXEC_CHUNK_LEN];
	loop {
		match reader.read(&mut buffer).await {
			Ok(0) | Err(_) => break,
			Ok(n) => {
				if tx.send(make(buffer[..n].to_vec())).await.is_err() {
					break;
				}
			}
		}
	}
}

#[cfg(test)]
mod ut_tests;

#[cfg(test)]
mod it_tests;
