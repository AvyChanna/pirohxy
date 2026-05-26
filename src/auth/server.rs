use iroh::{EndpointId, Signature, endpoint::Connection};
use rand::TryRng;
use tracing::trace;

use super::{AUTH_DENY, AUTH_OK, KEY_LEN, NONCE_LEN, SIG_LEN, VerifyError, make_message};

pub(super) async fn server_authenticate<A>(
	server_id: EndpointId,
	conn: &Connection,
	auther: &A,
) -> Result<(), VerifyError>
where
	A: Fn(EndpointId) -> bool,
{
	trace!("starting authentication for {}", conn.remote_id());
	// init conn
	let (mut send, mut recv) = conn
		.accept_bi()
		.await
		.map_err(|_| VerifyError::StreamError)?;

	trace!("new connection from {}", conn.remote_id());

	// get master key
	let mut key_bytes = [0u8; KEY_LEN];
	recv.read_exact(&mut key_bytes)
		.await
		.map_err(|_| VerifyError::StreamError)?;

	trace!("received master key from {}", conn.remote_id());

	// verify master key
	let master_id = EndpointId::from_bytes(&key_bytes).map_err(|_| VerifyError::InvalidToken)?;
	if !(auther)(master_id) {
		trace!("master key is not allowed for {}", conn.remote_id());
		return Err(VerifyError::NotAllowed);
	}

	trace!("master key is allowed for {}", conn.remote_id());

	// send nonce
	let mut nonce = [0u8; NONCE_LEN];
	rand::rng()
		.try_fill_bytes(&mut nonce)
		.map_err(|_| VerifyError::RngError)?;
	send.write_all(&nonce)
		.await
		.map_err(|_| VerifyError::StreamError)?;

	trace!("sent nonce to {}", conn.remote_id());
	// get sign
	let mut sig_bytes = [0u8; SIG_LEN];
	recv.read_exact(&mut sig_bytes)
		.await
		.map_err(|_| VerifyError::StreamError)?;
	let signature = Signature::from_bytes(&sig_bytes);

	trace!("received signature from {}", conn.remote_id());
	// verify sign
	let client_id = conn.remote_id();
	let msg = make_message(server_id.as_bytes(), client_id.as_bytes(), &nonce);

	let verified = master_id.verify(&msg, &signature).is_ok();

	if verified {
		send.write_all(&[AUTH_OK])
			.await
			.map_err(|_| VerifyError::StreamError)?;
		let _ = send.finish();
		let _ = recv.stop(0u8.into());
		trace!("authentication successful for {}", conn.remote_id());
		Ok(())
	} else {
		send.write_all(&[AUTH_DENY])
			.await
			.map_err(|_| VerifyError::StreamError)?;
		let _ = send.finish();
		let _ = recv.stop(0u8.into());
		trace!("authentication failed for {}", conn.remote_id());
		Err(VerifyError::InvalidSignature)
	}
}
