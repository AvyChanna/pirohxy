use core::fmt::Debug;
use std::{
	fs::Metadata as FsMetadata,
	io::SeekFrom,
	path::{Component, Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, ensure};
use iroh::{
	endpoint::{Connection, RecvStream, SendStream},
	protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::{
	fs::File as FsFile,
	io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};
use tracing::{debug, trace};

use crate::protoutils::{
	accept::serve_bi_streams,
	stream::{ReadStructExt, WriteStructExt},
};

/// How many bytes of a file are sent per frame.
///
/// Far below [`MAX_STRUCT_FRAME_LEN`], which is what the frame cap allows; the assertion
/// below keeps the two from drifting apart.
///
/// [`MAX_STRUCT_FRAME_LEN`]: crate::protoutils::stream::MAX_STRUCT_FRAME_LEN
pub const FILE_CHUNK_LEN: usize = 64 * 1024;

const _: () = assert!(FILE_CHUNK_LEN < crate::protoutils::stream::MAX_STRUCT_FRAME_LEN);

/// Largest download the buffered helpers will hold in memory.
///
/// A download is a stream by default - [`Download`] is an `AsyncRead` over the frames the
/// server sends - so this only bounds [`Download::read_to_end`], the convenience path for a
/// caller that wants the whole file in one piece. It is a thousand chunks: roomy enough for
/// the tests and for small-file callers, and far below the size at which a transfer should
/// be a copy instead of a buffer.
///
/// [`Download`]: crate::protocol::client::file::Download
/// [`Download::read_to_end`]: crate::protocol::client::file::Download::read_to_end
pub const MAX_BUFFERED_DOWNLOAD_LEN: u64 = 1024 * FILE_CHUNK_LEN as u64;

const _: () = assert!(MAX_BUFFERED_DOWNLOAD_LEN > FILE_CHUNK_LEN as u64);

/// The first frame on a file stream: the operation to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileRequest {
	/// Report metadata about one path.
	Stat {
		/// Path relative to the server's file root.
		path: String,
	},
	/// List the entries of a directory.
	ListDir {
		/// Path relative to the server's file root.
		path: String,
	},
	/// Stream a file back to the client, starting at `offset`.
	Download {
		/// Path relative to the server's file root.
		path: String,
		/// Byte to resume from.
		offset: u64,
	},
	/// Receive a file from the client.
	Upload {
		/// Path relative to the server's file root.
		path: String,
		/// Append to the file instead of replacing it.
		append: bool,
	},
	/// Create a directory, including parents.
	CreateDir {
		/// Path relative to the server's file root.
		path: String,
	},
	/// Remove a file, or an empty directory.
	Remove {
		/// Path relative to the server's file root.
		path: String,
	},
	/// Move or rename a path inside the root.
	Rename {
		/// Source path relative to the server's file root.
		from: String,
		/// Destination path relative to the server's file root.
		to: String,
	},
}

/// Follow-up frames for an upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilePayload {
	/// A chunk of the file being uploaded.
	Data(Vec<u8>),
	/// The upload is complete.
	Done,
}

/// Metadata about one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
	/// Name of the entry, relative to the file root.
	pub name: String,
	/// Whether the entry is a directory.
	pub is_dir: bool,
	/// Whether the entry is a regular file.
	pub is_file: bool,
	/// Whether the entry is a symlink.
	pub is_symlink: bool,
	/// Size in bytes.
	pub size: u64,
	/// Last modification time, as seconds since the Unix epoch.
	pub modified_secs: i64,
}

/// Everything the server answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileResponse {
	/// Metadata for a single path.
	Metadata(Box<FileMetadata>),
	/// Metadata for the children of a directory.
	Entries(Vec<FileMetadata>),
	/// A chunk of a downloaded file.
	Data(Vec<u8>),
	/// The operation finished successfully.
	Done,
	/// The operation failed.
	Error(String),
}

