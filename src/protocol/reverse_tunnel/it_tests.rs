use core::time::Duration;

use color_eyre::eyre::Result;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpStream,
};

use super::ReverseTunnel;
use crate::{
	protocol::client::reverse_tunnel::{self, accept},
	protoutils::{HostPort, alpn::ALPN_REVERSE_TUNNEL, interproto::testnet},
};

/// Fails the test instead of hanging when a step never completes.
async fn within<T>(step: impl Future<Output = T>) -> T {
	tokio::time::timeout(Duration::from_secs(15), step)
		.await
		.expect("reverse tunnel step timed out")
}

#[tokio::test]
async fn client_reverse_tunnel_bridges_a_connection_the_server_accepted() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_REVERSE_TUNNEL, ReverseTunnel).await?;
	let requested = HostPort::new("127.0.0.1", 0);

	let forward = within(reverse_tunnel::listen(server.conn(), &requested)).await?;
	assert_eq!(forward.bind, requested);
	assert_ne!(forward.bound.port, 0, "port 0 must report the real port");

	let mut remote = TcpStream::connect(forward.bound.resolve_first().await?).await?;
	remote.write_all(b"from-tcp").await?;

	let channel = within(accept(server.conn())).await?;
	assert_eq!(channel.bind, forward.bind);

	let (mut send, mut recv) = channel.into_streams();
	let mut inbound = [0u8; 8];
	within(recv.read_exact(&mut inbound)).await?;
	assert_eq!(&inbound, b"from-tcp");

	send.write_all(b"from-iroh").await?;
	send.finish()?;

	let mut echoed = Vec::new();
	let echoed_len = within(remote.read_to_end(&mut echoed)).await?;
	assert_eq!(echoed_len, b"from-iroh".len());
	assert_eq!(echoed, b"from-iroh");

	forward.close()?;
	Ok(())
}

#[tokio::test]
async fn client_reverse_tunnel_reports_a_bind_failure() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_REVERSE_TUNNEL, ReverseTunnel).await?;

	// TEST-NET-1 is not assigned to this host, so binding to it cannot succeed.
	let unroutable = HostPort::new("192.0.2.1", 8123);

	let refusal = within(reverse_tunnel::listen(server.conn(), &unroutable))
		.await
		.expect_err("an unroutable bind must be refused");
	assert!(refusal.to_string().contains("refused"));
	Ok(())
}
