//! Several ALPNs at the same time, against one server and one grant table.
//!
//! Every test here opens more than one protocol at once and keeps the older connections
//! open while the newer ones run. That is what a muxed server has to survive: a channel
//! that works alone but breaks as soon as another ALPN is registered, another client
//! connects, or a refused dial happens next door is a router bug, not a protocol bug.
//!
//! Grants are the same decision the CLI makes from the `auth/` directory — an identity and
//! a protocol — so this file covers the muxing while `cli_smoke.rs` covers the real
//! directory layout.

mod common;

extern crate alloc;

use alloc::sync::Arc;
use core::{net::SocketAddr, time::Duration};

use color_eyre::eyre::{Context, Result, ensure, eyre};
use common::{
	ECHO_PAYLOAD, EchoTarget, Grants, TestClient, TestServer, free_port, read_line, shell_command,
	step, work_dir,
};
use iroh::endpoint::{Connection, ConnectionError, ReadExactError};
use pirohxy::{
	protocol::{
		client::{
			connect::{self, Client},
			exec, file, reverse_tunnel, shell, tunnel,
		},
		shell::{ShellOutput, ShellRequest},
	},
	protoutils::{HostPort, alpn::Protocol},
};
use tokio::{
	io::{AsyncWriteExt, BufReader},
	net::TcpStream,
	task::JoinSet,
};
use tokio_util::sync::CancellationToken;

/// The payload the `file` probe round-trips.
const FILE_PAYLOAD: &[u8] = b"mux-file-payload\n";
/// [`ECHO_PAYLOAD`] as text, for the probes that read a line back from the target.
const ECHO_LINE: &str = "hello-from-target";

/// The word each probe prints or transfers, so a test can tell which channel carried it.
fn marker(protocol: Protocol) -> String {
	format!("mux-{protocol}")
}

/// Opens one authenticated connection per protocol, all at once from one client node.
async fn open_all(
	client: Arc<TestClient>,
	server: Arc<TestServer>,
	protocols: &[Protocol],
) -> Result<Vec<(Protocol, Client)>> {
	let mut set: JoinSet<Result<(Protocol, Client)>> = JoinSet::new();
	for protocol in protocols {
		let protocol = *protocol;
		let client = Arc::clone(&client);
		let server = Arc::clone(&server);
		let _task = set.spawn(async move {
			let client = client.connect(&server, protocol).await?;
			Ok((protocol, client))
		});
	}

	let mut opened = Vec::new();
	while let Some(joined) = set.join_next().await {
		opened.push(joined.wrap_err("the connect task panicked")??);
	}
	Ok(opened)
}

/// Runs one probe per connection, all at once, keeping the order they were opened in.
///
/// The connections are cloned rather than borrowed because a probe must be able to run on
/// any worker while the others are still in flight. The `Client` handles that own those
/// connections stay with the caller: dropping one would close its connection.
async fn run_probes(
	connections: &[(Protocol, Connection)],
	target: &EchoTarget,
	name: &str,
) -> Result<Vec<(Protocol, String)>> {
	let mut set = JoinSet::new();
	for (index, (protocol, conn)) in connections.iter().enumerate() {
		let protocol = *protocol;
		let conn = conn.clone();
		let target = target.addr();
		let name = format!("{name}-{index}");
		let _task = set.spawn(async move {
			let probe = probe(protocol, &conn, target, &name).await;
			probe.map(|outcome| (protocol, outcome))
		});
	}

	let mut done = Vec::new();
	while let Some(joined) = set.join_next().await {
		done.push(joined.wrap_err("the probe task panicked")??);
	}
	Ok(done)
}

/// The connection handles behind `opened`, so probes can run without moving the clients.
fn connections_of(opened: &[(Protocol, Client)]) -> Vec<(Protocol, Connection)> {
	opened
		.iter()
		.map(|(protocol, client)| (*protocol, client.conn().clone()))
		.collect()
}

