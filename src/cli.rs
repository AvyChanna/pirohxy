use core::{
	net::{IpAddr, Ipv4Addr, SocketAddr},
	time::Duration,
};
use std::{fs, path::PathBuf};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{OptionExt, Result, ensure};
use directories::ProjectDirs;

use parser::parse_duration;

const ORGANIZATION_NAME: &str = "avychanna";
const APPLICATION_NAME: &str = "pirohxy";

#[derive(Parser, Debug)]
#[command(about, version, long_version=include_str!(concat!(env!("OUT_DIR"), "/long-help.txt")))]
pub(super) struct Cli {
	#[command(subcommand)]
	pub(super) command: Commands,

	#[arg(short = 'c', long, value_name = "CONFIG-DIR", global = true)]
	config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub(super) enum ConfigGetter {
	Path,
	Identity {
		#[arg(help = "identity name")]
		identity_name: Option<String>,
	},
}
#[derive(Subcommand, Debug)]
pub(super) enum Commands {
	#[command(about = "Run egress")]
	Egress {
		#[arg(long, default_value = "5s", value_parser = parse_duration)]
		server_req_timeout: Duration,
	},
	#[command(about = "Connect to an egress server")]
	Connect {
		#[arg(help = "server name, use `:node_id` to connect directly")]
		server: String,
		#[arg(short='b', long, default_value_t = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))]
		bind_addr: SocketAddr,
		#[arg(long, default_value = "10s", value_parser = parse_duration)]
		conn_timeout: Duration,
	},
	#[command(about = "Initialize an identity")]
	Init,
	#[command(about = "Print current configuration")]
	Conf {
		#[command(subcommand)]
		getter: ConfigGetter,
	},
}

impl Cli {
	/// Returns the path to the configuration directory.
	/// If the `--config` argument is provided, it returns that path.
	/// Otherwise, it uses the default project directories.
	pub(super) fn get_config_path(&self) -> Result<PathBuf> {
		let cfg_dir = match &self.config {
			Some(path) => path.clone(),
			None => ProjectDirs::from("", ORGANIZATION_NAME, APPLICATION_NAME)
				.ok_or_eyre("Could not find project directories")?
				.config_local_dir()
				.to_path_buf(),
		};
		if !cfg_dir.exists() {
			fs::create_dir_all(&cfg_dir)?;
		}

		ensure!(
			cfg_dir.is_dir(),
			"Config path is not a directory: {}",
			cfg_dir.display()
		);
		Ok(cfg_dir.canonicalize()?)
	}
}

mod parser {
	use core::{num::ParseIntError, time::Duration};

	#[derive(Debug, PartialEq, thiserror::Error)]
	pub(crate) enum Error {
		#[error("invalid number format: {error}")]
		ParseErr { error: ParseIntError },
		#[error("minute to second conversion overflowed")]
		Overflow,
		#[error("duration string is empty")]
		Empty,
	}

	pub(crate) fn parse_duration(string: &str) -> Result<Duration, Error> {
		if string == "0" {
			return Ok(Duration::from_secs(0));
		}
		if string.is_empty() {
			return Err(Error::Empty);
		}

		if let Some(stripped) = string.strip_suffix('s') {
			let dur_num: u64 = stripped.parse().map_err(|e| Error::ParseErr { error: e })?;
			return Ok(Duration::from_secs(dur_num));
		}

		if let Some(stripped) = string.strip_suffix('m') {
			let dur_num: u64 = stripped.parse().map_err(|e| Error::ParseErr { error: e })?;
			let dur_sec = dur_num.checked_mul(60).ok_or(Error::Overflow)?;
			return Ok(Duration::from_secs(dur_sec));
		}

		let dur_num: u64 = string.parse().map_err(|e| Error::ParseErr { error: e })?;
		Ok(Duration::from_secs(dur_num))
	}
}

#[cfg(test)]
mod tests {
	use core::{
		net::{IpAddr, Ipv4Addr, SocketAddr},
		time::Duration,
	};
	use std::{fs, path::PathBuf};

	use clap::Parser;
	use color_eyre::eyre::Result;

	use super::{
		Cli, Commands, ConfigGetter,
		parser::{Error, parse_duration},
	};

	fn test_config_path(name: &str) -> PathBuf {
		let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
		path.push("target");
		path.push("test-data");
		path.push(format!("cli-{name}-{}", std::process::id()));
		path
	}

	#[test]
	fn parses_duration_seconds_minutes_and_plain_seconds() {
		assert_eq!(parse_duration("0"), Ok(Duration::from_secs(0)));
		assert_eq!(parse_duration("15"), Ok(Duration::from_secs(15)));
		assert_eq!(parse_duration("15s"), Ok(Duration::from_secs(15)));
		assert_eq!(parse_duration("2m"), Ok(Duration::from_mins(2)));
	}

	#[test]
	fn rejects_empty_invalid_and_overflowing_durations() {
		assert_eq!(parse_duration(""), Err(Error::Empty));
		assert!(matches!(
			parse_duration("not-a-duration"),
			Err(Error::ParseErr { .. })
		));
		assert!(matches!(
			parse_duration("18446744073709551615000"),
			Err(Error::ParseErr { .. })
		));
		assert_eq!(
			parse_duration("18446744073709551615m"),
			Err(Error::Overflow)
		);
	}

	#[test]
	fn parses_connect_defaults() -> Result<()> {
		let cli = Cli::try_parse_from(["pirohxy", "connect", "server-a"])?;

		let Commands::Connect {
			server,
			bind_addr,
			conn_timeout,
		} = cli.command
		else {
			panic!("expected connect command");
		};

		assert_eq!(server, "server-a");
		assert_eq!(
			bind_addr,
			SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
		);
		assert_eq!(conn_timeout, Duration::from_secs(10));
		Ok(())
	}

	#[test]
	fn parses_config_getters() -> Result<()> {
		let path_cli = Cli::try_parse_from(["pirohxy", "conf", "path"])?;
		assert!(matches!(
			path_cli.command,
			Commands::Conf {
				getter: ConfigGetter::Path
			}
		));

		let identity_cli = Cli::try_parse_from(["pirohxy", "conf", "identity", "main"])?;
		assert!(matches!(
			identity_cli.command,
			Commands::Conf {
				getter: ConfigGetter::Identity {
					identity_name: Some(ref name)
				}
			} if name == "main"
		));
		Ok(())
	}

	#[test]
	fn get_config_path_creates_and_canonicalizes_explicit_directory() -> Result<()> {
		let path = test_config_path("explicit");
		if path.exists() {
			fs::remove_dir_all(&path)?;
		}

		let cli = Cli::try_parse_from([
			"pirohxy",
			"--config",
			path.to_str().expect("test path should be utf-8"),
			"init",
		])?;
		let actual = cli.get_config_path()?;

		assert!(actual.is_absolute());
		assert!(actual.is_dir());
		assert_eq!(actual, path.canonicalize()?);

		fs::remove_dir_all(path)?;
		Ok(())
	}
}
