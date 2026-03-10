mod authenticated_router;
pub mod protocol;
mod shutdown;
mod stream;

use core::net::SocketAddr;

use color_eyre::eyre::Result;
use iroh::{Endpoint, EndpointId, SecretKey};
use tokio::net::TcpListener;
use tracing::{debug, info, trace};

use authenticated_router::AuthenticatedRouterBuilder;
use protocol::socks::Socks;
use shutdown::shutdown_signal;
use stream::copy_bidi_stream;

/// The ALPN (Application-Layer Protocol Negotiation) identifier for the iroh socks protocol.
/// This is used to identify the protocol.
/// Convention: namespace/app/proto/version
const ALPN: &[u8] = b"/avychanna/pirohxy/socks/v1";

/// Starts the egress proxy server with the given identity and configuration.
///
/// # Errors
/// The server fails to start, or if there is an error during the shutdown process.
pub async fn start_egress<T>(self_key: SecretKey, auther: T) -> Result<()>
where
	T: Fn(EndpointId) -> bool + Send + Sync + 'static + Clone,
{
	debug!("Serving as node {}", self_key.public().fmt_short());
	let endpoint = start_iroh_node(self_key).await?;

	let router = AuthenticatedRouterBuilder::new(endpoint.clone(), auther)
		.accept(ALPN, Socks::new())
		.spawn();

	shutdown_signal().await?;

	Ok(router.shutdown().await?)
}

/// Binds a TCP listener and connects to a server using the provided keys and address.
///
/// # Errors
/// The listener fails to bind, or there is an error during the connection process.
pub async fn bind_and_connect(
	self_key: SecretKey,
	server_key: EndpointId,
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
		trace!("Waiting for client connection on {}", bind_addr);

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
							conn.close(1u32.into(), b"BiDi exit");
							Result::<()>::Ok(())
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
	let endpoint = Endpoint::builder().secret_key(key).bind().await?;
	endpoint.online().await;
	Ok(endpoint)
}
