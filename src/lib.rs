pub mod config;
mod protocol;
mod stream;

use core::{fmt::Debug, net::SocketAddr};

use color_eyre::eyre::Result;
use iroh::{Endpoint, NodeId, SecretKey, Watcher, endpoint::VarInt, protocol::Router};
use tokio::{net::TcpListener, signal};
use tracing::{debug, info, warn};

use config::Auth;
use protocol::Socks;
use stream::copy_bidi_stream;

const ALPN: &[u8] = b"/pirohxy/socks";

pub async fn start_egress<T>(self_key: SecretKey, cfg: T) -> Result<()>
where
	T: Auth + Debug + Send + Sync + 'static,
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
	let endpoint = start_iroh_node(self_key).await?;

	loop {
		info!("Waiting for client connection on {}", bind_addr);

		tokio::select! {
			client_socket_res = client_listener.accept() => {
				let endpoint2 = endpoint.clone();
				match client_socket_res {
					Ok((client_socket, _)) => {
						debug!("Accepted client connection at {}", client_socket.peer_addr()?);
						let _task = tokio::spawn(async move {
							let conn = endpoint2.connect(server_key, ALPN).await?;

							let (to1, from1) = conn.open_bi().await?;
							let (from2, to2) = client_socket.into_split();

							copy_bidi_stream(from2, to2, from1, to1).await?;
							conn.close(VarInt::from_u32(1), b"BiDi exit");
							Result::<()>::Ok(())
						});
					}
					Err(e) => {
						warn!("Failed to accept client connection: {}", e);
					}
				}
			}
			_ = signal::ctrl_c() => {
				info!("Received Ctrl+C, exiting process loop.");
				endpoint.close().await;
				return Ok(());
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
	let _relay_url = endpoint.home_relay().initialized().await;
	Ok(endpoint)
}
