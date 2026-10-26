use core::fmt::Debug;

use color_eyre::eyre::Result;
use serde::de::DeserializeOwned;

use super::{
	ReverseTunnelChannel, ReverseTunnelFailure, ReverseTunnelNotice, ReverseTunnelRequest,
	ReverseTunnelResponse, Serialize,
};
use crate::protoutils::HostPort;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) -> Result<T> {
	Ok(postcard::from_bytes::<T>(&postcard::to_allocvec(value)?)?)
}

#[test]
fn reverse_tunnel_messages_round_trip_through_wire_format() -> Result<()> {
	let request = ReverseTunnelRequest {
		bind: HostPort::new("127.0.0.1", 9090),
	};
	let response = ReverseTunnelResponse {
		bind: HostPort::new("127.0.0.1", 9090),
		bound: HostPort::new("127.0.0.1", 43210),
	};
	let channel = ReverseTunnelChannel {
		bind: HostPort::new("[::1]", 9090),
	};

	assert_eq!(round_trip(&request)?, request);
	assert_eq!(round_trip(&response)?, response);
	assert_eq!(round_trip(&channel)?, channel);

	let bound = ReverseTunnelNotice::Bound(response);
	assert_eq!(round_trip(&bound)?, bound);
	let refused = ReverseTunnelNotice::Failed(ReverseTunnelFailure {
		bind: HostPort::new("127.0.0.1", 9090),
		reason: "address already in use".to_owned(),
	});
	assert_eq!(round_trip(&refused)?, refused);
	Ok(())
}
