use iroh::endpoint::{RecvStream, SendStream};
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::stream::{ReadStructExt, WriteStructExt};
use super::tasks::Tasks;

/// Number of messages buffered in each direction of an interactive session.
const INTERACTIVE_CHANNEL_CAPACITY: usize = 32;

/// Starts the two pumps of an interactive session over a bidirectional stream.
///
/// The returned [`Interactive`] carries the messages in both directions and owns the tasks
/// that move them. End it with [`Interactive::close`]: dropping it without closing aborts
/// the pumps, which resets the stream instead of finishing it.
///
/// Infallible: the channels always exist, and both pump tasks report their own failures
/// in the logs, so there is nothing left for the caller to handle.
pub async fn spawn_interactive<F, T>(
	mut recv_stream: RecvStream,
	mut send_stream: SendStream,
) -> Interactive<F, T>
where
	F: DeserializeOwned + Send + 'static,
	T: Serialize + Send + Sync + 'static,
{
	let (tx_from_remote, rx_from_remote) = channel::<F>(INTERACTIVE_CHANNEL_CAPACITY);
	let (tx_to_remote, mut rx_to_remote) = channel::<T>(INTERACTIVE_CHANNEL_CAPACITY);
	let closed = CancellationToken::new();

	let mut pumps = Tasks::new("interactive");
	pumps
		.spawn({
			let closed = closed.clone();
			async move {
				let ended = closed.cancelled();
				tokio::pin!(ended);
				loop {
					tokio::select! {
						biased;
						() = &mut ended => {
							let _ = recv_stream.stop(0u8.into());
							break;
						}
						msg = recv_stream.read_struct::<F>() => {
							match msg {
								Ok(msg) => {
									if tx_from_remote.send(msg).await.is_err() {
										debug!("Interactive receiver dropped, stopping inbound stream");
										let _ = recv_stream.stop(0u8.into());
										break;
									}
								}
								Err(err) => {
									debug!("Inbound interactive stream ended: {err:?}");
									break;
								}
							}
						}
					}
				}
			}
		})
		.await;
	pumps
		.spawn(async move {
			while let Some(msg) = rx_to_remote.recv().await {
				if send_stream.write_struct(&msg).await.is_err() {
					debug!("Failed to write to interactive stream, draining send queue");
					while rx_to_remote.try_recv().is_ok() {}
					break;
				}
			}
			// All senders were dropped: close the stream so the peer sees a clean end.
			let _ = send_stream.finish();
		})
		.await;

	Interactive {
		rx: rx_from_remote,
		tx: tx_to_remote,
		closed,
		pumps,
	}
}

/// One interactive session: the messages in both directions and the tasks that carry them.
///
/// Built by [`spawn_interactive`]. End it with [`Interactive::close`] to hand over the
/// queued output and let the peer see a clean stream end.
#[derive(Debug)]
pub struct Interactive<F, T> {
	rx: Receiver<F>,
	tx: Sender<T>,
	closed: CancellationToken,
	pumps: Tasks,
}

impl<F, T> Interactive<F, T> {
	/// Waits for the next message from the peer.
	///
	/// `None` once the session is over: the inbound stream ended or the session was closed.
	pub async fn recv(&mut self) -> Option<F> {
		self.rx.recv().await
	}

	/// Sends `msg` to the peer.
	///
	/// # Errors
	/// The session is over, so the pump that writes to the stream has already stopped.
	pub async fn send(&self, msg: T) -> Result<(), tokio::sync::mpsc::error::SendError<T>> {
		self.tx.send(msg).await
	}

	/// A sending handle for the caller's own tasks.
	///
	/// The send stream is finished by [`Interactive::close`], not by dropping the last
	/// clone, so a clone is only useful while it is held: hand it back before closing.
	#[must_use]
	pub fn sender(&self) -> Sender<T> {
		self.tx.clone()
	}

	/// Ends the session and joins both pumps.
	///
	/// Queued output is written and the send stream finished, so the peer sees a clean end,
	/// while the inbound stream is stopped. Every clone handed out by [`Interactive::sender`]
	/// must already be dropped: the stream is only finished once the last sender is gone. A
	/// pump that outlasts the drain grace period is aborted rather than left running.
	pub async fn close(self) {
		let Self {
			mut rx,
			tx,
			closed,
			mut pumps,
		} = self;
		drop(tx);
		rx.close();
		closed.cancel();
		pumps.drain().await;
	}
}

#[cfg(test)]
pub mod testnet {
	use core::sync::atomic::{AtomicU64, Ordering};
	use core::time::Duration;
	use std::path::{Path, PathBuf};

	use color_eyre::eyre::Result;
	use iroh::endpoint::Connection;
	use serde::{Deserialize, Serialize};
	use tokio_util::sync::CancellationToken;

	use super::{Interactive, spawn_interactive};

	#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
	enum Ping {
		Ping(u32),
	}

