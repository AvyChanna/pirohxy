use core::fmt::Debug;
use core::time::Duration;

use color_eyre::eyre::Result;
use fast_socks5::{
	ReplyError, Socks5Command,
	server::{Socks5ServerProtocol, run_tcp_proxy},
};
use iroh::{
	endpoint::{Connection, RecvStream, SendStream},
	protocol::{AcceptError, ProtocolHandler},
};
use tracing::{debug, trace};

/// A Socks protocol handler for Iroh that implements the `ProtocolHandler` trait.
/// It handles incoming connections and processes SOCKS5 commands.
#[derive(Debug, Copy, Clone, Default)]
pub struct Socks {
	request_timeout: Duration,
}

impl Socks {
	/// Creates a new `Socks` instance.
	#[must_use]
	pub fn new(request_timeout: Duration) -> Self {
		Self { request_timeout }
	}
}

impl ProtocolHandler for Socks {
	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		let endpoint_id = conn.remote_id();
		trace!("Accepted connection from {}", endpoint_id.fmt_short());
		loop {
			let (s, r) = conn.accept_bi().await?;
			let request_timeout = self.request_timeout;
			let _task = tokio::spawn(async move {
				if let Err(err) = handle_socks(r, s, request_timeout).await {
					debug!("Failed to process SOCKS stream: {}", err);
				}
			});
		}
	}
}

async fn handle_socks(
	mut r: RecvStream,
	mut s: SendStream,
	server_timeout: Duration,
) -> Result<(), AcceptError> {
	let socket = tokio::io::join(&mut r, &mut s);
	let (proto, cmd, mut target_addr) = Socks5ServerProtocol::accept_no_auth(socket)
		.await
		.map_err(AcceptError::from_err)?
		.read_command()
		.await
		.map_err(AcceptError::from_err)?;
	target_addr = target_addr
		.resolve_dns()
		.await
		.map_err(AcceptError::from_err)?;

	let result = match cmd {
		Socks5Command::TCPConnect => {
			let _tcp_proxy = run_tcp_proxy(proto, &target_addr, server_timeout, true)
				.await
				.map_err(AcceptError::from_err)?;
			Ok(())
		}
		Socks5Command::TCPBind => {
			debug!("TCP bind command is not supported");
			proto
				.reply_error(&ReplyError::CommandNotSupported)
				.await
				.map_err(AcceptError::from_err)?;
			Err(AcceptError::from_err(ReplyError::CommandNotSupported))
		}
		Socks5Command::UDPAssociate => {
			debug!("UDP associate command is not supported");
			proto
				.reply_error(&ReplyError::CommandNotSupported)
				.await
				.map_err(AcceptError::from_err)?;
			Err(AcceptError::from_err(ReplyError::CommandNotSupported))
		}
	};

	trace!(
		"Processed command {:?} for target {}, result: {:?}",
		cmd, target_addr, result
	);
	let _ = r.stop(0u8.into());
	let _ = s.finish();

	result
}
