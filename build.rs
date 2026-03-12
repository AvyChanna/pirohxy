use core::str;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::{env, error};

use git2::Repository;

const OUTFILE_NAME: &str = "long-help.txt";
const UNKNOWN_VAL: &str = "unknown";

fn rustc_version() -> Option<String> {
	let rustc = env::var("RUSTC").ok()?;
	let output = Command::new(rustc).arg("--version").output().ok()?;
	let version = str::from_utf8(&output.stdout).ok()?;
	Some(version.trim().to_owned())
}

fn pkg_version() -> Option<String> {
	let v = env!("CARGO_PKG_VERSION");
	if v.is_empty() {
		None
	} else {
		Some(v.to_owned())
	}
}

fn commit_info() -> Option<String> {
	let repo_root = env::var("CARGO_MANIFEST_DIR").ok()?;
	let repo = Repository::discover(repo_root).ok()?;
	let head = repo.head().ok()?;
	let commit = head.peel_to_commit().ok()?;
	Some(commit.id().to_string())
}

fn main() -> Result<(), Box<dyn error::Error>> {
	// OUT_DIR is provided by Cargo during build. These shenanigans are done to make codeql happy (.\\_//.)
	let out_dir_raw = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
	let out_dir = out_dir_raw
		.canonicalize()
		.expect("failed to canonicalize OUT_DIR");

	let manifest_dir =
		PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));

	let target_dir = match env::var_os("CARGO_TARGET_DIR") {
		Some(t) => PathBuf::from(t),
		None => manifest_dir.join("target"),
	}
	.canonicalize()
	.expect("failed to canonicalize target dir");

	if !out_dir.starts_with(&target_dir) {
		panic!("OUT_DIR is outside target dir: {out_dir:?} not under {target_dir:?}");
	}

	// Now use out_dir for generated outputs.
	let out_file_name = out_dir.join(OUTFILE_NAME);

	// It is safe to write unsanitized data to this file - this is only used as help text for CLI
	write!(
		File::create(out_file_name)?,
		"v{}\ncommit: {}\nrustc: {}",
		pkg_version().unwrap_or(UNKNOWN_VAL.to_owned()),
		commit_info().unwrap_or(UNKNOWN_VAL.to_owned()),
		rustc_version().unwrap_or(UNKNOWN_VAL.to_owned())
	)?;

	Ok(())
}
