use core::{fmt::Debug, net::SocketAddr};
use std::io;

use color_eyre::eyre::Result;
use iroh::{
	endpoint::{Connection, RecvStream, SendStream},
	protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tracing::{debug, trace};

use crate::protoutils::stream::{ReadStructExt, copy_bidi_stream};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelRequest {
	pub target_addr: SocketAddr,
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
		let endpoint_id = conn.remote_id();
		trace!("Accepted connection from {}", endpoint_id.fmt_short());
		let mut r = conn.accept_uni().await?;
		let TunnelRequest { target_addr } = r.read_struct().await.map_err(|x| {
			AcceptError::from_err(io::Error::new(io::ErrorKind::InvalidData, x.to_string()))
		})?;
		debug!("Tunnel request to {}", target_addr);
		loop {
			let (s, r) = conn.accept_bi().await?;
			let _task = tokio::spawn(async move {
				if let Err(err) = handle_tunnel(s, r, target_addr).await {
					debug!("Failed to process Tunnel stream: {}", err);
				}
			});
		}
	}
}

async fn handle_tunnel(s: SendStream, r: RecvStream, target: SocketAddr) -> Result<()> {
	let tcp_stream = TcpStream::connect(&target).await?;
	let (tcp_read, tcp_write) = tcp_stream.into_split();
	copy_bidi_stream(tcp_read, tcp_write, r, s).await;

	Ok(())
}
