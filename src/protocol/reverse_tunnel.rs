use core::{fmt::Debug, net::SocketAddr};

use color_eyre::eyre::Result;
use iroh::{
	endpoint::Connection,
	protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tracing::{debug, trace};

use crate::protoutils::stream::{ReadStructExt, copy_bidi_stream};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseTunnelRequest {
	pub bind_addr: SocketAddr,
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
		let endpoint_id = conn.remote_id();
		trace!("Accepted connection from {}", endpoint_id.fmt_short());
		loop {
			let mut r = conn.accept_uni().await?;
			let conn_clone = conn.clone();
			let _task = tokio::spawn(async move {
				let ReverseTunnelRequest { bind_addr } = match r.read_struct().await {
					Ok(req) => req,
					Err(err) => {
						debug!("Failed to read ReverseTunnel request: {}", err);
						return;
					}
				};
				debug!("Reverse Tunnel request to bind to {}", bind_addr);

				let listener = match TcpListener::bind(&bind_addr).await {
					Ok(listener) => listener,
					Err(err) => {
						debug!("Failed to bind to {}: {}", bind_addr, err);
						return;
					}
				};
				debug!("Listening on {}", bind_addr);

				if let Err(err) = handle_reverse_tunnel(conn_clone, listener).await {
					debug!("Failed to process ReverseTunnel stream: {}", err);
				}
			});
		}
	}
}

async fn handle_reverse_tunnel(conn: Connection, listener: TcpListener) -> Result<()> {
	loop {
		let (tcp_stream, _) = listener.accept().await?;
		let (tcp_read, tcp_write) = tcp_stream.into_split();

		let (new_s, new_r) = conn.open_bi().await?;
		let _task = tokio::spawn(async move {
			copy_bidi_stream(tcp_read, tcp_write, new_r, new_s).await;
		});
	}
}
