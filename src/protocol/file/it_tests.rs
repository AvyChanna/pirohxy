use core::time::Duration;

use color_eyre::eyre::Result;
use tokio::io::AsyncReadExt;

use super::{FILE_CHUNK_LEN, File, MAX_BUFFERED_DOWNLOAD_LEN};
use crate::protocol::client::file;
use crate::protoutils::{alpn::ALPN_FILE, interproto::testnet};

/// A file larger than one `FILE_CHUNK_LEN`, so both sides have to chunk it.
fn payload() -> Vec<u8> {
	(0..200_000u32).map(|i| i.to_le_bytes()[0]).collect()
}

/// Fails the test instead of hanging when a step never completes.
async fn within<T>(step: impl Future<Output = T>) -> T {
	tokio::time::timeout(Duration::from_secs(20), step)
		.await
		.expect("file step timed out")
}

#[tokio::test]
async fn client_file_operations_round_trip_through_the_root() -> Result<()> {
	let root = testnet::TempDir::new("round-trip").await?;
	let server = testnet::spawn_server_on(ALPN_FILE, File::new(root.path().to_path_buf())).await?;
	let conn = server.conn();

	file::create_dir(conn, "docs").await?;
	file::upload(conn, "docs/note.txt", false, b"first half\n".as_slice()).await?;
	file::upload(conn, "docs/note.txt", true, b"second half\n".as_slice()).await?;

	let metadata = within(file::stat(conn, "docs/note.txt")).await?;
	assert!(metadata.is_file);
	assert!(!metadata.is_dir);
	assert_eq!(metadata.name, "docs/note.txt");
	assert_eq!(metadata.size, 23);

	let entries = within(file::list_dir(conn, "docs")).await?;
	assert_eq!(entries.len(), 1);
	assert_eq!(entries[0].name, "note.txt");

	let bytes = within(file::download(conn, "docs/note.txt", 0))
		.await?
		.read_to_end()
		.await?;
	assert_eq!(bytes, b"first half\nsecond half\n");

	let tail = within(file::download(conn, "docs/note.txt", 11))
		.await?
		.read_to_end()
		.await?;
	assert_eq!(tail, b"second half\n");

	file::rename(conn, "docs/note.txt", "docs/memo.txt").await?;
	assert!(file::stat(conn, "docs/note.txt").await.is_err());

	file::remove(conn, "docs/memo.txt").await?;
	file::remove(conn, "docs").await?;

	let missing = within(file::stat(conn, "docs/memo.txt"))
		.await
		.expect_err("a removed path must be reported");
	assert_ne!(missing.to_string(), "");
	Ok(())
}

#[tokio::test]
async fn client_file_operations_refuse_paths_outside_the_root() -> Result<()> {
	let root = testnet::TempDir::new("escapes").await?;
	let server = testnet::spawn_server_on(ALPN_FILE, File::new(root.path().to_path_buf())).await?;
	let conn = server.conn();

	let escape = within(file::stat(conn, "../outside"))
		.await
		.expect_err("a parent path must be refused")
		.to_string();
	assert!(escape.contains("not allowed") || escape.contains("escapes"));

	let absolute = within(file::upload(conn, "/etc/passwd", false, b"nope".as_slice()))
		.await
		.expect_err("an absolute path must be refused")
		.to_string();
	assert!(absolute.contains("not allowed") || absolute.contains("escapes"));

	// A refusal arrives as the first frame, so it is reported when the download is
	// opened rather than halfway through a copy.
	let refused = within(file::download(conn, "../outside", 0))
		.await
		.expect_err("a parent path must be refused before any bytes arrive");
	assert_ne!(refused.to_string(), "");
	Ok(())
}

#[tokio::test]
async fn client_file_upload_and_download_survive_many_chunks() -> Result<()> {
	let root = testnet::TempDir::new("chunks").await?;
	let server = testnet::spawn_server_on(ALPN_FILE, File::new(root.path().to_path_buf())).await?;
	let conn = server.conn();

	let payload = payload();
	file::upload(conn, "big.bin", false, &payload[..]).await?;

	let metadata = within(file::stat(conn, "big.bin")).await?;
	assert_eq!(metadata.size, u64::try_from(payload.len())?);

	let downloaded = within(file::download(conn, "big.bin", 0))
		.await?
		.read_to_end()
		.await?;
	assert_eq!(downloaded, payload);
	Ok(())
}

