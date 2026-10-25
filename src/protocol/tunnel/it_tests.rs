use core::time::Duration;

use color_eyre::eyre::Result;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpListener,
	task::JoinHandle,
};

use super::Tunnel;
use crate::{
	protocol::client::tunnel,
	protoutils::{HostPort, alpn::ALPN_TUNNEL, interproto::testnet},
};

/// Fails the test instead of hanging when a step never completes.
async fn within<T>(step: impl Future<Output = T>) -> T {
	tokio::time::timeout(Duration::from_secs(15), step)
		.await
		.expect("tunnel step timed out")
}

/// Answers every connection on a free port with `reply` followed by the request.
///
/// The request is drained first: a target that closes while our bytes are still
/// sitting unread in its buffer makes Windows abort the connection, which discards
/// the answer as well.
async fn echo_server(reply: &[u8]) -> Result<(HostPort, JoinHandle<()>)> {
	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let bind = HostPort::from(listener.local_addr()?);
	let reply = reply.to_owned();
	let task = tokio::spawn(async move {
		while let Ok((mut stream, _peer)) = listener.accept().await {
			let mut request = Vec::new();
			drop(stream.read_to_end(&mut request).await);
			drop(stream.write_all(&reply).await);
			drop(stream.write_all(&request).await);
			drop(stream.shutdown().await);
		}
	});
	Ok((bind, task))
}

#[tokio::test]
async fn client_tunnel_relays_bytes_to_and_from_the_target() -> Result<()> {
	testnet::init_tracing();
	let server = testnet::spawn_server_on(ALPN_TUNNEL, Tunnel).await?;
	let (target, echo) = echo_server(b"pong").await?;

	let (mut send, mut recv) = within(tunnel::open(server.conn(), &target)).await?;
	send.write_all(b"ping").await?;
	send.finish()?;

	let got = within(recv.read_to_end(usize::MAX)).await?;
	// The target answers and echoes the request back, so both directions are proven.
	assert_eq!(got, b"pongping");

	echo.abort();
	Ok(())
}

#[tokio::test]
async fn client_tunnel_reports_a_refused_target() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_TUNNEL, Tunnel).await?;

	// Reserve a port and release it again, so nothing is listening when the server
	// tries to reach it.
	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let dead = HostPort::from(listener.local_addr()?);
	drop(listener);

	let refusal = within(tunnel::open(server.conn(), &dead))
		.await
		.expect_err("a dead target must be refused");
	assert!(refusal.to_string().contains("refused"));
	Ok(())
}
