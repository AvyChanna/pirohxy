use core::time::Duration;
use std::{
	fs,
	time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::Result;

use super::{FilePayload, FileRequest, FileResponse, describe, resolve, seconds_since_epoch};
use crate::protoutils::interproto::testnet;

#[test]
fn file_messages_round_trip_through_wire_format() -> Result<()> {
	let requests = [
		FileRequest::Stat {
			path: "notes.txt".to_owned(),
		},
		FileRequest::Download {
			path: "a/b.bin".to_owned(),
			offset: 4096,
		},
		FileRequest::Upload {
			path: "a/b.bin".to_owned(),
			append: true,
		},
		FileRequest::Rename {
			from: "old".to_owned(),
			to: "new/old".to_owned(),
		},
	];
	let payloads = [FilePayload::Data(vec![1, 2, 3]), FilePayload::Done];
	let responses = [
		FileResponse::Data(vec![9, 8, 7]),
		FileResponse::Done,
		FileResponse::Error("denied".to_owned()),
		FileResponse::Entries(Vec::new()),
	];

	for request in requests {
		assert_eq!(
			postcard::from_bytes::<FileRequest>(&postcard::to_allocvec(&request)?)?,
			request
		);
	}
	for payload in payloads {
		assert_eq!(
			postcard::from_bytes::<FilePayload>(&postcard::to_allocvec(&payload)?)?,
			payload
		);
	}
	for response in responses {
		assert_eq!(
			postcard::from_bytes::<FileResponse>(&postcard::to_allocvec(&response)?)?,
			response
		);
	}
	Ok(())
}

#[test]
fn resolves_relative_paths_inside_the_root() -> Result<()> {
	let dir = testnet::TempDir::new_sync("resolve")?;
	fs::create_dir_all(dir.path().join("sub"))?;
	fs::write(dir.path().join("sub/notes.txt"), b"hi")?;
	let root = dir.path().canonicalize()?;

	assert_eq!(resolve(&root, "sub/notes.txt")?, root.join("sub/notes.txt"));
	assert_eq!(resolve(&root, ".")?, root);
	assert!(resolve(&root, "sub/missing.txt").is_ok());
	Ok(())
}

#[test]
fn rejects_paths_that_escape_the_root() -> Result<()> {
	let dir = testnet::TempDir::new_sync("reject")?;
	let root = dir.path().canonicalize()?;

	for path in ["../escape", "sub/../../escape", "/etc/passwd", "sub/../.."] {
		let result = resolve(&root, path);
		assert!(result.is_err(), "{path} should have been rejected");
	}
	assert!(resolve(&root, "").is_err());
	Ok(())
}

#[test]
fn describe_reports_the_metadata_this_module_serves() -> Result<()> {
	let dir = testnet::TempDir::new_sync("describe")?;
	fs::write(dir.path().join("notes.txt"), b"hello world")?;
	fs::create_dir_all(dir.path().join("sub"))?;

	let file = describe("notes.txt", &fs::metadata(dir.path().join("notes.txt"))?);
	assert_eq!(file.name, "notes.txt");
	assert!(file.is_file);
	assert!(!file.is_dir);
	assert!(!file.is_symlink);
	assert_eq!(file.size, 11);

	// The modification time is a wall-clock reading, so it is compared against now
	// rather than against a fixed value.
	let now = seconds_since_epoch(SystemTime::now());
	assert!(
		(file.modified_secs - now).abs() <= 60,
		"mtime {} is not near now ({now})",
		file.modified_secs
	);

	let entry = describe("sub", &fs::metadata(dir.path().join("sub"))?);
	assert!(entry.is_dir);
	assert!(!entry.is_file);
	Ok(())
}

#[test]
fn seconds_since_epoch_covers_both_sides_of_the_epoch() {
	assert_eq!(seconds_since_epoch(UNIX_EPOCH), 0);
	assert_eq!(seconds_since_epoch(UNIX_EPOCH + Duration::from_secs(5)), 5);
	assert_eq!(seconds_since_epoch(UNIX_EPOCH - Duration::from_secs(5)), -5);
}
