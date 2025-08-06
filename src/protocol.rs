use core::fmt::Debug;

use color_eyre::eyre::{Result, eyre};
use fast_socks5::{
	ReplyError, Socks5Command,
	server::{Socks5ServerProtocol, run_tcp_proxy},
};
use iroh::{
	endpoint::Connection,
	protocol::{AcceptError, ProtocolHandler},
};
use tracing::warn;

use crate::config::Auth;

// Timeout for server TCP requests in seconds.
/// This is used to limit the time a server will wait for a TCP request before timing out.
const SERVER_TCP_REQ_TIMEOUT_SEC: u64 = 10;

/// A Socks protocol handler for Iroh that implements the `ProtocolHandler` trait.
/// It handles incoming connections and processes SOCKS5 commands.
#[derive(Debug)]
pub(crate) struct Socks<T>
where
	T: Auth,
{
	auth: T,
}

impl<T> Socks<T>
where
	T: Auth,
{
	/// Creates a new `Socks` instance with the provided authentication mechanism.
	pub(crate) fn new(auth: T) -> Self {
		Self { auth }
	}
}

impl<T> ProtocolHandler for Socks<T>
where
	T: Auth + Debug + Send + Sync + 'static,
{
	fn accept(&self, conn: Connection) -> impl Future<Output = Result<(), AcceptError>> + Send {
		Box::pin(async move {
			let node_id = conn.remote_node_id()?;
			if !self.auth.is_allowed(&node_id) {
				return Err(AcceptError::User {
					source: eyre!("remote node {} is not allowed", node_id).into(),
				});
			}

			let (s, r) = conn.accept_bi().await?;
			let socket = tokio::io::join(r, s);
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

			match cmd {
				Socks5Command::TCPConnect => {
					let _tcp_proxy =
						run_tcp_proxy(proto, &target_addr, SERVER_TCP_REQ_TIMEOUT_SEC, true)
							.await
							.map_err(AcceptError::from_err)?;
					Ok(())
				}
				Socks5Command::TCPBind => {
					warn!("TCP bind command is not supported");
					proto
						.reply_error(&ReplyError::CommandNotSupported)
						.await
						.map_err(AcceptError::from_err)?;
					Err(AcceptError::from_err(ReplyError::CommandNotSupported))
				}
				Socks5Command::UDPAssociate => {
					warn!("UDP associate command is not supported");
					proto
						.reply_error(&ReplyError::CommandNotSupported)
						.await
						.map_err(AcceptError::from_err)?;
					Err(AcceptError::from_err(ReplyError::CommandNotSupported))
				}
			}
		})
	}
}