	/// ALPN the fixtures register their handlers under.
	const TEST_ALPN: &[u8] = b"/avychanna/pirohxy/test/v1";

	/// How long a test waits for a single message before giving up.
	const TEST_TIMEOUT_MS: u64 = 5_000;

	/// Lets `RUST_LOG` trace a session or a tunnel that misbehaves.
	///
	/// `drop(try_init(..))` because a test binary may already have installed a
	/// subscriber: a second one is not worth failing the test over.
	pub fn init_tracing() {
		drop(
			tracing_subscriber::fmt()
				.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
				.with_test_writer()
				.try_init(),
		);
	}

	/// The path [`TempDir`] uses for `tag`: under the system temp dir, named after this
	/// crate, the process and a per-test counter, so neither parallel tests inside one
	/// run nor two runs of the suite can collide.
	fn unique_path(tag: &str) -> PathBuf {
		static COUNTER: AtomicU64 = AtomicU64::new(0);
		let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
		std::env::temp_dir().join(format!(
			"pirohxy-test-{}-{tag}-{unique}",
			std::process::id()
		))
	}

	/// A scratch directory that removes itself when the guard is dropped.
	///
	/// Every test that needs a directory on disk goes through here, so they share one
	/// naming scheme and one lifetime. `Drop` also runs while unwinding a panic, so a
	/// failed test does not leave litter behind.
	pub struct TempDir {
		path: PathBuf,
	}

	impl TempDir {
		/// Creates the directory for `tag`, clearing whatever an earlier run left there.
		///
		/// # Errors
		/// The directory cannot be created.
		pub async fn new(tag: &str) -> Result<Self> {
			let path = unique_path(tag);
			drop(tokio::fs::remove_dir_all(&path).await);
			tokio::fs::create_dir_all(&path).await?;
			Ok(Self { path })
		}

		/// The synchronous form, for tests that do not run on the async runtime.
		///
		/// # Errors
		/// The directory cannot be created.
		pub fn new_sync(tag: &str) -> Result<Self> {
			let path = unique_path(tag);
			drop(std::fs::remove_dir_all(&path));
			std::fs::create_dir_all(&path)?;
			Ok(Self { path })
		}

		/// The directory itself.
		#[must_use]
		pub fn path(&self) -> &Path {
			&self.path
		}
	}

	impl Drop for TempDir {
		fn drop(&mut self) {
			drop(std::fs::remove_dir_all(&self.path));
		}
	}

	/// A connected pair of endpoints plus the connection established between them.
	///
	/// Dropping an [`iroh::Endpoint`] shuts it down and its router waits for the protocol
	/// handlers to finish, so the fixture releases the handler before tearing the rest down.
	pub struct TestPair {
		stop: CancellationToken,
		_server: iroh::Endpoint,
		_client: iroh::Endpoint,
		_router: iroh::protocol::Router,
		pub client_conn: Connection,
		pub server_conn: Connection,
	}

	impl Drop for TestPair {
		fn drop(&mut self) {
			// Runs before the fields are dropped, so the accept handler returns before the
			// router and endpoints shut down.
			self.stop.cancel();
		}
	}

	/// Builds a connected pair of iroh endpoints over the local transport.
	///
	/// The `Minimal` preset with relays disabled keeps the test offline: the client dials
	/// the server's bound address directly instead of looking it up.
	///
	/// # Errors
	/// An endpoint cannot be bound, or the client cannot reach the server.
	pub async fn connected_pair() -> Result<TestPair> {
		let server = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
			.secret_key(iroh::SecretKey::generate())
			.relay_mode(iroh::RelayMode::Disabled)
			.bind()
			.await?;
		let client = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
			.secret_key(iroh::SecretKey::generate())
			.relay_mode(iroh::RelayMode::Disabled)
			.bind()
			.await?;

		let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Connection>();
		let stop = CancellationToken::new();
		let server_addr = server.addr();
		let router = iroh::protocol::Router::builder(server.clone())
			.accept(
				TEST_ALPN,
				Handoff {
					tx,
					stop: stop.clone(),
				},
			)
			.spawn();

		let client_conn = client.connect(server_addr, TEST_ALPN).await?;
		let server_conn = rx
			.recv()
			.await
			.ok_or_else(|| color_eyre::eyre::eyre!("server never accepted"))?;
		Ok(TestPair {
			stop,
			_server: server,
			_client: client,
			_router: router,
			client_conn,
			server_conn,
		})
	}

	/// Waits for the next message of a session, failing the test instead of hanging when
	/// none arrives.
	///
	/// # Errors
	/// No message arrived in time.
	pub async fn recv_within<F, T>(session: &mut Interactive<F, T>) -> Result<Option<F>> {
		Ok(tokio::time::timeout(Duration::from_millis(TEST_TIMEOUT_MS), session.recv()).await?)
	}

