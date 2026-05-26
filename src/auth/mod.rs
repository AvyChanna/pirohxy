mod client;
mod server;

use core::fmt::{Debug, Display};

use iroh::{
	Endpoint, EndpointId, SecretKey, Signature,
	endpoint::{Accepting, Connection},
	protocol::{AcceptError, ProtocolHandler, RouterBuilder},
};

use client::client_authenticate;
use server::server_authenticate;

const NONCE_LEN: usize = 32;
const SIG_LEN: usize = Signature::LENGTH;
const KEY_LEN: usize = EndpointId::LENGTH;
const MSG_LEN: usize = KEY_LEN + KEY_LEN + NONCE_LEN; // server_id + clinet_id + nonce
const AUTH_OK: u8 = 1;
const AUTH_DENY: u8 = 0;

#[derive(Debug)]
pub(super) enum VerifyError {
	StreamError,
	InvalidToken,
	InvalidSignature,
	NotAllowed,
	RngError,
}

impl Display for VerifyError {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		match self {
			Self::StreamError => write!(f, "stream error during authentication"),
			Self::InvalidToken => write!(f, "invalid identity token"),
			Self::InvalidSignature => write!(f, "invalid identity signature"),
			Self::NotAllowed => write!(f, "identity not allowed"),
			Self::RngError => write!(f, "random number generation error"),
		}
	}
}

impl core::error::Error for VerifyError {}

fn make_message(
	server_id: &[u8; KEY_LEN],
	client_id: &[u8; KEY_LEN],
	nonce: &[u8; NONCE_LEN],
) -> [u8; MSG_LEN] {
	let mut msg = [0; MSG_LEN];
	msg[..KEY_LEN].copy_from_slice(server_id);
	msg[KEY_LEN..KEY_LEN * 2].copy_from_slice(client_id);
	msg[KEY_LEN * 2..].copy_from_slice(nonce);
	msg
}

pub(super) trait AuthenticatedConnection {
	async fn connect_with_auth(
		&self,
		server_id: EndpointId,
		alpn: &[u8],
		master_key: SecretKey,
	) -> Result<Connection, VerifyError>;
}

impl AuthenticatedConnection for Endpoint {
	async fn connect_with_auth(
		&self,
		server_id: EndpointId,
		alpn: &[u8],
		master_key: SecretKey,
	) -> Result<Connection, VerifyError> {
		// connect
		let conn = self
			.connect(server_id, alpn)
			.await
			.map_err(|_| VerifyError::StreamError)?;

		// authenticate
		client_authenticate(server_id, self.id(), master_key, &conn).await?;

		Ok(conn)
	}
}

#[derive(Clone)]
struct AccessLimiter<P, A>
where
	P: ProtocolHandler,
	A: Fn(EndpointId) -> bool + Send + Sync + 'static,
{
	proto: P,
	auther: A,
	endpoint: Endpoint,
}

impl<P, A> AccessLimiter<P, A>
where
	P: ProtocolHandler,
	A: Fn(EndpointId) -> bool + Send + Sync + 'static,
{
	fn new(endpoint: Endpoint, proto: P, auther: A) -> Self {
		Self {
			proto,
			auther,
			endpoint,
		}
	}
}

impl<P, A> Debug for AccessLimiter<P, A>
where
	P: ProtocolHandler,
	A: Fn(EndpointId) -> bool + Send + Sync + 'static,
{
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.debug_struct("AccessLimiter")
			.field("endpoint", &self.endpoint)
			.field("proto", &self.proto)
			.field(
				"auther",
				&"<Fn(EndpointId) -> bool + Send + Sync + 'static>",
			)
			.finish()
	}
}
impl<P, A> ProtocolHandler for AccessLimiter<P, A>
where
	P: ProtocolHandler,
	A: Fn(EndpointId) -> bool + Send + Sync + 'static,
{
	fn on_accepting(
		&self,
		accepting: Accepting,
	) -> impl Future<Output = Result<Connection, AcceptError>> + Send {
		self.proto.on_accepting(accepting)
	}

	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		if let Err(err) = server_authenticate(self.endpoint.id(), &conn, &self.auther).await {
			conn.close(403u32.into(), b"identity not allowed");
			return Err(AcceptError::from_err(err));
		}

		self.proto.accept(conn).await
	}

	fn shutdown(&self) -> impl Future<Output = ()> + Send {
		self.proto.shutdown()
	}
}

pub(super) trait AuthenticatedRouterBuilder {
	fn accept_with_auth<P, A>(self, alpn: &[u8], proto: P, auther: A) -> Self
	where
		P: ProtocolHandler,
		A: Fn(EndpointId) -> bool + Send + Sync + 'static;
}

impl AuthenticatedRouterBuilder for RouterBuilder {
	fn accept_with_auth<P, A>(self, alpn: &[u8], proto: P, auther: A) -> Self
	where
		P: ProtocolHandler,
		A: Fn(EndpointId) -> bool + Send + Sync + 'static,
	{
		let endpoint = self.endpoint().clone();
		self.accept(alpn, AccessLimiter::new(endpoint, proto, auther))
	}
}
