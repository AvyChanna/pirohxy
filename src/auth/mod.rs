mod client;
mod server;

use core::fmt::Debug;

use iroh::{
	Endpoint, EndpointAddr, EndpointId, KeyParsingError, SecretKey, Signature,
	endpoint::{Accepting, ConnectError, Connection, ConnectionError, ReadExactError, WriteError},
	protocol::{AcceptError, ProtocolHandler, RouterBuilder},
};

use client::client_authenticate;
use server::server_authenticate;
use tracing::trace;

use crate::protoutils::alpn::Protocol;

const NONCE_LEN: usize = 32;
const SIG_LEN: usize = Signature::LENGTH;
const KEY_LEN: usize = EndpointId::LENGTH;
const MSG_PREFIX: &str = "pirohxy-auth-challenge";
const SUFFIX: [u8; 10] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff];
const MSG_LEN: usize = MSG_PREFIX.len() + KEY_LEN + KEY_LEN + NONCE_LEN + SUFFIX.len(); // prefix + server_id + client_id + nonce + suffix
const AUTH_OK: u8 = 1;
const AUTH_DENY: u8 = 0;
/// Longest close reason the server reports back to a rejected client. QUIC caps the
/// reason field, so anything longer is trimmed instead of being dropped entirely.
const MAX_CLOSE_REASON_LEN: usize = 1024;

/// Why an authentication attempt failed.
///
/// Every variant that wraps a transport failure keeps the original error reachable
/// through `source`, so a caller can tell a refused identity apart from a dropped
/// connection or a bad signature instead of seeing a flat `StreamError`.
#[derive(thiserror::Error, Debug)]
pub(super) enum VerifyError {
	/// The peer could not be reached at all.
	#[error("could not reach the peer: {source}")]
	Connect {
		#[from]
		source: ConnectError,
	},
	/// A stream could not be opened, or the peer closed the connection.
	#[error("authentication stream failed: {source}")]
	Stream {
		#[from]
		source: ConnectionError,
	},
	/// A frame could not be written to the peer.
	#[error("authentication write failed: {source}")]
	Write {
		#[from]
		source: WriteError,
	},
	/// A frame could not be read from the peer.
	#[error("authentication read failed: {source}")]
	Read {
		#[from]
		source: ReadExactError,
	},
	/// The identity token the peer offered is not a valid endpoint id.
	#[error("invalid identity token: {source}")]
	InvalidToken {
		#[from]
		source: KeyParsingError,
	},
	/// The peer is not allowed to use this protocol.
	#[error("identity not allowed for the {0} protocol")]
	NotAllowed(Protocol),
	/// The signature the peer produced for the nonce did not verify.
	#[error("peer rejected the authentication signature")]
	InvalidSignature,
	/// The server could not produce a nonce.
	///
	/// The system RNG used here reports `Infallible`, so there is nothing more useful to
	/// keep than the fact that it failed.
	#[error("random number generation failed")]
	RngError,
}

fn make_message(
	server_id: &[u8; KEY_LEN],
	client_id: &[u8; KEY_LEN],
	nonce: &[u8; NONCE_LEN],
) -> [u8; MSG_LEN] {
	const SERVER_ID_START: usize = MSG_PREFIX.len();
	const CLIENT_ID_START: usize = SERVER_ID_START + KEY_LEN;
	const NONCE_START: usize = CLIENT_ID_START + KEY_LEN;
	const SUFFIX_START: usize = NONCE_START + NONCE_LEN;

	let mut msg = [0; MSG_LEN];
	msg[..SERVER_ID_START].copy_from_slice(MSG_PREFIX.as_bytes());
	msg[SERVER_ID_START..CLIENT_ID_START].copy_from_slice(server_id);
	msg[CLIENT_ID_START..NONCE_START].copy_from_slice(client_id);
	msg[NONCE_START..SUFFIX_START].copy_from_slice(nonce);
	msg[SUFFIX_START..].copy_from_slice(&SUFFIX);
	msg
}