/// The probe for `protocol`: the smallest action that proves the channel carries its own
/// traffic and not another channel's.
async fn probe(
	protocol: Protocol,
	conn: &Connection,
	target: SocketAddr,
	name: &str,
) -> Result<String> {
	let target = HostPort::from(target);
	match protocol {
		Protocol::Exec => exec_probe(conn).await,
		Protocol::Tunnel => tunnel_probe(conn, &target).await,
		Protocol::Socks => socks_probe(conn, &target).await,
		Protocol::ReverseTunnel => reverse_tunnel_probe(conn, &target).await,
		Protocol::Shell => shell_probe(conn).await,
		Protocol::File => file_probe(conn, name).await,
	}
}

async fn exec_probe(conn: &Connection) -> Result<String> {
	let command = shell_command(&format!("echo {}", marker(Protocol::Exec)));
	let outcome = exec::run(conn, &command).await?;
	ensure!(outcome.error.is_none(), "exec failed: {:?}", outcome.error);
	ensure!(
		outcome.code == Some(0),
		"exec exit code: {:?}",
		outcome.code
	);
	let stdout = String::from_utf8_lossy(&outcome.stdout).trim().to_owned();
	ensure!(
		stdout.contains(&marker(Protocol::Exec)),
		"exec stdout was {stdout:?}"
	);
	Ok(stdout)
}

async fn tunnel_probe(conn: &Connection, target: &HostPort) -> Result<String> {
	let (mut send, recv) = tunnel::open(conn, target).await?;
	let mut reader = BufReader::new(recv);
	let line = read_line(&mut reader).await?;
	let _ = send.finish();
	ensure!(line.trim() == ECHO_LINE, "tunnel got {line:?}");
	Ok(line.trim().to_owned())
}

async fn file_probe(conn: &Connection, name: &str) -> Result<String> {
	let path = format!("{name}.txt");
	file::upload(conn, &path, false, FILE_PAYLOAD).await?;
	let downloaded = file::download(conn, &path, 0).await?.read_to_end().await?;
	ensure!(
		downloaded == FILE_PAYLOAD,
		"file round trip changed the bytes"
	);
	Ok(marker(Protocol::File))
}

/// Talks SOCKS5 to the server's proxy channel by hand: greeting, no-auth method, CONNECT.
async fn socks_probe(conn: &Connection, target: &HostPort) -> Result<String> {
	let (mut send, mut recv) = conn.open_bi().await?;

	send.write_all(&[0x05, 0x01, 0x00]).await?;
	let mut greeting = [0u8; 2];
	recv.read_exact(&mut greeting).await?;
	ensure!(greeting == [0x05, 0x00], "socks greeting was {greeting:?}");

	let SocketAddr::V4(target) = target.resolve_first().await? else {
		return Err(eyre!("the SOCKS probe needs an IPv4 target"));
	};
	let mut request = vec![0x05, 0x01, 0x00, 0x01];
	request.extend_from_slice(&target.ip().octets());
	request.extend_from_slice(&target.port().to_be_bytes());
	send.write_all(&request).await?;

	let mut reply = [0u8; 10];
	recv.read_exact(&mut reply).await?;
	ensure!(
		reply[0] == 0x05 && reply[1] == 0x00,
		"socks CONNECT failed with {:#04x}",
		reply[1]
	);

	let mut reader = BufReader::new(recv);
	let line = read_line(&mut reader).await?;
	let _ = send.finish();
	ensure!(line.trim() == ECHO_LINE, "socks got {line:?} from {target}");
	Ok(line.trim().to_owned())
}