	/// A server running `handler` plus the client connection dialed to it.
	///
	/// The handler keeps running until the fixture closes the connection, which is what
	/// releases its accept loop.
	pub struct TestServer {
		stop: CancellationToken,
		_server: iroh::Endpoint,
		_client: iroh::Endpoint,
		_router: iroh::protocol::Router,
		conn: Connection,
	}

	impl TestServer {
		/// The connection the client side of the test talks to the server over.
		#[must_use]
		pub fn conn(&self) -> &Connection {
			&self.conn
		}
	}

	impl Drop for TestServer {
		fn drop(&mut self) {
			self.stop.cancel();
			// Closing the connection is what makes the handler's accept loop return.
			self.conn.close(0u32.into(), b"test finished");
		}
	}

	/// Binds a server endpoint running `handler` and dials a client connection to it.
	///
	/// The `Minimal` preset with relays disabled keeps the test offline.
	///
	/// # Errors
	/// An endpoint cannot be bound, or the client cannot reach the server.
	pub async fn spawn_server<H>(handler: H) -> Result<TestServer>
	where
		H: iroh::protocol::ProtocolHandler + 'static,
	{
		spawn_server_on(TEST_ALPN, handler).await
	}

	/// Binds a server endpoint running `handler` under `alpn` and dials a client to it.
	///
	/// Pass the real ALPN of the protocol under test so the client side of the test can
	/// dial it the way the production client does.
	///
	/// # Errors
	/// An endpoint cannot be bound, or the client cannot reach the server.
	pub async fn spawn_server_on<H>(alpn: &[u8], handler: H) -> Result<TestServer>
	where
		H: iroh::protocol::ProtocolHandler + 'static,
	{
		let server = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
			.secret_key(iroh::SecretKey::generate())
			.relay_mode(iroh::RelayMode::Disabled)
			.bind()
			.await?;
		let client = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
			.secret_key(iroh::SecretKey::generate())
			.relay_mode(iroh::RelayMode::Disabled)
			.bind()
			.await?;

		let router = iroh::protocol::Router::builder(server.clone())
			.accept(alpn, handler)
			.spawn();
		let conn = client.connect(server.addr(), alpn).await?;

		Ok(TestServer {
			stop: CancellationToken::new(),
			_server: server,
			_client: client,
			_router: router,
			conn,
		})
	}

	/// Handler that hands the accepted connection back to the test.
	struct Handoff {
		tx: tokio::sync::mpsc::UnboundedSender<Connection>,
		stop: CancellationToken,
	}

	impl core::fmt::Debug for Handoff {
		fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
			f.debug_struct("Handoff").finish()
		}
	}

	impl iroh::protocol::ProtocolHandler for Handoff {
		#[expect(
			let_underscore_drop,
			reason = "The test always has a receiver waiting for the connection"
		)]
		async fn accept(&self, conn: Connection) -> Result<(), iroh::protocol::AcceptError> {
			// The router closes the connection once this handler returns, so hand out a
			// clone and stay parked until the fixture asks the session to end.
			let _ = self.tx.send(conn.clone());
			self.stop.cancelled().await;
			Ok(())
		}
	}

	#[tokio::test]
	async fn relays_messages_in_both_directions() -> Result<()> {
		let pair = connected_pair().await?;

		// QUIC only announces a stream to the peer once its first frame arrives, so the
		// opener must write before the peer can accept the stream.
		let (client_send, client_recv) = pair.client_conn.open_bi().await?;
		let mut client = spawn_interactive::<Ping, Ping>(client_recv, client_send).await;
		client.send(Ping::Ping(1)).await?;

		let (server_send, server_recv) = pair.server_conn.accept_bi().await?;
		let mut server = spawn_interactive::<Ping, Ping>(server_recv, server_send).await;

		assert_eq!(recv_within(&mut server).await?, Some(Ping::Ping(1)));

		server.send(Ping::Ping(2)).await?;
		assert_eq!(recv_within(&mut client).await?, Some(Ping::Ping(2)));

		client.close().await;
		server.close().await;
		Ok(())
	}

	#[tokio::test]
	async fn closing_the_session_finishes_the_stream() -> Result<()> {
		let pair = connected_pair().await?;
		let (client_send, client_recv) = pair.client_conn.open_bi().await?;
		let client = spawn_interactive::<Ping, Ping>(client_recv, client_send).await;
		client.send(Ping::Ping(7)).await?;

		let (server_send, server_recv) = pair.server_conn.accept_bi().await?;
		let mut server = spawn_interactive::<Ping, Ping>(server_recv, server_send).await;

		assert_eq!(recv_within(&mut server).await?, Some(Ping::Ping(7)));

		client.close().await;
		// The send stream was finished, so the peer's reader reaches a clean end.
		assert_eq!(recv_within(&mut server).await?, None);
		server.close().await;
		Ok(())
	}
}
