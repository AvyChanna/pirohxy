use color_eyre::eyre::{Result, ensure};
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
	io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
	task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Number of bytes used for the big-endian length prefix of a struct frame.
const STRUCT_FRAME_LEN_BYTES: usize = 4;
/// Largest struct frame this crate will read or write.
///
/// The length prefix is a `u32`, so the wire format could describe a frame of up to
/// 4 GiB. Accepting that would let a peer force an allocation of the same size, so the
/// read side is capped instead.
///
/// Bulk payloads are split into chunks far below this cap: [`FILE_CHUNK_LEN`] for file
/// transfer, `EXEC_CHUNK_LEN` (`protocol/exec.rs`) for command output and `PTY_READ_CHUNK`
/// (`protocol/shell.rs`) for the pty. Each of those modules carries a `const` assertion
/// against this value, so a future bump of either side fails at compile time rather than
/// silently breaking the invariant.
///
/// [`FILE_CHUNK_LEN`]: crate::protocol::file::FILE_CHUNK_LEN
pub const MAX_STRUCT_FRAME_LEN: usize = 1024 * 1024;

enum CopyResult {
	/// The copy ran to the end of its source and closed the write side it owned.
	Completed,
	/// The peer stopped sending, so the write side facing it was closed. The opposite
	/// direction may still have bytes to deliver, so it is left running.
	HalfClosed,
	/// The copy was torn down because the other direction or the process ended.
	Cancelled,
}

/// Writes a value as a length-prefixed postcard frame.
pub trait WriteStructExt: AsyncWrite + Unpin {
	/// Serializes `value` with postcard and writes it as one frame.
	///
	/// # Errors
	/// The value cannot be serialized, it is larger than [`MAX_STRUCT_FRAME_LEN`], or
	/// the underlying writer fails.
	fn write_struct<'a, T>(&'a mut self, value: &'a T) -> impl Future<Output = Result<()>> + 'a
	where
		T: Serialize + 'a,
	{
		write_struct_frame(self, value)
	}
}

impl<W> WriteStructExt for W where W: AsyncWrite + Unpin + ?Sized {}

/// Reads a length-prefixed postcard frame as a value.
pub trait ReadStructExt: AsyncRead + Unpin {
	/// Reads one frame and deserializes it with postcard.
	///
	/// # Errors
	/// The stream ends mid-frame, the frame is larger than [`MAX_STRUCT_FRAME_LEN`], or
	/// the payload cannot be deserialized as `T`.
	fn read_struct<'a, T>(&'a mut self) -> impl Future<Output = Result<T>> + 'a
	where
		T: DeserializeOwned + 'a,
	{
		read_struct_frame(self)
	}
}

impl<R> ReadStructExt for R where R: AsyncRead + Unpin + ?Sized {}

async fn write_struct_frame<W, T>(writer: &mut W, value: &T) -> Result<()>
where
	W: AsyncWrite + Unpin + ?Sized,
	T: Serialize,
{
	let bytes = postcard::to_allocvec(value)?;
	ensure!(
		bytes.len() <= MAX_STRUCT_FRAME_LEN,
		"postcard frame length {} exceeds maximum {}",
		bytes.len(),
		MAX_STRUCT_FRAME_LEN
	);

	let len = u32::try_from(bytes.len())?;
	writer.write_all(&len.to_be_bytes()).await?;
	writer.write_all(&bytes).await?;
	writer.flush().await?;
	Ok(())
}

async fn read_struct_frame<R, T>(reader: &mut R) -> Result<T>
where
	R: AsyncRead + Unpin + ?Sized,
	T: DeserializeOwned,
{
	let mut len_bytes = [0u8; STRUCT_FRAME_LEN_BYTES];
	let _ = reader.read_exact(&mut len_bytes).await?;

	let len = u32::from_be_bytes(len_bytes) as usize;
	ensure!(
		len <= MAX_STRUCT_FRAME_LEN,
		"postcard frame length {len} exceeds maximum {MAX_STRUCT_FRAME_LEN}"
	);

	let mut bytes = vec![0; len];
	let _ = reader.read_exact(&mut bytes).await?;
	Ok(postcard::from_bytes::<T>(&bytes)?)
}

