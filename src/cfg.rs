use std::{
	fs::{self, read_to_string},
	path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, ensure, eyre};
use iroh::{EndpointId, SecretKey};
use tracing::debug;

use pirohxy::protoutils::alpn::Protocol;

const PRIV_KEY_NAME: &str = "self.priv";
const PUB_KEY_NAME: &str = "self.pub";
const SECRET_KEY_LENGTH: usize = 32;
/// Longest server alias accepted by `resolve_server_name`.
const MAX_NAME_LEN: usize = 64;
/// Directory the `file` protocol is confined to unless `--file-root` says otherwise.
const FILES_DIR_NAME: &str = "files";
/// Name of the grant file inside `auth/<endpoint-id>/` that allows every protocol.
const GRANT_ALL: &str = "all";
/// Permissions the private key is written with: owner read/write only.
#[cfg(unix)]
const PRIVATE_KEY_MODE: u32 = 0o600;

#[derive(Debug, Clone)]
pub(super) struct FileConfig {
	auth_dir: PathBuf,

	priv_key_file: PathBuf,
	pub_key_file: PathBuf,

	name_dir: PathBuf,
	files_dir: PathBuf,
}

impl FileConfig {
	/// Creates a new `FileConfig` instance with the specified base directory.
	///
	/// # Errors
	/// The base directory or its children cannot be created or accessed.
	pub(super) fn new(base_dir: &Path) -> Result<Self> {
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

		let files_dir = base_dir.join(FILES_DIR_NAME);
		if !files_dir.exists() {
			fs::create_dir_all(&files_dir)?;
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
			files_dir,
		})
	}

	/// The directory the `file` protocol serves by default.
	pub(super) fn file_root(&self) -> PathBuf {
		self.files_dir.clone()
	}

	/// Whether `key` may use `protocol` on this node.
	///
	/// Grants are read from the `auth/` directory:
	///
	/// * `auth/<endpoint-id>/<protocol>` as a file allows that protocol only. The name is
	///   parsed as a protocol name, so an alias (`auth/<id>/sftp`, `auth/<id>/tty`) works
	///   as well as the canonical name.
	/// * `auth/<endpoint-id>/all` allows every protocol.
	///
	/// An identity with no directory under `auth/` is granted nothing.
	pub(super) fn is_access_allowed(&self, key: &EndpointId, protocol: Protocol) -> bool {
		let entry = self.auth_dir.join(key.to_z32());
		let Ok(grants) = fs::read_dir(&entry) else {
			return false;
		};
		for grant in grants.filter_map(std::io::Result::ok) {
			if !grant.file_type().is_ok_and(|kind| kind.is_file()) {
				continue;
			}
			let Some(name) = grant.file_name().to_str().map(str::to_owned) else {
				continue;
			};
			if name == GRANT_ALL {
				return true;
			}
			match name.parse::<Protocol>() {
				Ok(granted) => {
					if granted == protocol {
						return true;
					}
				}
				// A grant file nobody can name is almost always a typo, and it silently
				// grants nothing. Say so, but only at debug level: this runs per connection.
				Err(err) => debug!("{err} in {}", entry.display()),
			}
		}
		false
	}

	/// Reads this node's identity.
	///
	/// Every failure is reported against the key file: a missing file and a corrupt one
	/// need different fixes, and the reader cannot tell them apart from the raw error.
	///
	/// # Errors
	/// The key file is missing, unreadable, not valid z-base32, or not the right length.
	pub(super) fn load_identity(&self) -> Result<SecretKey> {
		let key_file = dunce::simplified(&self.priv_key_file);
		let raw_key = read_to_string(&self.priv_key_file).wrap_err_with(|| {
			format!(
				"no usable identity in {} - run `pirohxy init` if you have not yet",
				key_file.display()
			)
		})?;
		let decoded = z32::decode(raw_key.trim().as_bytes())
			.wrap_err_with(|| format!("{} is not a valid z-base32 key", key_file.display()))?;
		let key_bytes: [u8; SECRET_KEY_LENGTH] = decoded
			.try_into()
			.map_err(|_| eyre!("{} holds an invalid privkey size", key_file.display()))?;
		Ok(SecretKey::from_bytes(&key_bytes))
	}

	pub(super) fn init_identity(&self) -> Result<()> {
		ensure!(!self.does_identity_exist(), "Identity already exists");
		let key = SecretKey::generate();
		let encoded = z32::encode(&key.to_bytes());
		write_private_key(&self.priv_key_file, encoded.as_bytes())?;
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
				&& name_str.len() < MAX_NAME_LEN
				&& !matches!(name_str, "." | "..")
				&& name_str
					.chars()
					.all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-')),
			"Name '{name_str}' is invalid. It must only use [a-zA-Z0-9._-], must not be `.` or `..`, and must be shorter than {MAX_NAME_LEN} characters",
		);
		let file_path = self.name_dir.join(name_str);
		let data = read_to_string(file_path)?;
		Ok(EndpointId::from_z32(data.trim())?)
	}
}

