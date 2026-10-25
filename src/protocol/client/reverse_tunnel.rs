use color_eyre::eyre::{Result, eyre};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::{
	protocol::reverse_tunnel::{
		ReverseTunnelChannel, ReverseTunnelFailure, ReverseTunnelNotice, ReverseTunnelRequest,
		ReverseTunnelResponse,
	},
	protoutils::{
		HostPort,
		stream::{ReadStructExt, WriteStructExt, copy_bidi_stream},
		tasks::Tasks,
	},
	shutdown::shutdown_handle,
};

/// A forward the peer listens on for us, mirroring `ssh -R`.
#[derive(Debug)]
pub struct Forward {
	/// The address we asked the peer to listen on.
	pub bind: HostPort,
	/// The address the peer actually bound to.
	pub bound: HostPort,
	/// Left open on purpose: the peer releases the listener when this stream ends.
	keepalive: SendStream,
}

impl Forward {
	/// Releases the peer's listener by ending the stream that keeps it alive.
	///
	/// # Errors
	/// The keepalive stream cannot be finished.
	pub fn close(mut self) -> Result<()> {
		Ok(self.keepalive.finish()?)
	}
}

/// Asks the peer to listen on `bind` and hand every accepted connection back to us.
///
/// # Errors
/// The stream cannot be opened, the peer refuses the forward, or the reply cannot be read.
pub async fn listen(conn: &Connection, bind: &HostPort) -> Result<Forward> {
	let mut keepalive = conn.open_uni().await?;
	keepalive
		.write_struct(&ReverseTunnelRequest { bind: bind.clone() })
		.await?;

	let mut notice = conn.accept_uni().await?;
	match notice.read_struct::<ReverseTunnelNotice>().await? {
		ReverseTunnelNotice::Bound(ReverseTunnelResponse { bind, bound }) => Ok(Forward {
			bind,
			bound,
			keepalive,
		}),
		ReverseTunnelNotice::Failed(ReverseTunnelFailure { bind, reason }) => {
			Err(eyre!("forward for {bind} refused: {reason}"))
		}
	}
}

/// One connection the peer accepted on its side of a forward.
#[derive(Debug)]
pub struct ForwardedConnection {
	/// Which requested forward this connection belongs to.
	pub bind: HostPort,
	send: SendStream,
	recv: RecvStream,
}

impl ForwardedConnection {
	/// Takes the raw streams instead of bridging them somewhere.
	#[must_use]
	pub fn into_streams(self) -> (SendStream, RecvStream) {
		(self.send, self.recv)
	}

	/// Copies this connection to a TCP address until either side closes.
	///
	/// `closing` ends the copy when the process is asked to exit.
	///
	/// # Errors
	/// The local address cannot be resolved or connected to.
	pub async fn bridge_to(self, local: &HostPort, closing: &CancellationToken) -> Result<()> {
		let addr = local.resolve_first().await?;
		let stream = TcpStream::connect(addr).await?;
		let (tcp_read, tcp_write) = stream.into_split();
		copy_bidi_stream(tcp_read, tcp_write, self.recv, self.send, closing).await;
		Ok(())
	}
}

/// Waits for the next connection the peer accepted for one of our forwards.
///
/// # Errors
/// The stream cannot be accepted, or its header frame cannot be read.
pub async fn accept(conn: &Connection) -> Result<ForwardedConnection> {
	let (send, mut recv) = conn.accept_bi().await?;
	let ReverseTunnelChannel { bind } = recv.read_struct().await?;
	Ok(ForwardedConnection { bind, send, recv })
}

/// Asks the peer to listen on `bind` and bridges everything it accepts to `local`,
/// mirroring `ssh -R`.
///
/// Returns when the peer drops the connection or on the first shutdown signal, which
/// also releases the peer's listener. Bridged connections that are still copying when
/// this returns are given the drain grace period, then reset.
///
/// # Errors
/// The peer refuses the forward, or a forwarded connection cannot be accepted.
#[expect(
	let_underscore_drop,
	reason = "Releasing the forward is best-effort: the peer may already have dropped it"
)]
pub async fn serve(conn: &Connection, bind: &HostPort, local: &HostPort) -> Result<()> {
	let forward = listen(conn, bind).await?;
	info!(
		"Forward {} on the server is bridged to {local}",
		forward.bound
	);

	let mut bridged = Tasks::new("ReverseTunnel");
	let closing = shutdown_handle().token();

	loop {
		tokio::select! {
			accepted = accept(conn) => match accepted {
				Ok(connection) => {
					let local = local.clone();
					let closing = closing.clone();
					bridged
						.spawn(async move {
							if let Err(err) = connection.bridge_to(&local, &closing).await {
								debug!("Failed to bridge a connection to {local}: {err}");
							}
						})
						.await;
				}
				Err(err) => {
					info!("The forward for {bind} is gone: {err}");
					break;
				}
			},
			() = closing.cancelled() => {
				info!("Received exit signal, releasing the forward for {bind}");
				bridged.abort_all().await;
				break;
			}
		}
	}

	bridged.drain().await;
	let _ = forward.close();
	Ok(())
}