/// Copy data between two bidi streams until one of them is closed.
///
/// `closing` is the caller's handle on the process exit signal. When it is cancelled both
/// directions are torn down, so a Ctrl-C ends a proxied connection instead of leaving it
/// copying until the sockets on either side notice.
pub async fn copy_bidi_stream(
	from1: impl AsyncRead + Send + Sync + Unpin + 'static,
	to1: impl AsyncWrite + Send + Sync + Unpin + 'static,
	from2: RecvStream,
	to2: SendStream,
	closing: &CancellationToken,
) {
	let token = CancellationToken::new();
	let token1 = token.clone();
	let token2 = token.clone();
	let mut set = JoinSet::new();
	let _forward_from_stdin = set.spawn(async move { copy_forward(from1, to2, token1).await });
	let _forward_to_stdout = set.spawn(async move { copy_backward(from2, to1, token2).await });

	let stopped = closing.cancelled();
	tokio::pin!(stopped);
	let mut stopping = false;

	while !set.is_empty() {
		tokio::select! {
			res = set.join_next(), if !set.is_empty() => {
				if let Some(res) = res {
					let half_closed = matches!(&res, Ok(Ok(CopyResult::HalfClosed)));
					match res {
						Ok(Ok(CopyResult::Completed)) => debug!("Copy task completed"),
						Ok(Ok(CopyResult::HalfClosed)) => {
							debug!("Copy task half-closed, keeping the other direction open");
						}
						Ok(Ok(CopyResult::Cancelled)) => debug!("Copy task cancelled"),
						Ok(Err(err)) => debug!("Error in task: {err}"),
						Err(err) => debug!("Error in task join: {err}"),
					}

					// Stop the opposite copy direction as soon as one finishes or fails,
					// unless it only stopped sending and can still receive.
					if !half_closed {
						token.cancel();
					}
				}
			}
			() = &mut stopped, if !stopping => {
				stopping = true;
				token.cancel();
			}
		}
	}
}

async fn copy_forward(
	mut from1: impl AsyncRead + Unpin,
	mut to2: SendStream,
	token: CancellationToken,
) -> Result<CopyResult> {
	tokio::select! {
		res = tokio::io::copy(&mut from1, &mut to2) => {
			let copied = match res {
				Ok(copied) => copied,
				Err(err) => {
					debug!("copy from the peer into iroh failed: {err}");
					return Err(err.into());
				}
			};
			debug!("copy from the peer into iroh finished after {copied} bytes");
			to2.finish()?;
			Ok(CopyResult::Completed)
		}
		() = token.cancelled() => {
			let _ = to2.reset(0u8.into()).ok();
			Ok(CopyResult::Cancelled)
		}
	}
}

async fn copy_backward(
	mut from2: RecvStream,
	mut to1: impl AsyncWrite + Unpin,
	token: CancellationToken,
) -> Result<CopyResult> {
	tokio::select! {
		res = tokio::io::copy(&mut from2, &mut to1) => {
			let copied = match res {
				Ok(copied) => copied,
				Err(err) => {
					debug!("copy from iroh into the peer failed: {err}");
					return Err(err.into());
				}
			};
			debug!("copy from iroh into the peer finished after {copied} bytes");
			// Half-close: the peer sees the end of our input but can still answer.
			to1.shutdown().await?;
			Ok(CopyResult::HalfClosed)
		}
		() = token.cancelled() => {
			let _ = from2.stop(0u8.into()).ok();
			Ok(CopyResult::Cancelled)
		}
	}
}

#[cfg(test)]
mod tests {
	use color_eyre::eyre::Result;
	use serde::{Deserialize, Serialize};
	use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

	use super::{ReadStructExt, WriteStructExt};

	#[derive(Deserialize, Serialize, Debug, PartialEq)]
	struct TestMessage {
		command: String,
		args: Vec<String>,
		exit_code: u16,
	}

	#[tokio::test]
	async fn writes_and_reads_postcard_struct_frame() -> Result<()> {
		let expected = TestMessage {
			command: "echo".to_owned(),
			args: vec!["hello".to_owned(), "world".to_owned()],
			exit_code: 0,
		};
		let (mut sender, mut receiver) = duplex(64);

		let write = sender.write_struct(&expected);
		let read = receiver.read_struct::<TestMessage>();
		let (write_result, read_result) = tokio::join!(write, read);

		write_result?;
		let actual = read_result?;
		assert_eq!(actual, expected);
		Ok(())
	}

	#[tokio::test]
	async fn writes_big_endian_length_prefix_before_postcard_payload() -> Result<()> {
		let message = TestMessage {
			command: "true".to_owned(),
			args: Vec::new(),
			exit_code: 0,
		};
		let expected_payload = postcard::to_allocvec(&message)?;
		let mut writer = Vec::new();

		writer.write_struct(&message).await?;

		let len_bytes: [u8; 4] = writer[..4].try_into()?;
		assert_eq!(
			u32::from_be_bytes(len_bytes),
			u32::try_from(expected_payload.len())?
		);
		assert_eq!(&writer[4..], expected_payload.as_slice());
		Ok(())
	}

	#[tokio::test]
	async fn read_struct_fails_on_truncated_payload() -> Result<()> {
		let (mut sender, mut receiver) = duplex(16);

		sender.write_all(&5u32.to_be_bytes()).await?;
		sender.write_all(&[1, 2]).await?;
		drop(sender);

		let result = receiver.read_struct::<TestMessage>().await;
		assert!(result.is_err());
		Ok(())
	}

	#[tokio::test]
	async fn write_struct_flushes_writer() -> Result<()> {
		let message = TestMessage {
			command: "echo".to_owned(),
			args: vec!["flushed".to_owned()],
			exit_code: 0,
		};
		let (mut sender, mut receiver) = duplex(64);

		sender.write_struct(&message).await?;

		let mut len_bytes = [0u8; 4];
		let _bytes_read = receiver.read_exact(&mut len_bytes).await?;
		let payload_len = u32::from_be_bytes(len_bytes) as usize;
		assert!(payload_len > 0);
		Ok(())
	}
}
