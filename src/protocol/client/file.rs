use core::{
	pin::Pin,
	task::{Context, Poll, ready},
};
use std::io;

use color_eyre::eyre::{Report, Result, ensure, eyre};
use iroh::endpoint::{Connection, RecvStream};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

use crate::{
	protocol::file::{
		FILE_CHUNK_LEN, FileMetadata, FilePayload, FileRequest, FileResponse,
		MAX_BUFFERED_DOWNLOAD_LEN,
	},
	protoutils::stream::{ReadStructExt, WriteStructExt},
};

/// Sends one request on a stream of its own and reads the first reply frame.
async fn exchange(conn: &Connection, request: &FileRequest) -> Result<FileResponse> {
	let (mut send, mut recv) = conn.open_bi().await?;
	send.write_struct(request).await?;
	send.finish()?;
	check(recv.read_struct::<FileResponse>().await?)
}

/// Turns a refusal into an error and lets every other frame through.
fn check(response: FileResponse) -> Result<FileResponse> {
	match response {
		FileResponse::Error(reason) => Err(eyre!(reason)),
		other => Ok(other),
	}
}

fn unexpected(response: &FileResponse) -> Report {
	eyre!("unexpected file reply: {response:?}")
}

/// Reports the metadata of one path inside the peer's file root.
///
/// # Errors
/// The peer refuses the path, the path does not exist, or the reply is malformed.
pub async fn stat(conn: &Connection, path: &str) -> Result<FileMetadata> {
	match exchange(
		conn,
		&FileRequest::Stat {
			path: path.to_owned(),
		},
	)
	.await?
	{
		FileResponse::Metadata(metadata) => Ok(*metadata),
		other => Err(unexpected(&other)),
	}
}

/// Lists the children of one directory inside the peer's file root.
///
/// # Errors
/// The peer refuses the path, the path is not a directory, or the reply is malformed.
pub async fn list_dir(conn: &Connection, path: &str) -> Result<Vec<FileMetadata>> {
	match exchange(
		conn,
		&FileRequest::ListDir {
			path: path.to_owned(),
		},
	)
	.await?
	{
		FileResponse::Entries(entries) => Ok(entries),
		other => Err(unexpected(&other)),
	}
}

/// Creates a directory, including its parents.
///
/// # Errors
/// The peer refuses the path or cannot create it.
pub async fn create_dir(conn: &Connection, path: &str) -> Result<()> {
	match exchange(
		conn,
		&FileRequest::CreateDir {
			path: path.to_owned(),
		},
	)
	.await?
	{
		FileResponse::Done => Ok(()),
		other => Err(unexpected(&other)),
	}
}

/// Removes a file, or an empty directory.
///
/// # Errors
/// The peer refuses the path or cannot remove it.
pub async fn remove(conn: &Connection, path: &str) -> Result<()> {
	match exchange(
		conn,
		&FileRequest::Remove {
			path: path.to_owned(),
		},
	)
	.await?
	{
		FileResponse::Done => Ok(()),
		other => Err(unexpected(&other)),
	}
}

/// Moves a path inside the peer's file root.
///
/// # Errors
/// The peer refuses either path or cannot move the entry.
pub async fn rename(conn: &Connection, from: &str, to: &str) -> Result<()> {
	let request = FileRequest::Rename {
		from: from.to_owned(),
		to: to.to_owned(),
	};
	match exchange(conn, &request).await? {
		FileResponse::Done => Ok(()),
		other => Err(unexpected(&other)),
	}
}

