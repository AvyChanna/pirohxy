use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, ensure};
use tracing::{debug, level_filters::LevelFilter};
use tracing_subscriber::{EnvFilter, fmt::time::LocalTime};

use piroxy::{
	bind_and_connect,
	config::{
		auther::FileBasedAuther,
		default_config_path,
		identity::{FileBasedIdentity, IdentityManager},
		resolver::{FileBasedNameResolver, NameResolver},
	},
	start_egress,
};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
	#[command(subcommand)]
	command: Commands,

	#[arg(short = 'c', long, value_name = "CONFIG-DIR", global = true)]
	config: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
	#[command(about = "Run egress")]
	Egress,
	#[command(about = "Connect to an egress server")]
	Connect {
		#[arg(help = "server name")]
		server: String,
		#[arg(short='b', long, default_value_t = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))]
		bind_addr: SocketAddr,
	},
	#[command(about = "Initialize client identity")]
	Init,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
	color_eyre::install().expect("Unable to install global error handler");

	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::builder()
				.with_default_directive(LevelFilter::INFO.into())
				.from_env_lossy(),
		)
		.with_timer(LocalTime::rfc_3339())
		// .pretty()
		.init();

	let cli = Cli::parse();
	debug!(args = ?cli, "Cli args");
	let cfg_dir = cli.config.map_or_else(default_config_path, PathBuf::from);
	ensure!(
		cfg_dir.exists(),
		"Config directory does not exist: {}",
		cfg_dir.display()
	);
	ensure!(
		cfg_dir.is_dir(),
		"Config path is not a directory: {}",
		cfg_dir.display()
	);
	ensure!(
		!cfg_dir.is_symlink(),
		"Config path cannot be a symlink: {}",
		cfg_dir.display()
	);

	match cli.command {
		Commands::Egress => {
			let identity = FileBasedIdentity::new(cfg_dir.clone())?;
			let self_key = identity.load()?;
			let auther = FileBasedAuther::new(cfg_dir.clone())?;
			start_egress(self_key, auther).await
		}
		Commands::Connect {
			server: server_name,
			bind_addr,
		} => {
			let identity = FileBasedIdentity::new(cfg_dir.clone())?;
			let self_key = identity.load()?;
			let resolver = FileBasedNameResolver::new(cfg_dir.clone())?;
			let server_key = resolver.resolve(server_name)?;
			bind_and_connect(self_key, server_key, &bind_addr).await
		}
		Commands::Init => {
			let identity = FileBasedIdentity::new(cfg_dir.clone())?;
			ensure!(!identity.exists(), "Identity already exists");
			identity.generate()?;
			Ok(())
		}
	}
}
