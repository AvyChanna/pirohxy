use core::net::SocketAddr;

use color_eyre::eyre::{Result, eyre};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::net::TcpListener;
use tracing::{debug, info, trace};

use crate::{
	protocol::tunnel::{TunnelFailure, TunnelRequest, TunnelResponse},
	protoutils::{
		HostPort,
		stream::{ReadStructExt, WriteStructExt, copy_bidi_stream},
		tasks::Tasks,
	},
	shutdown::shutdown_handle,
};

/// Opens a forwarded TCP connection through the peer, mirroring `ssh -L`.
///
/// The returned streams carry the raw bytes of the connection the peer opened: write to
/// the [`SendStream`] to reach the target, read from the [`RecvStream`] to hear back
/// from it.
///
/// # Errors
/// The stream cannot be opened, the peer refuses the target, or the reply cannot be read.
pub async fn open(conn: &Connection, target: &HostPort) -> Result<(SendStream, RecvStream)> {
	let (mut send, mut recv) = conn.open_bi().await?;
	send.write_struct(&TunnelRequest {
		target: target.clone(),
	})
	.await?;

	match recv.read_struct::<TunnelResponse>().await? {
		TunnelResponse::Opened => Ok((send, recv)),
		TunnelResponse::Failed(TunnelFailure { reason }) => {
			Err(eyre!("tunnel to {target} refused: {reason}"))
		}
	}
}

/// Forwards `bind_addr` to `target` through the peer, mirroring `ssh -L`.
///
/// Every connection accepted on `bind_addr` becomes one stream on the peer, which the
/// peer then connects to `target`. Returns when the peer's connection dies or on the
/// first shutdown signal.
///
/// # Errors
/// The listener cannot bind, or a connection cannot be opened on the peer.
pub async fn serve(conn: &Connection, bind_addr: &SocketAddr, target: &HostPort) -> Result<()> {
	let listener = TcpListener::bind(bind_addr).await?;
	info!(
		"Forwarding {bind_addr} to {target} through {}",
		conn.remote_id().fmt_short()
	);

	// One task per accepted connection, tracked so the forward knows how many it has in
	// flight and can end them when it stops rather than leaving them copying on their own.
	let mut forwarded = Tasks::new("Tunnel");
	let closing = shutdown_handle().token();

	loop {
		trace!("Waiting for a connection on {bind_addr}");
		tokio::select! {
			accepted = listener.accept() => {
				let (socket, peer) = match accepted {
					Ok(accepted) => accepted,
					Err(err) => {
						info!("Failed to accept a connection on {bind_addr}: {err}");
						continue;
					}
				};
				debug!("Accepted {peer} for the forward to {target}");
				let conn = conn.clone();
				let target = target.clone();
				let closing = closing.clone();
				forwarded
					.spawn(async move {
						match open(&conn, &target).await {
							Ok((send, recv)) => {
								let (tcp_read, tcp_write) = socket.into_split();
								copy_bidi_stream(tcp_read, tcp_write, recv, send, &closing).await;
							}
							Err(err) => debug!("{err}"),
						}
					})
					.await;
			}
			() = closing.cancelled() => {
				info!("Received exit signal, closing the forward to {target}");
				forwarded.abort_all().await;
				return Ok(());
			}
		}
	}
}
