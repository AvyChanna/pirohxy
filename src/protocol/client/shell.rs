use color_eyre::eyre::Result;
use iroh::endpoint::{Connection, RecvStream, SendStream};

use crate::{
	protocol::shell::{ShellInput, ShellOutput, ShellRequest},
	protoutils::stream::{ReadStructExt, WriteStructExt},
};

/// An interactive session on the peer's pseudo terminal.
#[derive(Debug)]
pub struct Session {
	send: SendStream,
	recv: RecvStream,
}

/// Starts a shell session on the peer.
///
/// # Errors
/// The stream cannot be opened, or the request frame cannot be written.
pub async fn open(conn: &Connection, request: &ShellRequest) -> Result<Session> {
	let (mut send, recv) = conn.open_bi().await?;
	send.write_struct(request).await?;
	Ok(Session { send, recv })
}

impl Session {
	/// Writes keystrokes to the session.
	///
	/// # Errors
	/// The peer closed the stream.
	pub async fn write_keystrokes(&mut self, data: &[u8]) -> Result<()> {
		self.send
			.write_struct(&ShellInput::Data(data.to_vec()))
			.await
	}

	/// Tells the peer the local terminal changed size.
	///
	/// # Errors
	/// The peer closed the stream.
	pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
		self.send
			.write_struct(&ShellInput::Resize { cols, rows })
			.await
	}

	/// Asks the peer to signal the session; the value is a POSIX signal number.
	///
	/// # Errors
	/// The peer closed the stream.
	pub async fn signal(&mut self, signal: u8) -> Result<()> {
		self.send.write_struct(&ShellInput::Signal(signal)).await
	}

	/// Reads the next output frame, `None` once the peer ends the stream.
	pub async fn next_output(&mut self) -> Option<ShellOutput> {
		self.recv.read_struct::<ShellOutput>().await.ok()
	}

	/// Ends the session: asks the peer to terminate the shell and closes the stream.
	///
	/// # Errors
	/// The send stream cannot be finished.
	pub async fn close(mut self) -> Result<()> {
		drop(self.send.write_struct(&ShellInput::Close).await);
		Ok(self.send.finish()?)
	}
}