/// A `File` protocol handler for Iroh that implements the `ProtocolHandler` trait.
///
/// Every path it touches is resolved relative to `root`, so a client can only reach the
/// directory the operator opted into.
#[derive(Debug, Clone)]
pub struct File {
	root: PathBuf,
}

impl File {
	/// Creates a new `File` instance serving `root`.
	#[must_use]
	pub fn new(root: PathBuf) -> Self {
		Self { root }
	}

	/// The directory this handler is confined to.
	#[must_use]
	pub fn root(&self) -> &Path {
		&self.root
	}
}

impl ProtocolHandler for File {
	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		let root = self.root.clone();
		serve_bi_streams(&conn, "File", move |r, s| {
			handle_file_stream(root.clone(), r, s)
		})
		.await
	}
}

#[expect(
	let_underscore_drop,
	reason = "Reporting a failed file operation is best-effort: the stream may already be gone"
)]
async fn handle_file_stream(root: PathBuf, mut r: RecvStream, mut s: SendStream) -> Result<()> {
	let request: FileRequest = r.read_struct().await?;
	trace!("File request: {request:?}");

	match dispatch(&root, &mut r, &mut s, request).await {
		Ok(()) => {
			let _ = s.finish();
		}
		Err(err) => {
			let message = format!("{err:#}");
			debug!("File operation failed: {message}");
			let _ = send_error(&mut s, &message).await;
		}
	}

	Ok(())
}

async fn dispatch(
	root: &Path,
	r: &mut RecvStream,
	s: &mut SendStream,
	request: FileRequest,
) -> Result<()> {
	match request {
		FileRequest::Stat { path } => stat(root, s, &path).await,
		FileRequest::ListDir { path } => list_dir(root, s, &path).await,
		FileRequest::Download { path, offset } => download(root, s, &path, offset).await,
		FileRequest::Upload { path, append } => upload(root, r, s, &path, append).await,
		FileRequest::CreateDir { path } => create_dir(root, s, &path).await,
		FileRequest::Remove { path } => remove(root, s, &path).await,
		FileRequest::Rename { from, to } => rename(root, s, &from, &to).await,
	}
}

async fn stat(root: &Path, s: &mut SendStream, path: &str) -> Result<()> {
	let target = resolve(root, path)?;
	let metadata = tokio::fs::metadata(&target).await?;
	s.write_struct(&FileResponse::Metadata(Box::new(describe(path, &metadata))))
		.await
}

async fn list_dir(root: &Path, s: &mut SendStream, path: &str) -> Result<()> {
	let target = resolve(root, path)?;
	let mut entries = Vec::new();
	let mut read_dir = tokio::fs::read_dir(&target).await?;
	while let Some(entry) = read_dir.next_entry().await? {
		let file_type = entry.file_type().await?;
		let metadata = entry.metadata().await?;
		entries.push(FileMetadata {
			name: entry.file_name().to_string_lossy().into_owned(),
			is_dir: file_type.is_dir(),
			is_file: file_type.is_file(),
			is_symlink: file_type.is_symlink(),
			size: metadata.len(),
			modified_secs: metadata.modified().map_or(0, seconds_since_epoch),
		});
	}
	entries.sort_by(|a, b| a.name.cmp(&b.name));
	s.write_struct(&FileResponse::Entries(entries)).await
}

async fn download(root: &Path, s: &mut SendStream, path: &str, offset: u64) -> Result<()> {
	let target = resolve(root, path)?;
	let mut file = FsFile::open(&target).await?;
	if offset > 0 {
		let _pos = file.seek(SeekFrom::Start(offset)).await?;
	}
	let mut buffer = vec![0u8; FILE_CHUNK_LEN];
	loop {
		let read = file.read(&mut buffer).await?;
		if read == 0 {
			break;
		}
		s.write_struct(&FileResponse::Data(buffer[..read].to_vec()))
			.await?;
	}
	s.write_struct(&FileResponse::Done).await
}

