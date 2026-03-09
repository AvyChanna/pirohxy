use std::{
	fs::{self, read_to_string},
	path::PathBuf,
};

use color_eyre::eyre::{Result, ensure, eyre};
use iroh::{EndpointId, SecretKey, endpoint_info::EndpointIdExt};

const PRIV_KEY_NAME: &str = "self.priv";
const PUB_KEY_NAME: &str = "self.pub";
const SECRET_KEY_LENGTH: usize = 32;

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

	pub(super) fn is_access_allowed(&self, key: &EndpointId) -> bool {
		let file_path = self.auth_dir.join(key.to_z32());
		file_path.exists() && file_path.is_file()
	}

	pub(super) fn load_identity(&self) -> Result<SecretKey> {
		let raw_key = read_to_string(&self.priv_key_file)?;
		let decoded = z32::decode(raw_key.trim().as_bytes())?;
		let key_bytes: [u8; SECRET_KEY_LENGTH] = decoded
			.try_into()
			.map_err(|_| eyre!("invalid privkey size"))?;
		Ok(SecretKey::from_bytes(&key_bytes))
	}

	pub(super) fn init_identity(&self) -> Result<()> {
		ensure!(!self.does_identity_exist(), "Identity already exists");
		let key = SecretKey::generate(&mut rand::rng());
		let encoded = z32::encode(&key.to_bytes());
		fs::write(&self.priv_key_file, encoded)?;
		Ok(fs::write(&self.pub_key_file, key.public().to_z32())?)
	}

	fn does_identity_exist(&self) -> bool {
		self.priv_key_file.exists() && self.priv_key_file.is_file()
	}

	pub(super) fn resolve_server_name<T>(&self, name: T) -> Result<EndpointId>
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
			return Ok(EndpointId::from_z32(stripped)?);
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
		let data = read_to_string(file_path)?;
		Ok(EndpointId::from_z32(data.trim())?)
	}
}