/// Asks the server to listen, bridges what it accepts to the local target, then connects
/// to the server-side port the way any other host would.
async fn reverse_tunnel_probe(conn: &Connection, target: &HostPort) -> Result<String> {
	let bind = HostPort::new("127.0.0.1", free_port().await?);
	let forward = reverse_tunnel::listen(conn, &bind).await?;
	ensure!(forward.bind == bind, "the server echoed a different bind");
	ensure!(forward.bound.port != 0, "port 0 must report the real port");

	let conn = conn.clone();
	let target = target.clone();
	let local = forward.bound.resolve_first().await?;
	let _bridge = tokio::spawn(async move {
		// Nothing here is torn down by an exit signal: the fixture ends the bridge by
		// closing the connection.
		let closing = CancellationToken::new();
		match reverse_tunnel::accept(&conn).await {
			Ok(channel) => {
				let _done = channel.bridge_to(&target, &closing).await;
			}
			Err(err) => {
				let _refused = err;
			}
		}
	});

	let mut remote = TcpStream::connect(local).await?;
	let mut reader = BufReader::new(&mut remote);
	let line = read_line(&mut reader).await?;
	drop(reader);
	let _closed = remote.shutdown().await;
	forward.close()?;

	ensure!(line.trim() == ECHO_LINE, "the reverse tunnel got {line:?}");
	Ok(line.trim().to_owned())
}

async fn shell_probe(conn: &Connection) -> Result<String> {
	let request = ShellRequest {
		cmd: Some(format!("echo {}", marker(Protocol::Shell))),
		term: "xterm-256color".to_owned(),
		cols: 80,
		rows: 24,
		cwd: None,
		env: Vec::new(),
	};
	let mut session = shell::open(conn, &request).await?;

	let mut seen = String::new();
	let wanted = marker(Protocol::Shell);
	let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
	while tokio::time::Instant::now() < deadline && !seen.contains(&wanted) {
		match tokio::time::timeout(Duration::from_secs(10), session.next_output()).await {
			Ok(Some(ShellOutput::Data(chunk))) => seen.push_str(&String::from_utf8_lossy(&chunk)),
			Ok(Some(ShellOutput::Error(reason))) => {
				drop(session.close().await);
				return Err(eyre!("the shell session failed: {reason}"));
			}
			Ok(Some(ShellOutput::Exit { code })) => {
				ensure!(code == 0, "the shell exited with {code} before printing");
				break;
			}
			Ok(None) => break,
			Err(_elapsed) => break,
		}
	}
	drop(session.close().await);

	ensure!(seen.contains(&wanted), "shell output was {seen:?}");
	Ok(wanted)
}

/// Asserts that `protocol` is refused for `client`, and that the refusal reached the client
/// as a cut-short authentication rather than as a success.
///
/// The server refuses a disallowed identity by closing the connection with code 403, so the
/// client sees the handshake break mid-flight: either the connection itself is closed or a
/// read on the authentication stream fails.
async fn expect_refused(
	client: &TestClient,
	server: &TestServer,
	protocol: Protocol,
) -> Result<()> {
	match client.connect(server, protocol).await {
		Ok(client) => {
			client.close().await;
			Err(eyre!(
				"{protocol} was granted to an identity that should not have it"
			))
		}
		Err(err) => {
			let cut_short = err.chain().any(|cause| {
				cause.downcast_ref::<ConnectionError>().is_some()
					|| cause.downcast_ref::<ReadExactError>().is_some()
			});
			ensure!(
				cut_short,
				"the {protocol} refusal was not reported as a closed authentication: {err}"
			);
			Ok(())
		}
	}
}

/// Asserts that every entry in `results` is the marker its protocol should have produced.
fn expect_markers(results: &[(Protocol, String)], what: &str) -> Result<()> {
	for (protocol, outcome) in results {
		match protocol {
			Protocol::Exec | Protocol::Shell => ensure!(
				outcome.contains(&marker(*protocol)),
				"{what}: {protocol} produced {outcome:?}"
			),
			_ => (),
		}
	}
	Ok(())
}

