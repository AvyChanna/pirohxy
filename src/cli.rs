use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{fs, path::PathBuf};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{OptionExt, Result, ensure};
use directories::ProjectDirs;

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
	Egress,
	#[command(about = "Connect to an egress server")]
	Connect {
		#[arg(help = "server name, use `:node_id` to connect directly")]
		server: String,
		#[arg(short='b', long, default_value_t = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))]
		bind_addr: SocketAddr,
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
