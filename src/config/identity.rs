use std::{
	fs,
	path::{Path, PathBuf},
};

use color_eyre::eyre::Result;
use ed25519_dalek::{
	SigningKey,
	pkcs8::{DecodePrivateKey, EncodePrivateKey, spki::der::pem::LineEnding},
};
use iroh::{SecretKey, node_info::NodeIdExt};
use rand_core::OsRng;

const PRIV_KEY_NAME: &str = "self.priv";
const PUB_KEY_NAME: &str = "self.pub";

pub trait IdentityManager {
	fn load(&self) -> Result<SecretKey>;
	fn generate(&self) -> Result<()>;
	fn exists(&self) -> bool;
}

#[derive(Debug)]
pub struct FileBasedIdentity {
	priv_key_file: PathBuf,
	pub_key_file: PathBuf,
}

impl FileBasedIdentity {
	pub fn new<T>(key_dir: T) -> Result<Self>
	where
		T: AsRef<Path>,
	{
		let key_dir = key_dir.as_ref();
		if !key_dir.exists() {
			fs::create_dir_all(key_dir)?;
		}
		Ok(Self {
			priv_key_file: key_dir.join(PRIV_KEY_NAME),
			pub_key_file: key_dir.join(PUB_KEY_NAME),
		})
	}
}

impl IdentityManager for FileBasedIdentity {
	fn load(&self) -> Result<SecretKey> {
		let raw_key = SigningKey::read_pkcs8_pem_file(self.priv_key_file.clone())?;
		Ok(raw_key.into())
	}

	fn generate(&self) -> Result<()> {
		let key = SecretKey::generate(&mut OsRng);
		key.secret()
			.write_pkcs8_pem_file(self.priv_key_file.clone(), LineEnding::default())?;
		Ok(fs::write(self.pub_key_file.clone(), key.public().to_z32())?)
	}
	fn exists(&self) -> bool {
		self.priv_key_file.exists() && self.priv_key_file.is_file()
	}
}