/// Every ALPN at once, one client, one identity granted all of them.
#[tokio::test]
async fn six_alpns_at_once_under_one_grant() -> Result<()> {
	let dir = work_dir("mux_one_grant")?;
	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let client = Arc::new(TestClient::spawn().await?);
	let grants = Grants::new().allow(client.id(), &Protocol::ALL);
	let server = Arc::new(TestServer::spawn(&grants, dir.join("root")).await?);

	let opened = step(open_all(
		Arc::clone(&client),
		Arc::clone(&server),
		&Protocol::ALL,
	))
	.await?;
	ensure!(
		opened.len() == Protocol::ALL.len(),
		"every ALPN must get its own connection"
	);

	let results = step(run_probes(&connections_of(&opened), &target, "one-grant")).await?;
	ensure!(
		results.len() == Protocol::ALL.len(),
		"a probe did not finish"
	);
	expect_markers(&results, "one grant")?;

	for (_protocol, client) in opened {
		client.close().await;
	}
	server.close().await;
	Ok(())
}

/// Two identities with disjoint grants, both connected and both working at the same time.
///
/// Each identity holds what the other is refused, so a leak in either direction — a grant
/// applied to the wrong identity, or a refusal that also kills a live channel — shows up.
#[tokio::test]
async fn two_clients_with_different_grants_at_once() -> Result<()> {
	let dir = work_dir("mux_split_grant")?;
	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let commands = Arc::new(TestClient::spawn().await?);
	let tunnels = Arc::new(TestClient::spawn().await?);

	let granted_to_commands = [Protocol::Exec, Protocol::Shell, Protocol::File];
	let granted_to_tunnels = [Protocol::Socks, Protocol::Tunnel, Protocol::ReverseTunnel];

	let grants = Grants::new()
		.allow(commands.id(), &granted_to_commands)
		.allow(tunnels.id(), &granted_to_tunnels);
	let server = Arc::new(TestServer::spawn(&grants, dir.join("root")).await?);

	let mut opened = Vec::new();
	opened.extend(
		step(open_all(
			Arc::clone(&commands),
			Arc::clone(&server),
			&granted_to_commands,
		))
		.await?,
	);
	opened.extend(
		step(open_all(
			Arc::clone(&tunnels),
			Arc::clone(&server),
			&granted_to_tunnels,
		))
		.await?,
	);
	ensure!(opened.len() == 6, "both identities must hold their grants");

	let results = step(run_probes(&connections_of(&opened), &target, "split-grant")).await?;
	ensure!(results.len() == opened.len(), "a probe did not finish");
	expect_markers(&results, "split grant")?;

	// The grants are disjoint, so for every ALPN exactly one of the two identities is
	// refused. All six refusals happen while both identities are connected and working.
	let mut set = JoinSet::new();
	for protocol in Protocol::ALL {
		let denied = if granted_to_commands.contains(&protocol) {
			Arc::clone(&tunnels)
		} else {
			Arc::clone(&commands)
		};
		let server = Arc::clone(&server);
		let _task = set.spawn(async move { expect_refused(&denied, &server, protocol).await });
	}
	while let Some(joined) = set.join_next().await {
		joined.wrap_err("the refusal task panicked")??;
	}

	let results = step(run_probes(
		&connections_of(&opened),
		&target,
		"split-grant-after-refusal",
	))
	.await?;
	ensure!(
		results.len() == opened.len(),
		"a granted channel stopped working after a refused dial"
	);
	expect_markers(&results, "split grant after refusals")?;

	for (_protocol, client) in opened {
		client.close().await;
	}
	server.close().await;
	Ok(())
}

/// A run of refused dials must not disturb the channels that are already open.
#[tokio::test]
async fn refused_dials_leave_live_channels_alone() -> Result<()> {
	let dir = work_dir("mux_refused")?;
	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let client = Arc::new(TestClient::spawn().await?);

	let granted = [Protocol::Exec, Protocol::Shell];
	let denied: Vec<Protocol> = Protocol::ALL
		.into_iter()
		.filter(|protocol| !granted.contains(protocol))
		.collect();

	let grants = Grants::new().allow(client.id(), &granted);
	let server = Arc::new(TestServer::spawn(&grants, dir.join("root")).await?);

	let opened = step(open_all(Arc::clone(&client), Arc::clone(&server), &granted)).await?;
	let results = step(run_probes(
		&connections_of(&opened),
		&target,
		"before-refusals",
	))
	.await?;
	expect_markers(&results, "before refusals")?;

	let mut set = JoinSet::new();
	for protocol in &denied {
		let client = Arc::clone(&client);
		let server = Arc::clone(&server);
		let protocol = *protocol;
		let _task = set.spawn(async move { expect_refused(&client, &server, protocol).await });
	}
	while let Some(joined) = set.join_next().await {
		joined.wrap_err("the refusal task panicked")??;
	}

	let results = step(run_probes(
		&connections_of(&opened),
		&target,
		"after-refusals",
	))
	.await?;
	ensure!(
		results.len() == opened.len(),
		"a live channel dropped out after a refused dial"
	);
	expect_markers(&results, "after refusals")?;

	for (_protocol, client) in opened {
		client.close().await;
	}
	server.close().await;
	Ok(())
}

