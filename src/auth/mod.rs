mod client;
mod server;

use core::fmt::Debug;

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
const MSG_PREFIX: &str = "pirohxy-auth-challenge";
const SUFFIX: [u8; 10] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff];
const MSG_LEN: usize = MSG_PREFIX.len() + KEY_LEN + KEY_LEN + NONCE_LEN + SUFFIX.len(); // prefix + server_id + clinet_id + nonce + suffix
const AUTH_OK: u8 = 1;
const AUTH_DENY: u8 = 0;

#[derive(thiserror::Error, Debug)]
pub(super) enum VerifyError {
	#[error("stream error during authentication")]
	StreamError,
	#[error("invalid identity token")]
	InvalidToken,
	#[error("invalid identity signature")]
	InvalidSignature,
	#[error("identity not allowed")]
	NotAllowed,
	#[error("random number generation error")]
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

#[cfg(test)]
mod tests {
	use iroh::SecretKey;

	use crate::auth::{MSG_PREFIX, SUFFIX};

	use super::{AUTH_DENY, AUTH_OK, MSG_LEN, NONCE_LEN, make_message};

	#[test]
	fn auth_status_bytes_are_stable() {
		assert_eq!(AUTH_OK, 1);
		assert_eq!(AUTH_DENY, 0);
		assert_ne!(AUTH_OK, AUTH_DENY);
	}

	#[test]
	fn make_message_concatenates_server_client_and_nonce_correctly() {
		let server = SecretKey::generate().public();
		let client = SecretKey::generate().public();
		let nonce = [7u8; NONCE_LEN];

		let message = make_message(server.as_bytes(), client.as_bytes(), &nonce);

		let expected = {
			let mut m = Vec::new();
			m.extend_from_slice(MSG_PREFIX.as_bytes());
			m.extend_from_slice(server.as_bytes());
			m.extend_from_slice(client.as_bytes());
			m.extend_from_slice(&nonce);
			m.extend_from_slice(&SUFFIX);
			m
		};
		assert_eq!(message.len(), MSG_LEN);
		assert_eq!(message.to_vec(), expected);
	}
}
