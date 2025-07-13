use core::{fmt::Debug, net::SocketAddr};

use color_eyre::eyre::{Context, Result};
use config::auther::Authenticator;
use iroh::{Endpoint, NodeId, SecretKey, Watcher, protocol::Router};
use tokio::{net::TcpListener, signal};
use tracing::{debug, info, warn};

use crate::protocol::{ALPN, client::handle_req, iroh::Socks};

pub mod config;
mod protocol;
mod stream;

pub async fn start_egress<T>(self_key: SecretKey, cfg: T) -> Result<()>
where
	T: Authenticator + Debug + Send + Sync + 'static,
{
	debug!("Serving as node {}", self_key.public().fmt_short());
	let endpoint = start_iroh_node(self_key).await?;

	let iroh_socks = Socks::new(cfg);
	let router = Router::builder(endpoint.clone())
		.accept(ALPN, iroh_socks)
		.spawn();

	signal::ctrl_c().await?;

	Ok(router.shutdown().await?)
}

pub async fn bind_and_connect(
	self_key: SecretKey,
	server_key: NodeId,
	bind_addr: &SocketAddr,
) -> Result<()> {
	let client_listener = TcpListener::bind(bind_addr).await?;

	debug!(
		"Dialing server {} as client {}",
		server_key.fmt_short(),
		self_key.public().fmt_short()
	);
	let endpoint = start_iroh_node(self_key)
		.await
		.wrap_err_with(|| "Failed to start iroh node")?;

	loop {
		info!("Waiting for client connection on {}", bind_addr);

		tokio::select! {
			_ = signal::ctrl_c() => {
				info!("Received Ctrl+C, exiting process loop.");
				return Ok(());
			}
			res = handle_req(endpoint.clone(), server_key, &client_listener) => {
				if let Err(e) = res {
					warn!("Error in inner loop: {}", e);
				}
			}
		}
	}
}

async fn start_iroh_node(key: SecretKey) -> Result<Endpoint> {
	debug!("Starting iroh node as {}", key.public().fmt_short());
	let endpoint = Endpoint::builder()
		.secret_key(key)
		.discovery_n0()
		.bind()
		.await?;
	let _relay_url = endpoint.home_relay().initialized().await?;
	Ok(endpoint)
}
