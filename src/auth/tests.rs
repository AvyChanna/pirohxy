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
