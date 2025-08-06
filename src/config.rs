use std::{
	fs,
	path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, ensure};
use ed25519_dalek::{
	SigningKey,
	pkcs8::{DecodePrivateKey, EncodePrivateKey, spki::der::pem::LineEnding},
};
use iroh::{NodeId, SecretKey, node_info::NodeIdExt};
use rand_core::OsRng;

const PRIV_KEY_NAME: &str = "self.priv";
const PUB_KEY_NAME: &str = "self.pub";

pub trait Auth {
	fn is_allowed(&self, key: &NodeId) -> bool;
}

pub trait IdentityManager {
	fn load(&self) -> Result<SecretKey>;
	fn generate(&self) -> Result<()>;
	fn exists(&self) -> bool {
		self.load().is_ok()
	}
}

pub trait NameResolver {
	fn resolve<T>(&self, name: T) -> Result<NodeId>
	where
		T: AsRef<str>;
}

#[derive(Debug)]
pub struct FileBasedAuth {
	auth_dir: PathBuf,
}

impl FileBasedAuth {
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

impl Auth for FileBasedAuth {
	fn is_allowed(&self, key: &NodeId) -> bool {
		let file_path = self.auth_dir.join(key.to_z32());
		file_path.exists() && file_path.is_file()
	}
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
		let raw_key = SigningKey::read_pkcs8_pem_file(&self.priv_key_file)?;
		Ok(raw_key.into())
	}

	fn generate(&self) -> Result<()> {
		ensure!(!self.exists(), "Identity already exists");
		let key = SecretKey::generate(&mut OsRng);
		key.secret()
			.write_pkcs8_pem_file(&self.priv_key_file, LineEnding::default())?;
		Ok(fs::write(&self.pub_key_file, key.public().to_z32())?)
	}
	fn exists(&self) -> bool {
		self.priv_key_file.exists() && self.priv_key_file.is_file()
	}
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
		if let Some(stripped) = name_str.strip_prefix(':') {
			ensure!(
				name_str.len() > 1,
				"Name cannot be empty after ':' prefix: '{}'",
				name_str
			);
			return Ok(NodeId::from_z32(stripped)?);
		}

		ensure!(
			!name_str.is_empty()
				&& name_str.len() < 64
				&& name_str.chars().all(|c| {
					matches!( c,
				'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-')
				}),
			"Name '{name_str}' is invalid. It must only use [a-zA-Z0-9._-] and be less than 64 characters",
		);
		let file_path = self.name_dir.join(name_str);
		let data = fs::read_to_string(file_path)?;
		Ok(NodeId::from_z32(data.trim())?)
	}
}
