use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::{env, error, str};

use git2::Repository;

const OUTFILE_NAME: &str = "long-help.txt";
const UNKNOWN_VAL: &str = "unknown";

fn rustc_version() -> String {
	if let Ok(rustc) = env::var("RUSTC")
		&& let Ok(output) = Command::new(rustc).arg("--version").output()
		&& let Ok(version) = str::from_utf8(&output.stdout)
	{
		version.trim().to_string()
	} else {
		UNKNOWN_VAL.to_string()
	}
}

fn pkg_version() -> String {
	let v = env!("CARGO_PKG_VERSION");
	if v.len() != 0 {
		v.to_string()
	} else {
		UNKNOWN_VAL.to_string()
	}
}

fn commit_info() -> String {
	if let Ok(repo_root) = env::var("CARGO_MANIFEST_DIR")
		&& let Ok(repo) = Repository::discover(repo_root)
		&& let Ok(head) = repo.head()
		&& let Ok(commit) = head.peel_to_commit()
	{
		commit.id().to_string()
	} else {
		UNKNOWN_VAL.to_string()
	}
}

fn main() -> Result<(), Box<dyn error::Error>> {
	let out_path = env::var("OUT_DIR")?;
	let out = Path::new(out_path.as_str()).join(OUTFILE_NAME);

	let mut f = File::create(out)?;
	write!(
		f,
		"v{}\ncommit: {}\nrustc: {}",
		pkg_version(),
		commit_info(),
		rustc_version()
	)?;

	Ok(())
}
