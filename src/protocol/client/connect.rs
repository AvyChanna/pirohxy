use core::time::Duration;

use color_eyre::eyre::{Result, eyre};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey, endpoint::Connection};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::auth::AuthenticatedConnection;
use crate::protoutils::alpn::Protocol;
use crate::start_iroh_node;

/// Longest wait between two reconnect attempts.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// An iroh node plus the authenticated connection it holds to a server.
///
/// The endpoint is kept inside on purpose: dropping it closes the connection, so a
/// caller must hold this value for as long as it keeps using [`Client::conn`].
///
/// The node outlives any single connection: [`Client::reconnect`] dials the same server
/// again on it, which keeps the client's identity - and therefore the grant the server
/// gave it - the same across a drop.
#[derive(Debug)]
pub struct Client {
	endpoint: Endpoint,
	conn: Connection,
	server: EndpointAddr,
	master_key: SecretKey,
}

impl Client {
	/// Starts a node with a throwaway identity and authenticates to `server_id` for
	/// `protocol` on behalf of `master_key`.
	///
	/// Every protocol goes through here so that a missing handshake cannot slip in as a
	/// plain connection, and so a refusal reports the server's own reason.
	///
	/// # Errors
	/// The node cannot bind, the peer cannot be reached, or the peer refuses the identity.
	pub async fn connect(
		master_key: SecretKey,
		server_id: EndpointId,
		protocol: Protocol,
	) -> Result<Self> {
		let client_key = SecretKey::generate();
		debug!(
			"Dialing server {} as client {} for the {protocol} protocol",
			server_id.fmt_short(),
			client_key.public().fmt_short()
		);
		let endpoint = start_iroh_node(client_key).await?;
		match Self::connect_with(
			&endpoint,
			EndpointAddr::new(server_id),
			protocol,
			master_key,
		)
		.await
		{
			Ok(client) => Ok(client),
			Err(err) => {
				// The endpoint would otherwise be dropped without a close, which iroh
				// reports as an ungraceful shutdown.
				endpoint.close().await;
				Err(err)
			}
		}
	}

	/// Authenticates the caller's own `endpoint` to `server` for `protocol`.
	///
	/// Use this instead of [`Client::connect`] when one node must carry several protocols,
	/// or when the caller picked the node's transport: the endpoint is only borrowed, so
	/// the caller keeps deciding when it shuts down. Keep a handle on `endpoint` alive for
	/// as long as the returned connection is used.
	///
	/// # Errors
	/// The peer cannot be reached, or the peer refuses the identity.
	pub async fn connect_with(
		endpoint: &Endpoint,
		server: EndpointAddr,
		protocol: Protocol,
		master_key: SecretKey,
	) -> Result<Self> {
		let conn = endpoint
			.connect_with_auth(server.clone(), protocol, master_key.clone())
			.await?;
		Ok(Self {
			endpoint: endpoint.clone(),
			conn,
			server,
			master_key,
		})
	}

	/// Dials the same server again on the same node and swaps in the new connection.
	///
	/// The connection that died is closed first, so the server sees it end instead of
	/// waiting for its idle timeout. The node is not restarted, so the client identity the
	/// server already granted stays the same.
	///
	/// # Errors
	/// The peer cannot be reached, or the peer refuses the identity.
	pub async fn reconnect(&mut self, protocol: Protocol) -> Result<()> {
		let old = self.conn.clone();
		old.close(0u32.into(), b"reconnecting");

		debug!(
			"Re-dialing server {} as client {} for the {protocol} protocol",
			self.server.id.fmt_short(),
			self.endpoint.id().fmt_short()
		);
		self.conn = self
			.endpoint
			.connect_with_auth(self.server.clone(), protocol, self.master_key.clone())
			.await?;
		Ok(())
	}

	/// The authenticated connection to the server.
	#[must_use]
	pub fn conn(&self) -> &Connection {
		&self.conn
	}

	/// The server this client is connected to.
	#[must_use]
	pub fn server_id(&self) -> EndpointId {
		self.server.id
	}

	/// Closes the node and everything running over it.
	pub async fn close(self) {
		self.endpoint.close().await;
	}
}

/// Brings `client`'s connection back after it died, waiting between attempts.
///
/// The wait doubles per failed attempt up to [`MAX_RECONNECT_DELAY`]: a server that is
/// briefly unreachable should be retried, but not hammered. `closing` ends the wait, so
/// an exit signal is not swallowed by a backoff timer.
///
/// # Errors
/// The process was asked to exit before the connection came back.
pub async fn reconnect(
	client: &mut Client,
	protocol: Protocol,
	closing: &CancellationToken,
) -> Result<()> {
	let mut delay = Duration::from_secs(1);
	loop {
		match client.reconnect(protocol).await {
			Ok(()) => {
				info!("Reconnected to {}", client.server_id().fmt_short());
				return Ok(());
			}
			Err(err) => warn!(
				"Reconnect to {} failed: {err}, retrying in {:?}",
				client.server_id().fmt_short(),
				delay
			),
		}

		tokio::select! {
			() = closing.cancelled() => {
				return Err(eyre!(
					"exit signal while reconnecting to {}",
					client.server_id().fmt_short()
				));
			}
			() = tokio::time::sleep(delay) => {}
		}
		delay = (delay * 2).min(MAX_RECONNECT_DELAY);
	}
}
