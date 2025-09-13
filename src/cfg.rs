use std::{fs, path::PathBuf};

use color_eyre::eyre::{Result, ensure};
use ed25519_dalek::{
	SigningKey,
	pkcs8::{DecodePrivateKey, EncodePrivateKey, spki::der::pem::LineEnding},
};
use iroh::{NodeId, SecretKey, node_info::NodeIdExt};
use rand_core::OsRng;

const PRIV_KEY_NAME: &str = "self.priv";
const PUB_KEY_NAME: &str = "self.pub";

#[derive(Debug)]
pub(super) struct FileConfig {
	auth_dir: PathBuf,

	priv_key_file: PathBuf,
	pub_key_file: PathBuf,

	name_dir: PathBuf,
}

impl FileConfig {
	/// Creates a new `FileConfig` instance with the specified base directory.
	///
	/// # Errors
	/// The base directory or its children cannot be created or accessed.
	pub(super) fn new(base_dir: &PathBuf) -> Result<Self> {
		if !base_dir.exists() {
			fs::create_dir_all(base_dir)?;
		}

		let auth_dir = base_dir.join("auth");
		if !auth_dir.exists() {
			fs::create_dir_all(&auth_dir)?;
		}

		let name_dir = base_dir.join("names");
		if !name_dir.exists() {
			fs::create_dir_all(&name_dir)?;
		}

		Ok(Self {
			auth_dir,
			priv_key_file: base_dir.join(PRIV_KEY_NAME),
			pub_key_file: base_dir.join(PUB_KEY_NAME),
			name_dir,
		})
	}

	pub(super) fn is_access_allowed(&self, key: &NodeId) -> bool {
		let file_path = self.auth_dir.join(key.to_z32());
		file_path.exists() && file_path.is_file()
	}

	pub(super) fn load_identity(&self) -> Result<SecretKey> {
		let raw_key = SigningKey::read_pkcs8_pem_file(&self.priv_key_file)?;
		Ok(raw_key.into())
	}

	pub(super) fn init_identity(&self) -> Result<()> {
		ensure!(!self.does_identity_exist(), "Identity already exists");
		let key = SecretKey::generate(&mut OsRng);
		key.secret()
			.write_pkcs8_pem_file(&self.priv_key_file, LineEnding::default())?;
		Ok(fs::write(&self.pub_key_file, key.public().to_z32())?)
	}

	fn does_identity_exist(&self) -> bool {
		self.priv_key_file.exists() && self.priv_key_file.is_file()
	}

	pub(super) fn resolve_server_name<T>(&self, name: T) -> Result<NodeId>
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
