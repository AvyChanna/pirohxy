use core::fmt::Debug;

use color_eyre::eyre::Result;
use iroh::{
	endpoint::{Connection, RecvStream, SendStream},
	protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::{
	protoutils::{
		HostPort,
		accept::serve_bi_streams,
		stream::{ReadStructExt, WriteStructExt, copy_bidi_stream},
	},
	shutdown::shutdown_handle,
};

/// Opens one forwarded connection, mirroring SSH's `direct-tcpip` channel.
///
/// The client opens a bidirectional stream and sends this request as the first frame;
/// the server then connects to `target` and bridges the two streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelRequest {
	/// Host and port the server should connect to on the client's behalf.
	pub target: HostPort,
}

/// Reports that the server could not open the requested target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelFailure {
	/// Human readable reason the connection was refused.
	pub reason: String,
}

/// The server's first frame back on a tunnel stream.
///
/// Without it the client could not tell a refusal apart from the target's own bytes,
/// because everything after this frame is the raw byte stream of the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TunnelResponse {
	/// The target was reached, raw bytes follow on this stream.
	Opened,
	/// The target could not be reached.
	Failed(TunnelFailure),
}

/// A Tunnel protocol handler for Iroh that implements the `ProtocolHandler` trait.
/// It allows a client to request a TCP connection to a target address, which is then
/// bridged to the Iroh stream.
#[derive(Debug, Copy, Clone, Default)]
pub struct Tunnel;

impl Tunnel {
	/// Creates a new `Tunnel` instance.
	#[must_use]
	pub fn new() -> Self {
		Self {}
	}
}

impl ProtocolHandler for Tunnel {
	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		// One handle on the exit signal per connection, cloned into every stream task, so a
		// Ctrl-C ends the tunnels this connection opened instead of leaving them copying.
		let closing = shutdown_handle().token();
		serve_bi_streams(&conn, "Tunnel", move |r, s| {
			let closing = closing.clone();
			handle_tunnel(r, s, closing)
		})
		.await
	}
}

#[expect(
	let_underscore_drop,
	reason = "Notifying the client about a failed tunnel is best-effort: the stream may already be gone"
)]
async fn handle_tunnel(
	mut r: RecvStream,
	mut s: SendStream,
	closing: CancellationToken,
) -> Result<()> {
	let TunnelRequest { target } = r.read_struct().await?;
	debug!("Tunnel request to {target}");

	let addr = match target.resolve_first().await {
		Ok(addr) => addr,
		Err(err) => {
			let reason = format!("cannot resolve {target}: {err}");
			debug!("{reason}");
			let _ = s
				.write_struct(&TunnelResponse::Failed(TunnelFailure { reason }))
				.await;
			let _ = s.finish();
			return Ok(());
		}
	};

	let tcp_stream = match TcpStream::connect(addr).await {
		Ok(tcp_stream) => tcp_stream,
		Err(err) => {
			let reason = format!("cannot connect to {target} ({addr}): {err}");
			debug!("{reason}");
			let _ = s
				.write_struct(&TunnelResponse::Failed(TunnelFailure { reason }))
				.await;
			let _ = s.finish();
			return Ok(());
		}
	};

	trace!("Tunnel to {target} ({addr}) established");
	s.write_struct(&TunnelResponse::Opened).await?;
	let (tcp_read, tcp_write) = tcp_stream.into_split();
	copy_bidi_stream(tcp_read, tcp_write, r, s, &closing).await;

	Ok(())
}

#[cfg(test)]
mod ut_tests;

#[cfg(test)]
mod it_tests;
