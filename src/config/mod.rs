use std::path::PathBuf;

use color_eyre::eyre::{Result, ensure};
use directories::ProjectDirs;

pub mod auther;
pub mod identity;
pub mod resolver;

const QUALIFIER_NAME: &str = "";
const ORGANIZATION_NAME: &str = "avychanna";
const APPLICATION_NAME: &str = "piroxy";

#[must_use]
pub fn default_config_path() -> PathBuf {
	let base_dir = ProjectDirs::from(QUALIFIER_NAME, ORGANIZATION_NAME, APPLICATION_NAME)
		.expect("Could not determine config dir");
	base_dir.config_local_dir().to_path_buf()
}

fn ensure_name_sanitized(name: &str) -> Result<()> {
	ensure!(
		!name.is_empty()
			&& name.len() < 64
			&& name.chars().all(|c| {
				matches!( c,
				'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-')
			}),
		"Name '{name}' is invalid. It must only use [a-zA-Z0-9._-] and be less than 64 characters",
	);
	Ok(())
}