/// Many streams on one ALPN, one connection: the streams must not cross.
#[tokio::test]
async fn many_streams_share_one_alpn() -> Result<()> {
	const RUNS: usize = 8;
	let dir = work_dir("mux_streams")?;
	let client = Arc::new(TestClient::spawn().await?);
	let grants = Grants::new().allow(client.id(), &[Protocol::Exec]);
	let server = Arc::new(TestServer::spawn(&grants, dir.join("root")).await?);

	let opened = step(open_all(
		Arc::clone(&client),
		Arc::clone(&server),
		&[Protocol::Exec],
	))
	.await?;
	ensure!(opened.len() == 1, "one connection was expected");
	let conn = opened[0].1.conn().clone();

	let mut set = JoinSet::new();
	for index in 0..RUNS {
		let conn = conn.clone();
		let _task = set.spawn(async move {
			let wanted = format!("mux-exec-{index}");
			let outcome = exec::run(&conn, &shell_command(&format!("echo {wanted}"))).await?;
			let stdout = String::from_utf8_lossy(&outcome.stdout).trim().to_owned();
			ensure!(stdout.contains(&wanted), "stream {index} got {stdout:?}");
			Ok(wanted)
		});
	}

	let mut done = 0;
	while let Some(joined) = set.join_next().await {
		let _wanted = joined.wrap_err("the exec task panicked")??;
		done += 1;
	}
	ensure!(done == RUNS, "only {done} of {RUNS} streams finished");

	for (_protocol, client) in opened {
		client.close().await;
	}
	server.close().await;
	Ok(())
}

/// A remote forward held open while four other ALPNs run, then used.
///
/// The forward is the awkward case: its keepalive stream is deliberately left unfinished,
/// so a router that closes idle streams or reuses one connection per client would break it.
#[tokio::test]
async fn reverse_forward_stays_open_while_other_alpns_run() -> Result<()> {
	let dir = work_dir("mux_reverse")?;
	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let client = Arc::new(TestClient::spawn().await?);
	let grants = Grants::new().allow(client.id(), &Protocol::ALL);
	let server = Arc::new(TestServer::spawn(&grants, dir.join("root")).await?);

	let opened = step(open_all(
		Arc::clone(&client),
		Arc::clone(&server),
		&[
			Protocol::ReverseTunnel,
			Protocol::Exec,
			Protocol::File,
			Protocol::Tunnel,
			Protocol::Socks,
		],
	))
	.await?;
	ensure!(opened.len() == 5, "five ALPNs were expected");

	let forward_conn = opened
		.iter()
		.find_map(|(protocol, client)| {
			(protocol == &Protocol::ReverseTunnel).then(|| client.conn().clone())
		})
		.ok_or_else(|| eyre!("no reverse tunnel connection"))?;

	let bind = HostPort::new("127.0.0.1", free_port().await?);
	let forward = reverse_tunnel::listen(&forward_conn, &bind).await?;
	let bound = forward.bound.resolve_first().await?;

	let accept_loop = {
		let conn = forward_conn.clone();
		let local = target.host_port();
		tokio::spawn(async move {
			let mut bridged = 0u8;
			while bridged < 2 {
				let Ok(channel) = reverse_tunnel::accept(&conn).await else {
					break;
				};
				bridged += 1;
				let local = local.clone();
				let closing = CancellationToken::new();
				let _bridge = tokio::spawn(async move {
					let _done = channel.bridge_to(&local, &closing).await;
				});
			}
			bridged
		})
	};

	let live: Vec<(Protocol, Connection)> = connections_of(&opened)
		.into_iter()
		.filter(|(protocol, _conn)| protocol != &Protocol::ReverseTunnel)
		.collect();
	let results = step(run_probes(&live, &target, "reverse-live")).await?;
	expect_markers(&results, "reverse forward held open")?;

	for _ in 0..2 {
		let mut remote = TcpStream::connect(bound).await?;
		let mut reader = BufReader::new(&mut remote);
		let carried = read_line(&mut reader).await?;
		drop(reader);
		let _closed = remote.shutdown().await;
		ensure!(carried.trim() == ECHO_LINE, "the forward got {carried:?}");
	}

	let bridged = accept_loop.await.wrap_err("the accept task panicked")?;
	ensure!(
		bridged == 2,
		"the forward bridged {bridged} of 2 connections"
	);
	forward.close()?;

	for (_protocol, client) in opened {
		client.close().await;
	}
	server.close().await;
	Ok(())
}

