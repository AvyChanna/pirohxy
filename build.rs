use std::fmt::Display;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::str;

use anyhow::Result;
use git2::{Reference, Repository};

fn rustc_version() -> Result<String> {
	let output = Command::new("rustc").arg("--version").output()?;
	Ok(str::from_utf8(&output.stdout)?.trim().to_string())
}

fn get_profile_base_path(path: &Path) -> Option<String> {
	let mut current_path = path;

	for _ in 0..4 {
		match current_path.parent() {
			Some(parent) => current_path = parent,
			None => return None,
		}
	}

	Some(current_path.file_name()?.to_str()?.to_owned())
}

#[derive(Debug)]
pub struct LongHelp {
	pub clean: String,
	pub commit: String,
	pub branch: String,
	pub tag: String,
	pub pkg_version: String,
	pub build_profile: String,
	pub rustc_version: String,
}

impl Default for LongHelp {
	fn default() -> Self {
		LongHelp {
			clean: String::from("unknown"),
			commit: String::from("unknown"),
			branch: String::from("unknown"),
			tag: String::from("unknown"),
			pkg_version: String::from("unknown"),
			build_profile: String::from("unknown"),
			rustc_version: String::from("unknown"),
		}
	}
}

impl Display for LongHelp {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}\n", self.pkg_version)?;
		write!(f, "commit: {}\n", self.commit)?;
		write!(f, "branch: {}\n", self.branch)?;
		write!(f, "tag: {}\n", self.tag)?;
		write!(f, "clean: {}\n", self.clean)?;
		write!(f, "profile: {}\n", self.build_profile)?;
		write!(f, "rustc: {}\n", self.rustc_version)
	}
}

impl LongHelp {
	fn new() -> Self {
		Self::default()
	}

	fn init(src_path: &Path, out_path: &Path) -> Self {
		let mut this = LongHelp::new();
		if let Err(err) = this.set_git_info(src_path) {
			eprintln!("err: {}", err);
		}
		this.set_env_info(out_path);
		this
	}

	fn set_env_info(&mut self, path: &Path) {
		self.pkg_version = env!("CARGO_PKG_VERSION").to_string();

		if let Some(profile) = get_profile_base_path(path) {
			self.build_profile = profile;
		}

		if let Ok(value) = rustc_version() {
			self.rustc_version = value;
		}
	}

	fn set_git_info(&mut self, path: &Path) -> Result<()> {
		let repo = Repository::discover(path)?;
		self.set_dirty_status(&repo);

		let head = repo.head()?;
		if let Some(v) = head.shorthand() {
			self.branch = v.to_string();
		};
		self.set_commit_and_tag(&repo, head)
	}

	fn set_commit_and_tag(&mut self, repo: &Repository, head: Reference<'_>) -> Result<()> {
		// commit info
		let head_commit = head.peel_to_commit()?.id();
		self.commit = head_commit.to_string();

		// tag info
		let all_tags = repo.tag_names(None)?;
		for tag in all_tags.iter() {
			if let Some(tag) = tag {
				let tag_obj = repo
					.find_reference(&format!("refs/tags/{}", tag))?
					.peel_to_commit()?;
				if tag_obj.id() == head_commit {
					self.tag = tag.to_string();
					break;
				}
			}
		}
		Ok(())
	}

	fn set_dirty_status(&mut self, repo: &Repository) {
		let mut repo_opts = git2::StatusOptions::new();

		if let Ok(status) = repo.statuses(Some(
			repo_opts.include_ignored(false).include_untracked(true),
		)) {
			self.clean = format!("{}", status.is_empty());
		}
	}
}

const OUTFILE: &str = "long-help.txt";

fn main() -> Result<()> {
	let src_path = std::env::var("CARGO_MANIFEST_DIR")?;
	let out_path = std::env::var("OUT_DIR")?;
	let out = {
		let path = Path::new(out_path.as_str());
		if !out_path.ends_with('/') {
			path.join(format!("{out_path}/{OUTFILE}"))
		} else {
			path.join(OUTFILE)
		}
	};

	let src_path = Path::new(&src_path);
	let long_help = LongHelp::init(src_path, out.as_path());

	let mut f = File::create(out)?;
	write!(f, "{}", long_help)?;
	Ok(())
}

// const PKG_VERSION :&str = r#"0.0.1"#;
// const COMMIT_HASH :&str = r#"HEAD"#;
// const BRANCH :&str = r#""#;
// const TAG :&str = r#""#;
// const GIT_CLEAN :bool = false;
// const BUILD_RUST_CHANNEL :&str = r#"debug"#;
// const RUST_VERSION :&str = r#"rustc 1.82.0 (f6e511eec 2024-10-15)"#;
