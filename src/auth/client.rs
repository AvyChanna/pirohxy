use iroh::{EndpointId, SecretKey, endpoint::Connection};
use tracing::trace;

use super::{AUTH_OK, NONCE_LEN, VerifyError, make_message};

pub(super) async fn client_authenticate(
	server_id: EndpointId,
	client_id: EndpointId,
	master_key: SecretKey,
	conn: &Connection,
) -> Result<(), VerifyError> {
	trace!("starting authentication for {}", conn.remote_id());
	// init conn
	let (mut send, mut recv) = conn.open_bi().await.map_err(|_| VerifyError::StreamError)?;

	trace!("connected to {}", conn.remote_id());
	// send master key
	send.write_all(master_key.public().as_bytes())
		.await
		.map_err(|_| VerifyError::StreamError)?;

	trace!("sent master key to {}", conn.remote_id());
	// get nonce
	let mut nonce = [0u8; NONCE_LEN];
	recv.read_exact(&mut nonce)
		.await
		.map_err(|_| VerifyError::StreamError)?;

	trace!("received nonce from {}", conn.remote_id());
	// sign nonce
	let signature = master_key.sign(&make_message(
		server_id.as_bytes(),
		client_id.as_bytes(),
		&nonce,
	));

	// send sign
	send.write_all(&signature.to_bytes())
		.await
		.map_err(|_| VerifyError::StreamError)?;

	trace!("sent signature to {}", conn.remote_id());

	// get auth result
	let mut auth_result = [0u8; 1];
	recv.read_exact(&mut auth_result)
		.await
		.map_err(|_| VerifyError::StreamError)?;

	trace!("received auth result from {}", conn.remote_id());
	if auth_result[0] == AUTH_OK {
		trace!("authentication successful for {}", conn.remote_id());
		let _ = recv.stop(0u8.into());
		let _ = send.finish();
		Ok(())
	} else {
		trace!("authentication failed for {}", conn.remote_id());
		let _ = recv.stop(0u8.into());
		let _ = send.finish();
		Err(VerifyError::NotAllowed)
	}
}
