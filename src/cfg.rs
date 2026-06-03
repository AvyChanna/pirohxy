use std::{
	fs::{self, read_to_string},
	path::PathBuf,
};

use color_eyre::eyre::{Result, ensure, eyre};
use iroh::{EndpointId, SecretKey};

const PRIV_KEY_NAME: &str = "self.priv";
const PUB_KEY_NAME: &str = "self.pub";
const SECRET_KEY_LENGTH: usize = 32;

#[derive(Debug, Clone)]
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

		let gitignore_file = base_dir.join(".gitignore");
		if !gitignore_file.exists() {
			fs::write(&gitignore_file, "*\n")?;
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
		let key = SecretKey::generate();
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

#[cfg(test)]
mod tests {
	use std::{
		fs,
		path::{Path, PathBuf},
	};

	use color_eyre::eyre::Result;
	use iroh::SecretKey;

	use super::FileConfig;

	fn fresh_base(name: &str) -> Result<PathBuf> {
		let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
		path.push("target");
		path.push("test-data");
		path.push(format!("cfg-{name}-{}", std::process::id()));
		cleanup(&path)?;
		Ok(path)
	}

	fn cleanup(path: &Path) -> Result<()> {
		if path.exists() {
			fs::remove_dir_all(path)?;
		}
		Ok(())
	}

	#[test]
	fn new_creates_expected_directory_layout() -> Result<()> {
		let base = fresh_base("layout")?;
		let cfg = FileConfig::new(&base)?;

		assert!(cfg.auth_dir.is_dir());
		assert!(cfg.name_dir.is_dir());
		assert_eq!(fs::read_to_string(base.join(".gitignore"))?, "*\n");
		assert_eq!(cfg.priv_key_file, base.join("self.priv"));
		assert_eq!(cfg.pub_key_file, base.join("self.pub"));

		cleanup(&base)
	}

	#[test]
	fn identity_round_trips_and_cannot_be_reinitialized() -> Result<()> {
		let base = fresh_base("identity")?;
		let cfg = FileConfig::new(&base)?;

		cfg.init_identity()?;
		let loaded = cfg.load_identity()?;
		let public_from_file = fs::read_to_string(base.join("self.pub"))?;

		assert_eq!(loaded.public().to_z32(), public_from_file);
		assert!(cfg.init_identity().is_err());

		let loaded2 = cfg.load_identity()?;
		assert_eq!(loaded2.public().to_z32(), public_from_file);

		cleanup(&base)
	}

	#[test]
	fn load_identity_rejects_wrong_length_key_data() -> Result<()> {
		let base = fresh_base("bad-identity")?;
		let cfg = FileConfig::new(&base)?;
		fs::write(base.join("self.priv"), z32::encode(&[1, 2, 3]))?;

		let err = cfg.load_identity().expect_err("short key should fail");
		assert!(err.to_string().contains("invalid privkey size"));

		cleanup(&base)
	}

	#[test]
	fn access_is_allowed_only_for_files_named_after_endpoint_ids() -> Result<()> {
		let base = fresh_base("access")?;
		let cfg = FileConfig::new(&base)?;
		let key = SecretKey::generate().public();

		assert!(!cfg.is_access_allowed(&key));
		fs::write(cfg.auth_dir.join(key.to_z32()), "")?;
		assert!(cfg.is_access_allowed(&key));

		let other = SecretKey::generate().public();
		fs::create_dir_all(cfg.auth_dir.join(other.to_z32()))?;
		assert!(!cfg.is_access_allowed(&other));

		cleanup(&base)
	}

	#[test]
	fn resolves_direct_endpoint_ids_and_named_entries() -> Result<()> {
		let base = fresh_base("names")?;
		let cfg = FileConfig::new(&base)?;
		let endpoint = SecretKey::generate().public();

		assert_eq!(
			cfg.resolve_server_name(format!(":{}", endpoint.to_z32()))?,
			endpoint
		);

		fs::write(cfg.name_dir.join("prod_1"), format!("{}\n", endpoint.to_z32()))?;
		assert_eq!(cfg.resolve_server_name("prod_1")?, endpoint);

		cleanup(&base)
	}

	#[test]
	fn rejects_invalid_server_names_before_reading_files() -> Result<()> {
		let base = fresh_base("invalid-names")?;
		let cfg = FileConfig::new(&base)?;

		assert!(cfg.resolve_server_name("").is_err());
		assert!(cfg.resolve_server_name(":").is_err());
		assert!(cfg.resolve_server_name(":hello").is_err());
		assert!(cfg.resolve_server_name("has/slash").is_err());
		assert!(cfg.resolve_server_name("a".repeat(64)).is_err());

		cleanup(&base)
	}
}