/// Two clients holding the same grant, each running every ALPN at the same time.
#[tokio::test]
async fn two_clients_with_the_same_grant_at_once() -> Result<()> {
	let dir = work_dir("mux_same_grant")?;
	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let first = Arc::new(TestClient::spawn().await?);
	let second = Arc::new(TestClient::spawn().await?);

	let grants = Grants::new()
		.allow(first.id(), &Protocol::ALL)
		.allow(second.id(), &Protocol::ALL);
	let server = Arc::new(TestServer::spawn(&grants, dir.join("root")).await?);

	let mut opened = Vec::new();
	for (index, client) in [first.clone(), second.clone()].iter().enumerate() {
		let connections = step(open_all(
			Arc::clone(client),
			Arc::clone(&server),
			&Protocol::ALL,
		))
		.await?;
		ensure!(
			connections.len() == Protocol::ALL.len(),
			"client {index} lost a connection to an ALPN"
		);
		opened.extend(connections);
	}

	let results = step(run_probes(&connections_of(&opened), &target, "same-grant")).await?;
	ensure!(
		results.len() == 2 * Protocol::ALL.len(),
		"one of the twelve probes did not finish"
	);
	expect_markers(&results, "same grant")?;

	for (_protocol, client) in opened {
		client.close().await;
	}
	server.close().await;
	Ok(())
}

/// A connection that dies is dialled again on the same node, and the channel works after.
///
/// The point is the identity: a reconnect that started a fresh node would present a
/// different client id and lose the grant the server already gave it.
#[tokio::test]
async fn a_dropped_connection_is_dialled_again() -> Result<()> {
	let dir = work_dir("mux_reconnect")?;
	let target = EchoTarget::spawn(ECHO_PAYLOAD).await?;
	let node = TestClient::spawn().await?;
	let grants = Grants::new().allow(node.id(), &Protocol::ALL);
	let server = TestServer::spawn(&grants, dir.join("root")).await?;

	let mut client = step(node.connect(&server, Protocol::Socks)).await?;
	let before = client.conn().clone();
	let _first_reply = step(socks_probe(&before, &target.host_port())).await?;

	// Kill the connection the way a peer that went away does, and wait until this side
	// has noticed it.
	before.close(0u32.into(), b"test");
	let _reason = tokio::time::timeout(Duration::from_secs(10), before.closed()).await?;

	let closing = CancellationToken::new();
	step(connect::reconnect(&mut client, Protocol::Socks, &closing)).await?;

	ensure!(
		client.conn().remote_id() == server.id(),
		"the reconnect must dial the same server"
	);
	// Same node, so the identity the server granted is still the one being asked about.
	let after = client.conn().clone();
	let _second_reply = step(socks_probe(&after, &target.host_port())).await?;

	client.close().await;
	server.close().await;
	Ok(())
}
