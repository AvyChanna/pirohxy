use color_eyre::eyre::Result;

use super::{ReadStructExt, TunnelRequest, WriteStructExt};
use crate::protoutils::HostPort;
use tokio::io::AsyncWriteExt;

#[test]
fn tunnel_request_round_trips_through_wire_format() -> Result<()> {
	let request = TunnelRequest {
		target: HostPort::new("example.test", 443),
	};
	let decoded = postcard::from_bytes::<TunnelRequest>(&postcard::to_allocvec(&request)?)?;
	assert_eq!(decoded, request);
	Ok(())
}

#[tokio::test]
async fn tunnel_request_carries_one_target_per_stream() -> Result<()> {
	let first = TunnelRequest {
		target: HostPort::new("127.0.0.1", 1),
	};
	let second = TunnelRequest {
		target: HostPort::new("[::1]", 2),
	};

	// An in-memory pipe stands in for the bidi stream the real handler reads.
	let (mut writer, mut reader) = tokio::io::duplex(1024);
	writer.write_struct(&first).await?;
	writer.write_struct(&second).await?;
	writer.flush().await?;

	let first_decoded: TunnelRequest = reader.read_struct().await?;
	let second_decoded: TunnelRequest = reader.read_struct().await?;

	assert_eq!(first_decoded, first);
	assert_eq!(second_decoded, second);
	Ok(())
}