async fn upload(
	root: &Path,
	r: &mut RecvStream,
	s: &mut SendStream,
	path: &str,
	append: bool,
) -> Result<()> {
	let target = resolve(root, path)?;
	if let Some(parent) = target.parent() {
		tokio::fs::create_dir_all(parent).await?;
	}
	let mut file = if append {
		tokio::fs::OpenOptions::new()
			.append(true)
			.open(&target)
			.await?
	} else {
		FsFile::create(&target).await?
	};
	while let FilePayload::Data(chunk) = r.read_struct::<FilePayload>().await? {
		file.write_all(&chunk).await?;
	}
	file.flush().await?;
	s.write_struct(&FileResponse::Done).await
}

async fn create_dir(root: &Path, s: &mut SendStream, path: &str) -> Result<()> {
	let target = resolve(root, path)?;
	tokio::fs::create_dir_all(&target).await?;
	s.write_struct(&FileResponse::Done).await
}

async fn remove(root: &Path, s: &mut SendStream, path: &str) -> Result<()> {
	let target = resolve(root, path)?;
	let metadata = tokio::fs::symlink_metadata(&target).await?;
	if metadata.is_dir() {
		tokio::fs::remove_dir(&target).await?;
	} else {
		tokio::fs::remove_file(&target).await?;
	}
	s.write_struct(&FileResponse::Done).await
}

async fn rename(root: &Path, s: &mut SendStream, from: &str, to: &str) -> Result<()> {
	let source = resolve(root, from)?;
	let destination = resolve(root, to)?;
	if let Some(parent) = destination.parent() {
		tokio::fs::create_dir_all(parent).await?;
	}
	tokio::fs::rename(&source, &destination).await?;
	s.write_struct(&FileResponse::Done).await
}

async fn send_error(s: &mut SendStream, message: &str) -> Result<()> {
	s.write_struct(&FileResponse::Error(message.to_owned()))
		.await
}

/// Resolves a client-supplied path against `root`, refusing anything that would escape
/// it.
///
/// # Errors
/// The path is empty, absolute, contains `..`, or resolves outside the root.
fn resolve(root: &Path, relative: &str) -> Result<PathBuf> {
	ensure!(!relative.is_empty(), "path must not be empty");
	let requested = Path::new(relative);
	ensure!(
		!requested.is_absolute(),
		"absolute paths are not allowed: {relative}"
	);
	ensure!(
		!requested
			.components()
			.any(|c| matches!(c, Component::ParentDir | Component::Prefix(_))),
		"'..' components are not allowed: {relative}"
	);

	let root = root.canonicalize()?;
	let joined = root.join(requested);

	// Canonicalize the deepest existing ancestor: a symlink anywhere along the path must
	// also land inside the root.
	let mut anchor = joined.clone();
	while !anchor.exists() {
		let Some(parent) = anchor.parent().map(Path::to_path_buf) else {
			break;
		};
		anchor = parent;
	}
	let real = anchor.canonicalize()?;
	ensure!(
		real.starts_with(&root),
		"path escapes the configured root: {relative}"
	);

	Ok(joined)
}

fn describe(name: &str, metadata: &FsMetadata) -> FileMetadata {
	FileMetadata {
		name: name.to_owned(),
		is_dir: metadata.is_dir(),
		is_file: metadata.is_file(),
		is_symlink: metadata.is_symlink(),
		size: metadata.len(),
		modified_secs: metadata.modified().map_or(0, seconds_since_epoch),
	}
}

fn seconds_since_epoch(time: SystemTime) -> i64 {
	match time.duration_since(UNIX_EPOCH) {
		Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
		Err(err) => -i64::try_from(err.duration().as_secs()).unwrap_or(i64::MIN),
	}
}

#[cfg(test)]
mod it_tests;

#[cfg(test)]
mod ut_tests;