#[tokio::test]
async fn upload_streams_a_local_file() -> Result<()> {
	let root = testnet::TempDir::new("upload-root").await?;
	let source = testnet::TempDir::new("upload-source").await?;
	let server = testnet::spawn_server_on(ALPN_FILE, File::new(root.path().to_path_buf())).await?;
	let conn = server.conn();

	let payload = payload();
	let local = source.path().join("big.bin");
	tokio::fs::write(&local, &payload).await?;

	let mut handle = tokio::fs::File::open(&local).await?;
	file::upload(conn, "big.bin", false, &mut handle).await?;

	let metadata = within(file::stat(conn, "big.bin")).await?;
	assert_eq!(metadata.size, u64::try_from(payload.len())?);

	let downloaded = within(file::download(conn, "big.bin", 0))
		.await?
		.read_to_end()
		.await?;
	assert_eq!(downloaded, payload);
	Ok(())
}

#[tokio::test]
async fn download_pulls_one_frame_at_a_time() -> Result<()> {
	let root = testnet::TempDir::new("streaming").await?;
	let server = testnet::spawn_server_on(ALPN_FILE, File::new(root.path().to_path_buf())).await?;
	let conn = server.conn();

	let payload = payload();
	file::upload(conn, "big.bin", false, &payload[..]).await?;

	let mut download = within(file::download(conn, "big.bin", 0)).await?;
	let mut head = [0u8; 10];
	let _ = within(download.read_exact(&mut head)).await?;
	assert_eq!(head, payload[..10]);

	// Only the frame that carried the head was pulled. The rest of the file is still the
	// server's to send, which is the whole point of a stream: a reader that wants 10
	// bytes never holds 200 000.
	assert_eq!(
		download.bytes_received(),
		u64::try_from(FILE_CHUNK_LEN)?,
		"a 10 byte read pulled more than one frame"
	);

	drop(download);

	// Abandoning a download halfway through leaves the connection usable for the next.
	let tail = within(file::download(conn, "big.bin", 199_000))
		.await?
		.read_to_end()
		.await?;
	assert_eq!(tail, payload[199_000..]);
	Ok(())
}

#[tokio::test]
async fn buffered_download_refuses_to_grow_past_its_limit() -> Result<()> {
	let root = testnet::TempDir::new("buffered-limit").await?;
	let server = testnet::spawn_server_on(ALPN_FILE, File::new(root.path().to_path_buf())).await?;
	let conn = server.conn();

	let payload = payload();
	file::upload(conn, "big.bin", false, &payload[..]).await?;

	let download = within(file::download(conn, "big.bin", 0)).await?;
	let error = within(download.read_to_end_within(10_000))
		.await
		.expect_err("a 200 000 byte file must not fit a 10 000 byte buffer");
	let message = error.to_string();
	assert!(message.contains("10000"), "{message}");
	assert!(message.contains("stream"), "{message}");

	// The default ceiling is the documented constant, and it is roomier than a chunk.
	assert!(MAX_BUFFERED_DOWNLOAD_LEN > FILE_CHUNK_LEN as u64);
	Ok(())
}

#[tokio::test]
async fn download_of_an_empty_file_ends_cleanly() -> Result<()> {
	let root = testnet::TempDir::new("empty-file").await?;
	let server = testnet::spawn_server_on(ALPN_FILE, File::new(root.path().to_path_buf())).await?;
	let conn = server.conn();

	file::upload(conn, "empty.bin", false, b"".as_slice()).await?;

	let mut download = within(file::download(conn, "empty.bin", 0)).await?;
	let mut buffer = [0u8; 8];
	assert_eq!(within(download.read(&mut buffer)).await?, 0);
	assert_eq!(within(download.read_to_end()).await?.len(), 0);
	Ok(())
}
