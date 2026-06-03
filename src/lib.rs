mod auth;
pub mod protocol;
pub mod protoutils;
mod shutdown;
use core::net::SocketAddr;

use color_eyre::eyre::Result;
use iroh::{Endpoint, EndpointId, SecretKey, endpoint::presets, protocol::RouterBuilder};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, trace};

use auth::{AuthenticatedConnection, AuthenticatedRouterBuilder};
use protocol::socks::Socks;
use protoutils::stream::copy_bidi_stream;
use shutdown::shutdown_signal;

/// The ALPN (Application-Layer Protocol Negotiation) identifier for the iroh socks protocol.
/// This is used to identify the protocol.
/// Convention: namespace/app/proto/version
const ALPN: &[u8] = b"/avychanna/pirohxy/socks/v1";

/// Starts the egress proxy server with the given identity and configuration.
///
/// # Errors
/// The server fails to start, or if there is an error during the shutdown process.
pub async fn start_egress<T>(
	self_key: SecretKey,
	auther: T,
	server_req_timeout: Duration,
) -> Result<()>
where
	T: Fn(EndpointId) -> bool + Send + Sync + 'static,
{
	debug!("Serving as node {}", self_key.public().fmt_short());
	let endpoint = start_iroh_node(self_key).await?;

	let router = RouterBuilder::new(endpoint.clone())
		.accept_with_auth(ALPN, Socks::new(server_req_timeout), auther)
		.spawn();

	shutdown_signal().await?;

	Ok(router.shutdown().await?)
}

/// Binds a TCP listener and connects to a server using the provided keys and address.
///
/// # Errors
/// The listener fails to bind, or there is an error during the connection process.
pub async fn bind_and_connect(
	master_key: SecretKey,
	server_id: EndpointId,
	bind_addr: &SocketAddr,
	conn_timeout: Duration,
) -> Result<()> {
	let client_listener = TcpListener::bind(bind_addr).await?;
	let client_key = SecretKey::generate();
	debug!(
		"Dialing server {} as client {}",
		server_id.fmt_short(),
		client_key.public().fmt_short()
	);
	let endpoint = start_iroh_node(client_key).await?;
	let conn = endpoint
		.connect_with_auth(server_id, ALPN, master_key)
		.await?;

	loop {
		trace!("Waiting for client connection on {}", bind_addr);

		tokio::select! {
			client_socket_res = client_listener.accept() => {
				match client_socket_res {
					Ok((client_socket, _)) => {
						debug!("Accepted client connection at {}", client_socket.peer_addr()?);
						let conn_clone = conn.clone();
						let _task = tokio::spawn(async move {
							trace!("Opening bi-directional stream to server {}", server_id.fmt_short());
							let (to1, from1) = match timeout(conn_timeout, conn_clone.open_bi()).await {
								Ok(Ok(streams)) => streams,
								Ok(Err(err)) => {
									debug!("Failed to open bi-directional stream to server {}: {}", server_id.fmt_short(), err);
									return;
								}
								Err(_) => {
									debug!("Timed out opening bi-directional stream to server {}", server_id.fmt_short());
									return;
								}
							};
							trace!("Bi-directional stream to server {} opened", server_id.fmt_short());
							let (from2, to2) = client_socket.into_split();

							copy_bidi_stream(from2, to2, from1, to1).await;
						});
					}
					Err(e) => {
						info!("Failed to accept client connection: {}", e);
					}
				}
			}
			_ = shutdown_signal() => {
				info!("Received exit signal, exiting process loop");
				endpoint.close().await;
				return Ok(());
			}
		}
	}
}

/// Starts an iroh node with the given secret key and returns the endpoint.
///
/// # Errors
/// The node fails to start or there is an error during initialization.
async fn start_iroh_node(key: SecretKey) -> Result<Endpoint> {
	debug!("Starting iroh node as {}", key.public().fmt_short());
	let endpoint = Endpoint::builder(presets::N0)
		.secret_key(key)
		.bind()
		.await?;
	endpoint.online().await;
	Ok(endpoint)
}
