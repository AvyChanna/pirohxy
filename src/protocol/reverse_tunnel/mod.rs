use core::fmt::Debug;

use color_eyre::eyre::{Result, eyre};
use iroh::{
	endpoint::{Connection, RecvStream},
	protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::{
	protoutils::{
		HostPort,
		accept::serve_uni_streams,
		stream::{ReadStructExt, WriteStructExt, copy_bidi_stream},
		tasks::Tasks,
	},
	shutdown::shutdown_handle,
};

/// Asks the server to listen on `bind` and forward every accepted connection back to
/// the client, mirroring SSH's `forwarded-tcpip` (`ssh -R`) channel.
///
/// Sent on a unidirectional stream that the client deliberately leaves unfinished: the
/// stream staying open is what keeps the remote forward alive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseTunnelRequest {
	/// Address the server should listen on.
	pub bind: HostPort,
}

/// Tells the client which address the server actually bound to.
///
/// `bind` echoes the request so a client holding several forwards can match the reply,
/// while `bound` reports the real address (useful when port `0` was requested).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseTunnelResponse {
	/// The requested bind address, echoed back for matching.
	pub bind: HostPort,
	/// The address the listener is actually bound to.
	pub bound: HostPort,
}

/// Reports that the server could not listen on the requested address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseTunnelFailure {
	/// The requested bind address.
	pub bind: HostPort,
	/// Human readable reason the forward was refused.
	pub reason: String,
}

/// The server's reply to a reverse tunnel request.
///
/// Both outcomes travel as one frame so the client can tell them apart: postcard frames
/// are not self-describing, so two separate structs would have to be guessed at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReverseTunnelNotice {
	/// The server is listening, `bound` reports the address it chose.
	Bound(ReverseTunnelResponse),
	/// The server refused the forward.
	Failed(ReverseTunnelFailure),
}

/// Header written on the first frame of every forwarded connection, so the client can
/// demultiplex streams that belong to different forwards on the same connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseTunnelChannel {
	/// Which requested forward this stream belongs to.
	pub bind: HostPort,
}

/// A `ReverseTunnel` protocol handler for Iroh that implements the `ProtocolHandler` trait.
/// It allows a client to request that the server binds to a local address and forward
/// all traffic to the Iroh stream.
#[derive(Debug, Copy, Clone, Default)]
pub struct ReverseTunnel;

impl ReverseTunnel {
	/// Creates a new `ReverseTunnel` instance.
	#[must_use]
	pub fn new() -> Self {
		Self {}
	}
}

impl ProtocolHandler for ReverseTunnel {
	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		// The handler opens a listener and then dials back over the same connection, so it
		// needs its own handle on it while this loop keeps accepting.
		let dialer = conn.clone();
		serve_uni_streams(&conn, "ReverseTunnel", move |r| {
			handle_reverse_tunnel(dialer.clone(), r)
		})
		.await
	}
}

async fn handle_reverse_tunnel(conn: Connection, mut request_stream: RecvStream) -> Result<()> {
	let ReverseTunnelRequest { bind } = request_stream.read_struct().await?;
	debug!("Reverse Tunnel request to bind to {bind}");

	let bind_addr = match bind.resolve_first().await {
		Ok(addr) => addr,
		Err(err) => {
			return report_failure(&conn, &bind, &format!("cannot resolve {bind}: {err}")).await;
		}
	};

	let listener = match TcpListener::bind(bind_addr).await {
		Ok(listener) => listener,
		Err(err) => {
			return report_failure(&conn, &bind, &format!("cannot bind {bind}: {err}")).await;
		}
	};
	let bound = HostPort::from(listener.local_addr()?);
	debug!("Listening on {bound} for reverse tunnel {bind}");

	send_notice(
		&conn,
		&ReverseTunnelNotice::Bound(ReverseTunnelResponse {
			bind: bind.clone(),
			bound: bound.clone(),
		}),
	)
	.await?;

	// The request stream is the lifetime handle for this forward: as soon as the client
	// drops it (or the connection dies), the listener is released and the connections it
	// already accepted get the drain grace period before they are reset.
	let mut keepalive = [0u8; 1];
	let mut forwarded = Tasks::new("ReverseTunnel");
	// Cloned into every accepted connection, so an exit signal ends the forwards this one
	// is carrying rather than leaving them copying.
	let closing = shutdown_handle().token();
	loop {
		tokio::select! {
			biased;
			res = request_stream.read(&mut keepalive) => {
				match res {
					Ok(None) | Err(_) => {
						debug!("Reverse tunnel {bind} closed by the client");
						break;
					}
					Ok(Some(_)) => {}
				}
			}
			accepted = listener.accept() => {
				let (tcp_stream, peer) = match accepted {
					Ok(accepted) => accepted,
					Err(err) => {
						debug!("Failed to accept connection for {bind}: {}", err);
						continue;
					}
				};
				trace!("Accepted {peer} for reverse tunnel {bind}");
				let conn = conn.clone();
				let forward_bind = bind.clone();
				let log_bind = forward_bind.clone();
				let closing = closing.clone();
				forwarded
					.spawn(async move {
						let bridged =
							forward_connection(conn, forward_bind, tcp_stream, closing);
						if let Err(err) = bridged.await {
							debug!("Failed to forward connection for {}: {}", log_bind, err);
						}
					})
					.await;
			}
		}
	}

	forwarded.drain().await;
	Ok(())
}

async fn forward_connection(
	conn: Connection,
	bind: HostPort,
	tcp_stream: TcpStream,
	closing: CancellationToken,
) -> Result<()> {
	let (mut send, recv) = conn.open_bi().await?;
	send.write_struct(&ReverseTunnelChannel { bind: bind.clone() })
		.await?;

	let (tcp_read, tcp_write) = tcp_stream.into_split();
	copy_bidi_stream(tcp_read, tcp_write, recv, send, &closing).await;

	Ok(())
}

/// Writes a one-way notice (success or failure) back to the client.
async fn send_notice(conn: &Connection, notice: &impl Serialize) -> Result<()> {
	let mut notify = conn.open_uni().await?;
	notify.write_struct(notice).await?;
	notify.finish()?;
	Ok(())
}

async fn report_failure(conn: &Connection, bind: &HostPort, reason: &str) -> Result<()> {
	debug!("Reverse tunnel request for {bind} failed: {reason}");
	send_notice(
		conn,
		&ReverseTunnelNotice::Failed(ReverseTunnelFailure {
			bind: bind.clone(),
			reason: reason.to_owned(),
		}),
	)
	.await?;
	Err(eyre!("reverse tunnel request for {bind} failed: {reason}"))
}

#[cfg(test)]
mod ut_tests;

#[cfg(test)]
mod it_tests;
