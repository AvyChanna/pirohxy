use std::{
	fs,
	path::{Path, PathBuf},
};

use color_eyre::eyre::Result;
use iroh::{NodeId, node_info::NodeIdExt};

use crate::config::ensure_name_sanitized;

pub trait NameResolver {
	fn resolve<T>(&self, name: T) -> Result<NodeId>
	where
		T: AsRef<str>;
}

#[derive(Debug)]
pub struct FileBasedNameResolver {
	name_dir: PathBuf,
}

impl FileBasedNameResolver {
	pub fn new<T>(base_dir: T) -> Result<Self>
	where
		T: AsRef<Path>,
	{
		let name_dir = base_dir.as_ref().join("names");
		if !name_dir.exists() {
			fs::create_dir_all(&name_dir)?;
		}
		Ok(Self { name_dir })
	}
}

impl NameResolver for FileBasedNameResolver {
	fn resolve<T>(&self, name: T) -> Result<NodeId>
	where
		T: AsRef<str>,
	{
		let name_str = name.as_ref();
		ensure_name_sanitized(name_str)?;
		let file_path = self.name_dir.join(name_str);
		let data = fs::read_to_string(file_path)?;
		Ok(NodeId::from_z32(&data)?)
	}
}
