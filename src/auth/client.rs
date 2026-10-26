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
	let (mut send, mut recv) = conn.open_bi().await?;

	trace!("connected to {}", conn.remote_id());
	// send master key
	send.write_all(master_key.public().as_bytes()).await?;

	trace!("sent master key to {}", conn.remote_id());
	// get nonce
	let mut nonce = [0u8; NONCE_LEN];
	recv.read_exact(&mut nonce).await?;

	trace!("received nonce from {}", conn.remote_id());
	// sign nonce
	let signature = master_key.sign(&make_message(
		server_id.as_bytes(),
		client_id.as_bytes(),
		&nonce,
	));

	// send sign
	send.write_all(&signature.to_bytes()).await?;

	trace!("sent signature to {}", conn.remote_id());

	// get auth result
	let mut auth_result = [0u8; 1];
	recv.read_exact(&mut auth_result).await?;

	trace!("received auth result from {}", conn.remote_id());
	if auth_result[0] == AUTH_OK {
		trace!("authentication successful for {}", conn.remote_id());
		let _ = recv.stop(0u8.into());
		let _ = send.finish();
		Ok(())
	} else {
		// The server only writes this byte after the nonce was signed, so it always
		// means the signature was rejected. A disallowed identity is refused by
		// closing the connection instead.
		trace!("authentication failed for {}", conn.remote_id());
		let _ = recv.stop(0u8.into());
		let _ = send.finish();
		Err(VerifyError::InvalidSignature)
	}
}
