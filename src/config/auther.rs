use std::{
	fs,
	path::{Path, PathBuf},
};

use color_eyre::eyre::Result;
use iroh::{NodeId, node_info::NodeIdExt};

pub trait Authenticator {
	fn authenticate(&self, key: &NodeId) -> bool;
}

#[derive(Debug)]
pub struct FileBasedAuther {
	auth_dir: PathBuf,
}

impl FileBasedAuther {
	pub fn new<T>(base_dir: T) -> Result<Self>
	where
		T: AsRef<Path>,
	{
		let auth_dir = base_dir.as_ref().join("auth");
		if !auth_dir.exists() {
			fs::create_dir_all(&auth_dir)?;
		}
		Ok(Self { auth_dir })
	}
}

impl Authenticator for FileBasedAuther {
	fn authenticate(&self, key: &NodeId) -> bool {
		let file_path = self.auth_dir.join(key.to_z32());
		file_path.exists() && file_path.is_file()
	}
}
