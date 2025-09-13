use clap::Parser;
use color_eyre::eyre::{Result, bail};
use iroh::node_info::NodeIdExt;
use tracing::{debug, level_filters::LevelFilter};
use tracing_subscriber::{EnvFilter, fmt::time::LocalTime};

mod cli;
mod cfg;
use cfg::FileConfig;
use cli::Cli;
use pirohxy::{bind_and_connect, start_egress};

use crate::cli::{Commands, ConfigGetter};

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
	let cfg = FileConfig::new(&cfg_dir)?;

	match &cli.command {
		Commands::Egress => {
			let self_key = cfg.load_identity()?;
			start_egress(self_key, move |x| cfg.is_access_allowed(&x)).await
		}
		Commands::Connect {
			server: server_name,
			bind_addr,
		} => {
			let self_key = cfg.load_identity()?;
			let server_key = cfg.resolve_server_name(server_name)?;
			bind_and_connect(self_key, server_key, bind_addr).await
		}
		Commands::Init => cfg.init_identity(),
		Commands::Conf { getter } => match getter {
			ConfigGetter::Path => {
				#[expect(clippy::print_stdout, reason = "user requested data on stdout")]
				{
					println!("{}", dunce::simplified(&cfg_dir).display());
				}
				Ok(())
			}
			ConfigGetter::Identity { identity_name } => match identity_name {
				None => match cfg.load_identity() {
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
				},
				Some(name) => {
					let node_id = cfg.resolve_server_name(name)?;
					#[expect(clippy::print_stdout, reason = "user requested data on stdout")]
					{
						println!("{}", node_id.to_z32());
					}
					Ok(())
				}
			},
		},
	}
}
