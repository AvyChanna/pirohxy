use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{OptionExt, Result, bail, ensure};
use directories::ProjectDirs;
use iroh::node_info::NodeIdExt;
use tracing::{debug, level_filters::LevelFilter};
use tracing_subscriber::{EnvFilter, fmt::time::LocalTime};

use pirohxy::{
	bind_and_connect,
	config::{
		FileBasedAuth, FileBasedIdentity, FileBasedNameResolver, IdentityManager, NameResolver,
	},
	start_egress,
};

const QUALIFIER_NAME: &str = "";
const ORGANIZATION_NAME: &str = "avychanna";
const APPLICATION_NAME: &str = "pirohxy";

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
	#[command(subcommand)]
	command: Commands,

	#[arg(short = 'c', long, value_name = "CONFIG-DIR", global = true)]
	config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
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
	#[command(about = "Print current identity")]
	Identity,
}

impl Cli {
	fn get_config_path(&self) -> Result<PathBuf> {
		let cfg_dir = match &self.config {
			Some(path) => path.clone(),
			None => ProjectDirs::from(QUALIFIER_NAME, ORGANIZATION_NAME, APPLICATION_NAME)
				.ok_or_eyre("Could not find project directories")?
				.config_local_dir()
				.to_path_buf(),
		};
		Ok(cfg_dir.canonicalize()?)
	}
}

fn ensure_cfg_path(cfg_dir: &Path) -> Result<()> {
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
	Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
	color_eyre::install()?;

	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::builder()
				.with_default_directive(LevelFilter::INFO.into())
				.from_env_lossy(),
		)
		.with_timer(LocalTime::rfc_3339())
		.init();

	let cli = Cli::parse();
	debug!(args = ?cli, "Cli args");

	let cfg_dir = cli.get_config_path()?;

	match &cli.command {
		Commands::Egress => {
			ensure_cfg_path(&cfg_dir)?;
			let identity = FileBasedIdentity::new(&cfg_dir)?;
			let auth = FileBasedAuth::new(&cfg_dir)?;
			let self_key = identity.load()?;
			start_egress(self_key, auth).await
		}
		Commands::Connect {
			server: server_name,
			bind_addr,
		} => {
			ensure_cfg_path(&cfg_dir)?;
			let identity = FileBasedIdentity::new(&cfg_dir)?;
			let resolver = FileBasedNameResolver::new(&cfg_dir)?;
			let self_key = identity.load()?;
			let server_key = resolver.resolve(server_name)?;
			bind_and_connect(self_key, server_key, bind_addr).await
		}
		Commands::Init => {
			ensure_cfg_path(&cfg_dir)?;
			let identity = FileBasedIdentity::new(&cfg_dir)?;
			identity.generate()
		}
		Commands::Identity => {
			let identity = FileBasedIdentity::new(&cfg_dir)?;
			match identity.load() {
				Ok(self_key) => {
					#[expect(clippy::print_stdout, reason = "user requested data on stdout")]
					{
						println!("{}", self_key.public().to_z32());
					}
					Ok(())
				}
				Err(_) => {
					bail!("No/Invalid identity found. Did you run `init`?")
				}
			}
		}
	}
}
