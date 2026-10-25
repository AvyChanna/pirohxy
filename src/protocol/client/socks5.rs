use core::{net::SocketAddr, time::Duration};

use color_eyre::eyre::Result;
use iroh::{EndpointId, SecretKey};
use tokio::{net::TcpListener, time::timeout};
use tracing::{debug, info, trace};

use crate::{
	protocol::client::connect::{self, Client},
	protoutils::{alpn::Protocol, stream::copy_bidi_stream, tasks::Tasks},
	shutdown::shutdown_handle,
};

/// Binds a TCP listener and proxies it through a server, reconnecting when the peer dies.
///
/// The SOCKS listener outlives the connection it proxies over: when the peer's connection
/// closes, the same node dials the server again - same client identity, so the grant the
/// server already gave it still applies - and the listener keeps accepting.
///
/// # Errors
/// The listener fails to bind, or the process is asked to exit while reconnecting.
pub async fn bind_and_connect(
	master_key: SecretKey,
	server_id: EndpointId,
	bind_addr: &SocketAddr,
	conn_timeout: Duration,
) -> Result<()> {
	let client_listener = TcpListener::bind(bind_addr).await?;
	let mut client = Client::connect(master_key, server_id, Protocol::Socks).await?;

	// Every proxied client connection runs as its own task, tracked here so the loop can
	// say how many it has taken on and so an exit signal ends them instead of leaving a
	// half-proxied connection running until the process does.
	let mut proxied = Tasks::new("SOCKS");
	let closing = shutdown_handle().token();

	loop {
		trace!("Waiting for client connection on {}", bind_addr);

		// Re-read every round: a reconnect swaps the connection out from under the loop.
		let conn = client.conn().clone();
		let died = conn.closed();
		tokio::pin!(died);

		tokio::select! {
			// Poll the dead connection before the listener, so a peer that went away is
			// noticed instead of accepted onto.
			biased;
			err = &mut died => {
				info!(
					"Connection to server {} was closed: {err}",
					server_id.fmt_short()
				);
				// Streams already open on a dead connection are finished copying; only the
				// ones still handshaking are worth ending early.
				proxied.abort_all().await;
				if connect::reconnect(&mut client, Protocol::Socks, &closing).await.is_err() {
					break;
				}
			}
			client_socket_res = client_listener.accept() => {
				match client_socket_res {
					Ok((client_socket, _)) => {
						debug!("Accepted client connection at {}", client_socket.peer_addr()?);
						let conn_clone = conn.clone();
						let closing = closing.clone();
						proxied
							.spawn(async move {
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

								copy_bidi_stream(from2, to2, from1, to1, &closing).await;
							})
							.await;
					}
					Err(e) => {
						info!("Failed to accept client connection: {}", e);
					}
				}
			}
			() = closing.cancelled() => {
				info!("Received exit signal, exiting process loop");
				break;
			}
		}
	}

	proxied.abort_all().await;
	client.close().await;
	Ok(())
}