/// Writes the private key so that only the owning user can read it.
///
/// The mode is requested on `open` and then pinned with `set_permissions`, because the
/// creation mode is masked by the process umask: asking for `0600` under a permissive
/// umask still yields a group- or world-readable file on some systems.
#[cfg(unix)]
fn write_private_key(path: &Path, contents: &[u8]) -> Result<()> {
	use std::{
		fs::OpenOptions,
		io::Write,
		os::unix::fs::{OpenOptionsExt, PermissionsExt},
	};

	let mut file = OpenOptions::new()
		.write(true)
		.create_new(true)
		.mode(PRIVATE_KEY_MODE)
		.open(path)?;
	file.write_all(contents)?;
	file.flush()?;
	Ok(file.set_permissions(fs::Permissions::from_mode(PRIVATE_KEY_MODE))?)
}

/// Writes the private key with the platform default permissions.
///
/// Windows derives access from the ACL inherited from the parent directory, and there is
/// no umask to defeat, so the plain write is the closest equivalent available here.
#[cfg(not(unix))]
fn write_private_key(path: &Path, contents: &[u8]) -> Result<()> {
	Ok(fs::write(path, contents)?)
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
	use pirohxy::protoutils::alpn::Protocol;

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
		assert!(cfg.files_dir.is_dir());
		assert_eq!(cfg.file_root(), base.join("files"));
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
	fn access_is_allowed_only_for_grant_files_under_an_endpoint_directory() -> Result<()> {
		let base = fresh_base("access")?;
		let cfg = FileConfig::new(&base)?;
		let key = SecretKey::generate().public();

		assert!(!cfg.is_access_allowed(&key, Protocol::Socks));
		fs::create_dir_all(cfg.auth_dir.join(key.to_z32()))?;
		assert!(!cfg.is_access_allowed(&key, Protocol::Socks));

		fs::write(
			cfg.auth_dir.join(key.to_z32()).join(Protocol::Socks.name()),
			"",
		)?;
		assert!(cfg.is_access_allowed(&key, Protocol::Socks));

		let other = SecretKey::generate().public();
		fs::create_dir_all(cfg.auth_dir.join(other.to_z32()))?;
		assert!(!cfg.is_access_allowed(&other, Protocol::Socks));

		cleanup(&base)
	}

	#[test]
	fn a_bare_auth_file_grants_nothing() -> Result<()> {
		let base = fresh_base("bare-grant")?;
		let cfg = FileConfig::new(&base)?;
		let key = SecretKey::generate().public();

		// A file named after the peer rather than a directory of
		// grant files grants nothing, whatever it holds.
		fs::write(cfg.auth_dir.join(key.to_z32()), "a note the app ignores")?;

		for protocol in Protocol::ALL {
			assert!(
				!cfg.is_access_allowed(&key, protocol),
				"{protocol} must not be implied by a bare auth file"
			);
		}

		cleanup(&base)
	}

	#[test]
	fn per_protocol_grants_are_read_from_the_endpoint_directory() -> Result<()> {
		let base = fresh_base("per-protocol")?;
		let cfg = FileConfig::new(&base)?;
		let key = SecretKey::generate().public();
		let entry = cfg.auth_dir.join(key.to_z32());
		fs::create_dir_all(&entry)?;

		assert!(!cfg.is_access_allowed(&key, Protocol::Exec));

		fs::write(entry.join(Protocol::Exec.name()), "")?;
		assert!(cfg.is_access_allowed(&key, Protocol::Exec));
		assert!(!cfg.is_access_allowed(&key, Protocol::Shell));
		assert!(!cfg.is_access_allowed(&key, Protocol::Socks));

		cleanup(&base)
	}

	#[test]
	fn grant_files_may_use_a_protocol_alias() -> Result<()> {
		let base = fresh_base("grant-alias")?;
		let cfg = FileConfig::new(&base)?;
		let key = SecretKey::generate().public();
		let entry = cfg.auth_dir.join(key.to_z32());
		fs::create_dir_all(&entry)?;

		fs::write(entry.join("sftp"), "")?;
		fs::write(entry.join("tty"), "")?;
		assert!(cfg.is_access_allowed(&key, Protocol::File));
		assert!(cfg.is_access_allowed(&key, Protocol::Shell));
		assert!(!cfg.is_access_allowed(&key, Protocol::Exec));

		// A name no protocol answers to is a typo rather than a grant, and a directory
		// named after a protocol is not a grant either.
		fs::write(entry.join("sftpd"), "")?;
		fs::create_dir_all(entry.join(Protocol::Exec.name()))?;
		assert!(!cfg.is_access_allowed(&key, Protocol::Exec));

		cleanup(&base)
	}

	#[test]
	fn the_all_grant_opens_every_protocol() -> Result<()> {
		let base = fresh_base("all-grant")?;
		let cfg = FileConfig::new(&base)?;
		let key = SecretKey::generate().public();
		let entry = cfg.auth_dir.join(key.to_z32());
		fs::create_dir_all(&entry)?;
		fs::write(entry.join("all"), "")?;

		for protocol in Protocol::ALL {
			assert!(cfg.is_access_allowed(&key, protocol), "{protocol} denied");
		}

		cleanup(&base)
	}

	#[cfg(unix)]
	#[test]
	fn private_key_is_readable_only_by_its_owner() -> Result<()> {
		use std::os::unix::fs::PermissionsExt;

		let base = fresh_base("key-perms")?;
		let cfg = FileConfig::new(&base)?;
		cfg.init_identity()?;

		let mode = fs::metadata(base.join("self.priv"))?.permissions().mode();
		assert_eq!(mode & 0o777, 0o600);

		cleanup(&base)
	}

	#[cfg(unix)]
	#[test]
	fn loading_an_identity_tightens_a_loose_legacy_key() -> Result<()> {
		use std::os::unix::fs::PermissionsExt;

		let base = fresh_base("key-perms-repair")?;
		let cfg = FileConfig::new(&base)?;
		cfg.init_identity()?;

		let key_file = base.join("self.priv");
		fs::set_permissions(&key_file, fs::Permissions::from_mode(0o644))?;
		assert_eq!(fs::metadata(&key_file)?.permissions().mode() & 0o777, 0o644);

		cfg.load_identity()?;
		assert_eq!(fs::metadata(&key_file)?.permissions().mode() & 0o777, 0o600);

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

		fs::write(
			cfg.name_dir.join("prod_1"),
			format!("{}\n", endpoint.to_z32()),
		)?;
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
		assert!(cfg.resolve_server_name(".").is_err());
		assert!(cfg.resolve_server_name("..").is_err());
		assert!(cfg.resolve_server_name("a".repeat(64)).is_err());

		cleanup(&base)
	}
}
