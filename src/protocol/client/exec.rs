use color_eyre::eyre::Result;
use iroh::endpoint::{Connection, RecvStream, SendStream};

use crate::{
	protocol::exec::{ExecCommand, ExecInput, ExecOutput},
	protoutils::stream::{ReadStructExt, WriteStructExt},
	shutdown::shutdown_handle,
};

/// Everything one run produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
	/// Bytes the command wrote to standard output.
	pub stdout: Vec<u8>,
	/// Bytes the command wrote to standard error.
	pub stderr: Vec<u8>,
	/// The exit code, `None` when the command never reported one.
	pub code: Option<i32>,
	/// Why the command could not be started, when the peer reported one.
	pub error: Option<String>,
}

/// A command running on the peer.
#[derive(Debug)]
pub struct Session {
	send: SendStream,
	recv: RecvStream,
}

/// Starts `command` on the peer.
///
/// # Errors
/// The stream cannot be opened, or the command frame cannot be written.
pub async fn open(conn: &Connection, command: &ExecCommand) -> Result<Session> {
	let (mut send, recv) = conn.open_bi().await?;
	send.write_struct(command).await?;
	Ok(Session { send, recv })
}

/// Runs `command` to completion and collects everything it produced.
///
/// # Errors
/// The command cannot be started on the peer, or its streams fail.
pub async fn run(conn: &Connection, command: &ExecCommand) -> Result<Outcome> {
	open(conn, command).await?.finish().await
}

/// Resolves once this process is asked to exit: Ctrl-C everywhere, SIGTERM too on Unix.
///
/// A session waiting on a command that will never finish on its own uses this to end the
/// command instead of waiting for it. It shares the one signal listener the process already
/// has, so a second Ctrl-C still ends this process without waiting for the peer.
pub async fn interrupted() {
	shutdown_handle().cancelled().await;
}

impl Session {
	/// Writes one chunk to the command's standard input.
	///
	/// # Errors
	/// The peer closed the stream.
	pub async fn write_stdin(&mut self, data: &[u8]) -> Result<()> {
		self.send
			.write_struct(&ExecInput::Stdin(data.to_vec()))
			.await
	}

	/// Asks the peer to end the command now.
	///
	/// Pass [`SIGKILL`](crate::protocol::exec::SIGKILL): it is the one signal a command
	/// cannot catch, block or ignore, and it is what the peer uses whatever number arrives.
	///
	/// # Errors
	/// The peer closed the stream.
	pub async fn signal(&mut self, signal: u8) -> Result<()> {
		self.send.write_struct(&ExecInput::Signal(signal)).await
	}

	/// Closes the command's standard input without ending the session.
	///
	/// # Errors
	/// The peer closed the stream.
	pub async fn close_stdin(&mut self) -> Result<()> {
		self.send.write_struct(&ExecInput::Close).await
	}

	/// Reads the next output frame, `None` once the peer ends the stream.
	pub async fn next_output(&mut self) -> Option<ExecOutput> {
		self.recv.read_struct::<ExecOutput>().await.ok()
	}

	/// Stops sending input, so a command waiting on stdin can finish.
	///
	/// # Errors
	/// The send stream cannot be finished.
	pub fn close_input(&mut self) -> Result<()> {
		Ok(self.send.finish()?)
	}

	/// Drains the session, returning everything the command produced.
	///
	/// # Errors
	/// The output frames cannot be read.
	pub async fn finish(mut self) -> Result<Outcome> {
		drop(self.close_stdin().await);
		self.close_input()?;

		let mut outcome = Outcome::default();
		while let Some(output) = self.next_output().await {
			match output {
				ExecOutput::Stdout(chunk) => outcome.stdout.extend_from_slice(&chunk),
				ExecOutput::Stderr(chunk) => outcome.stderr.extend_from_slice(&chunk),
				ExecOutput::Code(code) => {
					outcome.code = Some(code);
					break;
				}
				ExecOutput::RunError(reason) => {
					outcome.error = Some(reason);
					break;
				}
			}
		}
		Ok(outcome)
	}
}