/// One remote file, handed out frame by frame.
///
/// The server answers a download with `FileResponse::Data` frames of at most
/// [`FILE_CHUNK_LEN`] bytes. This is an [`AsyncRead`] over those frames: the next frame is
/// requested only once the current one has been consumed, so a reader never holds more than
/// one frame at a time and a 2 GiB file costs 64 KiB of buffer instead of 2 GiB.
///
/// Dropping the value stops asking the peer for frames. Whatever it had already sent is
/// discarded when the connection closes, and the connection itself stays usable.
pub struct Download {
	/// The stream the frames come from. It is taken out of here while a read is in flight,
	/// because the future performing that read owns it.
	recv: Option<RecvStream>,
	/// The frame read in flight, if any.
	frame: Option<FrameReader>,
	/// The frame being handed out, and how far into it the reader has got.
	pending: Vec<u8>,
	cursor: usize,
	/// Bytes of file data pulled from the peer so far, whether or not the reader took them.
	received: u64,
	/// Set once the server finished the file, refused it mid-stream, or a read failed.
	finished: bool,
}

impl core::fmt::Debug for Download {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.debug_struct("Download")
			.field("pending", &self.pending.len())
			.field("cursor", &self.cursor)
			.field("received", &self.received)
			.field("finished", &self.finished)
			.finish_non_exhaustive()
	}
}

/// The read in flight: a frame request that owns the stream it reads from.
type FrameReader = Pin<Box<dyn Future<Output = Result<(FileResponse, RecvStream)>> + Send>>;

/// Reads one reply frame and hands the stream back, so the stream outlives the read.
async fn read_frame(mut recv: RecvStream) -> Result<(FileResponse, RecvStream)> {
	let response = recv.read_struct::<FileResponse>().await?;
	Ok((response, recv))
}

/// Turns a mid-stream failure into the `io::Error` an [`AsyncRead`] has to report.
///
/// The error chain is flattened into the message, because the readers built on top of this -
/// `tokio::io::copy` and friends - can only carry an `io::Error`.
fn read_failed(message: String) -> io::Error {
	io::Error::other(message)
}

impl Download {
	/// How many bytes of the file have been pulled from the peer so far.
	///
	/// A reader that stops early leaves the rest on the wire, and this is what shows it: the
	/// count is of frames received, not of bytes handed out.
	#[must_use]
	pub fn bytes_received(&self) -> u64 {
		self.received
	}

	/// Reads the whole file into memory, refusing to grow past [`MAX_BUFFERED_DOWNLOAD_LEN`].
	///
	/// # Errors
	/// The peer refuses the file mid-stream, the stream fails, or the file is larger than
	/// the limit.
	pub async fn read_to_end(self) -> Result<Vec<u8>> {
		self.read_to_end_within(MAX_BUFFERED_DOWNLOAD_LEN).await
	}

	/// Reads the whole file into memory, refusing to grow past `limit` bytes.
	///
	/// The CLI never does this - it copies the stream straight into a file or into stdout -
	/// but a caller that wants the bytes in one piece needs a ceiling on them.
	///
	/// # Errors
	/// The peer refuses the file mid-stream, the stream fails, or the file passes `limit`.
	pub async fn read_to_end_within(mut self, limit: u64) -> Result<Vec<u8>> {
		let mut data = Vec::new();
		let mut buffer = vec![0u8; FILE_CHUNK_LEN];
		loop {
			let read = self.read(&mut buffer).await?;
			if read == 0 {
				return Ok(data);
			}
			let buffered = u64::try_from(data.len())? + u64::try_from(read)?;
			ensure!(
				buffered <= limit,
				"download reached {buffered} bytes, above the {limit} byte in-memory limit - \
				 copy the stream instead of buffering it"
			);
			data.extend_from_slice(&buffer[..read]);
		}
	}
}

impl AsyncRead for Download {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		let this = self.get_mut();

