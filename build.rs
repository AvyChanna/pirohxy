use core::str;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::{env, error};

const OUTFILE_NAME: &str = "long-help.txt";
const UNKNOWN_VAL: &str = "unknown";

/// Paths that decide what this script writes. Only the ones that actually exist are
/// announced: cargo re-runs a build script on every build when a `rerun-if-changed`
/// path is missing, and `.git/packed-refs` or `.git/refs/heads` may be absent depending
/// on how the repository stores its refs.
const WATCHED_PATHS: &[&str] = &[
	"build.rs",
	"Cargo.toml",
	".git/HEAD",
	".git/refs/heads",
	".git/packed-refs",
];

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

/// The commit this checkout is at, asked from `git` itself.
///
/// `git2` was the previous choice, and it pulled `libgit2-sys` - a C build dependency -
/// purely to read one id, which complicated the musl dist target. Shelling out to `git`
/// keeps that out of the dependency tree, and a tree without `git` (a crates.io tarball,
/// for example) reports `unknown` instead of failing the build.
fn commit_info() -> Option<String> {
	let repo_root = env::var("CARGO_MANIFEST_DIR").ok()?;
	let output = Command::new("git")
		.args(["-C", &repo_root, "rev-parse", "HEAD"])
		.output()
		.ok()?;
	if !output.status.success() {
		return None;
	}
	Some(str::from_utf8(&output.stdout).ok()?.trim().to_owned())
}

fn main() -> Result<(), Box<dyn error::Error>> {
	let manifest_dir =
		PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));

	// Without these, cargo re-runs this script whenever anything in the package
	// changes, and never re-runs it when only the checked-out commit changed - which
	// is exactly the case that makes the embedded hash stale.
	for path in WATCHED_PATHS {
		if manifest_dir.join(path).exists() {
			println!("cargo::rerun-if-changed={path}");
		}
	}
	println!("cargo::rerun-if-env-changed=RUSTC");

	// OUT_DIR is provided by Cargo during build. These shenanigans are done to make codeql happy (.\\_//.)
	let out_dir_raw = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
	let out_dir = out_dir_raw
		.canonicalize()
		.expect("failed to canonicalize OUT_DIR");

	let target_dir = match env::var_os("CARGO_TARGET_DIR") {
		Some(t) => PathBuf::from(t),
		None => manifest_dir.join("target"),
	}
	.canonicalize()
	.expect("failed to canonicalize target dir");

	assert!(
		out_dir.starts_with(&target_dir),
		"OUT_DIR is outside target dir: {} not under {}",
		out_dir.display(),
		target_dir.display()
	);

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