pub(super) trait AuthenticatedConnection {
	/// Connects to `server` speaking `protocol` and proves ownership of `master_key`.
	///
	/// The address rather than the bare id is what the connection is dialled with, so a
	/// caller that already knows how to reach the peer — a direct address, a relay it was
	/// told about — is not forced to rely on discovery.
	async fn connect_with_auth(
		&self,
		server: EndpointAddr,
		protocol: Protocol,
		master_key: SecretKey,
	) -> Result<Connection, VerifyError>;
}

impl AuthenticatedConnection for Endpoint {
	async fn connect_with_auth(
		&self,
		server: EndpointAddr,
		protocol: Protocol,
		master_key: SecretKey,
	) -> Result<Connection, VerifyError> {
		let server_id = server.id;
		// connect
		let conn = self.connect(server, protocol.alpn()).await?;

		// authenticate
		match client_authenticate(server_id, self.id(), master_key, &conn).await {
			Ok(()) => Ok(conn),
			Err(err) => {
				// A server refuses a disallowed identity by closing the connection, so
				// the peer's own close reason is the only place the real cause is recorded.
				if let Some(reason) = conn.close_reason() {
					trace!("peer closed the connection during authentication: {reason}");
					return Err(VerifyError::Stream { source: reason });
				}
				Err(err)
			}
		}
	}
}

#[derive(Clone)]
struct AccessLimiter<P, A>
where
	P: ProtocolHandler,
	A: Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static,
{
	proto: P,
	auther: A,
	endpoint: Endpoint,
	protocol: Protocol,
}

impl<P, A> AccessLimiter<P, A>
where
	P: ProtocolHandler,
	A: Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static,
{
	fn new(endpoint: Endpoint, protocol: Protocol, proto: P, auther: A) -> Self {
		Self {
			proto,
			auther,
			endpoint,
			protocol,
		}
	}
}

impl<P, A> Debug for AccessLimiter<P, A>
where
	P: ProtocolHandler,
	A: Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static,
{
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.debug_struct("AccessLimiter")
			.field("endpoint", &self.endpoint)
			.field("protocol", &self.protocol)
			.field("proto", &self.proto)
			.field(
				"auther",
				&"<Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static>",
			)
			.finish()
	}
}
impl<P, A> ProtocolHandler for AccessLimiter<P, A>
where
	P: ProtocolHandler,
	A: Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static,
{
	fn on_accepting(
		&self,
		accepting: Accepting,
	) -> impl Future<Output = Result<Connection, AcceptError>> + Send {
		self.proto.on_accepting(accepting)
	}

	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		if let Err(err) =
			server_authenticate(self.endpoint.id(), &conn, self.protocol, &self.auther).await
		{
			// Report the real reason to the client: without it the peer only sees a
			// dead stream and cannot tell "not allowed" from a network failure.
			let reason = err.to_string();
			let bytes = reason.as_bytes();
			conn.close(
				403u32.into(),
				&bytes[..bytes.len().min(MAX_CLOSE_REASON_LEN)],
			);
			return Err(AcceptError::from_err(err));
		}

		self.proto.accept(conn).await
	}

	fn shutdown(&self) -> impl Future<Output = ()> + Send {
		self.proto.shutdown()
	}
}

pub(super) trait AuthenticatedRouterBuilder {
	/// Registers `proto` under the ALPN belonging to `protocol`, gated by `auther`.
	fn accept_with_auth<P, A>(self, protocol: Protocol, proto: P, auther: A) -> Self
	where
		P: ProtocolHandler,
		A: Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static;
}

impl AuthenticatedRouterBuilder for RouterBuilder {
	fn accept_with_auth<P, A>(self, protocol: Protocol, proto: P, auther: A) -> Self
	where
		P: ProtocolHandler,
		A: Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static,
	{
		let endpoint = self.endpoint().clone();
		self.accept(
			protocol.alpn(),
			AccessLimiter::new(endpoint, protocol, proto, auther),
		)
	}
}

#[cfg(test)]
mod tests;