		loop {
			if this.cursor < this.pending.len() {
				let take = buf.remaining().min(this.pending.len() - this.cursor);
				buf.put_slice(&this.pending[this.cursor..this.cursor + take]);
				this.cursor += take;
				return Poll::Ready(Ok(()));
			}

			if this.finished {
				// Leaving `buf` untouched is how `AsyncRead` spells a clean end of file.
				return Poll::Ready(Ok(()));
			}

			// The frame is spent. Drop it before asking for the next one.
			this.pending.clear();
			this.cursor = 0;

			let polled = match &mut this.frame {
				Some(frame) => Some(ready!(frame.as_mut().poll(cx))),
				None => None,
			};

			match polled {
				Some(Ok((FileResponse::Data(chunk), recv))) => {
					let len =
						u64::try_from(chunk.len()).map_err(|err| read_failed(err.to_string()))?;
					this.received += len;
					this.frame = None;
					this.recv = Some(recv);
					this.pending = chunk;
				}
				Some(Ok((FileResponse::Done, _recv))) => {
					this.frame = None;
					this.recv = None;
					this.finished = true;
					return Poll::Ready(Ok(()));
				}
				Some(Ok(other)) => {
					this.frame = None;
					this.recv = None;
					this.finished = true;
					return Poll::Ready(Err(read_failed(format!(
						"unexpected file reply: {other:?}"
					))));
				}
				Some(Err(err)) => {
					this.frame = None;
					this.recv = None;
					this.finished = true;
					return Poll::Ready(Err(read_failed(format!("{err:#}"))));
				}
				None => {
					let Some(recv) = this.recv.take() else {
						this.finished = true;
						return Poll::Ready(Ok(()));
					};
					this.frame = Some(Box::pin(read_frame(recv)));
				}
			}
		}
	}
}

/// Downloads `path`, resuming from `offset`, as a stream of its bytes.
///
/// # Errors
/// The peer refuses the path, the path cannot be read, or the reply is malformed. The first
/// frame is read before the stream is handed over, so a refusal is reported here instead of
/// halfway through a copy.
pub async fn download(conn: &Connection, path: &str, offset: u64) -> Result<Download> {
	let (mut send, mut recv) = conn.open_bi().await?;
	send.write_struct(&FileRequest::Download {
		path: path.to_owned(),
		offset,
	})
	.await?;
	send.finish()?;

	match recv.read_struct::<FileResponse>().await? {
		FileResponse::Data(chunk) => {
			let received = u64::try_from(chunk.len())?;
			Ok(Download {
				recv: Some(recv),
				frame: None,
				pending: chunk,
				cursor: 0,
				received,
				finished: false,
			})
		}
		FileResponse::Done => Ok(Download {
			recv: Some(recv),
			frame: None,
			pending: Vec::new(),
			cursor: 0,
			received: 0,
			finished: true,
		}),
		FileResponse::Error(reason) => Err(eyre!(reason)),
		other => Err(unexpected(&other)),
	}
}

/// Uploads `source` to the peer, replacing the file unless `append` is set.
///
/// The reader is pumped [`FILE_CHUNK_LEN`] bytes at a time, so uploading a 2 GiB file costs
/// one chunk of buffer rather than the file, and a `tokio::fs::File` can be handed straight
/// to it.
///
/// # Errors
/// The peer refuses the path, the file cannot be written, the reader fails, or the reply is
/// malformed.
pub async fn upload<R>(conn: &Connection, path: &str, append: bool, mut source: R) -> Result<()>
where
	R: AsyncRead + Unpin,
{
	let (mut send, mut recv) = conn.open_bi().await?;
	send.write_struct(&FileRequest::Upload {
		path: path.to_owned(),
		append,
	})
	.await?;

	let mut buffer = vec![0u8; FILE_CHUNK_LEN];
	loop {
		let read = source.read(&mut buffer).await?;
		if read == 0 {
			break;
		}
		send.write_struct(&FilePayload::Data(buffer[..read].to_vec()))
			.await?;
	}
	send.write_struct(&FilePayload::Done).await?;
	send.finish()?;

	match recv.read_struct::<FileResponse>().await? {
		FileResponse::Done => Ok(()),
		FileResponse::Error(reason) => Err(eyre!(reason)),
		other => Err(unexpected(&other)),
	}
}
