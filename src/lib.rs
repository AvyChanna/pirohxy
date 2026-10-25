extern crate alloc;

mod auth;
pub mod protocol;
pub mod protoutils;
mod shutdown;

use alloc::sync::Arc;
use std::path::Path;

use color_eyre::eyre::Result;
use iroh::{Endpoint, EndpointId, SecretKey, endpoint::presets, protocol::RouterBuilder};
use tokio::time::Duration;
use tracing::{debug, info};

use auth::AuthenticatedRouterBuilder;
use protocol::{
	exec::Exec, file::File, reverse_tunnel::ReverseTunnel, shell::Shell, socks::Socks,
	tunnel::Tunnel,
};
use protoutils::alpn::Protocol;
use shutdown::shutdown_handle;

/// Starts the egress proxy server with the given identity and configuration.
///
/// Every protocol in [`Protocol::ALL`] is registered. Which of them a peer may actually
/// use is decided per connection by `auther`, so an identity can be limited to SOCKS
/// while still being allowed to run commands, or the other way around.
///
/// `file_root` is the directory the `file` protocol is confined to. It must exist; the
/// CLI creates it when it is missing. `default_shell` is the program the `shell`
/// protocol runs when a client does not ask for a specific command.
///
/// # Errors
/// The server fails to start, or if there is an error during the shutdown process.
pub async fn start_egress<T>(
	self_key: SecretKey,
	file_root: &Path,
	default_shell: Option<&str>,
	server_req_timeout: Duration,
	auther: T,
) -> Result<()>
where
	T: Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static,
{
	debug!("Serving as node {}", self_key.public().fmt_short());
	let endpoint = start_iroh_node(self_key).await?;
	let router = egress_router(
		endpoint,
		file_root,
		default_shell,
		server_req_timeout,
		auther,
	)
	.spawn();

	// One listener for the whole process, the same one every accept loop the router runs
	// selects on.
	shutdown_handle().cancelled().await;
	info!("exit signal received, shutting the router down");

	Ok(router.shutdown().await?)
}

/// Builds the router that registers every protocol in [`Protocol::ALL`] on `endpoint`.
///
/// Each protocol is gated by `auther`, which decides per connection whether `id` may use
/// that protocol, so an identity can be limited to SOCKS while still being allowed to run
/// commands, or the other way around. The router is returned unspawned so a caller can
/// bind extra handlers or shut it down on its own schedule; [`start_egress`] is the
/// long-running form.
///
/// `file_root` is the directory the `file` protocol is confined to. It must exist.
/// `default_shell` is the program the `shell` protocol runs when a client does not ask for
/// a specific command.
#[must_use]
pub fn egress_router<T>(
	endpoint: Endpoint,
	file_root: &Path,
	default_shell: Option<&str>,
	server_req_timeout: Duration,
	auther: T,
) -> RouterBuilder
where
	T: Fn(EndpointId, Protocol) -> bool + Send + Sync + 'static,
{
	// The router needs one authorizer per protocol, so the caller's closure is shared
	// rather than moved into the first registration.
	let auther = Arc::new(auther);
	let mut router = RouterBuilder::new(endpoint);

	for protocol in Protocol::ALL {
		let allowed = {
			let auther = Arc::clone(&auther);
			move |id: EndpointId, protocol: Protocol| (*auther)(id, protocol)
		};
		let next = match protocol {
			Protocol::Socks => {
				router.accept_with_auth(protocol, Socks::new(server_req_timeout), allowed)
			}
			Protocol::Tunnel => router.accept_with_auth(protocol, Tunnel::new(), allowed),
			Protocol::ReverseTunnel => {
				router.accept_with_auth(protocol, ReverseTunnel::new(), allowed)
			}
			Protocol::Exec => router.accept_with_auth(protocol, Exec::new(), allowed),
			Protocol::Shell => router.accept_with_auth(
				protocol,
				Shell::new().with_shell(default_shell.map(str::to_owned)),
				allowed,
			),
			Protocol::File => {
				router.accept_with_auth(protocol, File::new(file_root.to_path_buf()), allowed)
			}
		};
		router = next;
		info!("registered the {protocol} protocol");
	}

	// The builder keeps its own handle on the endpoint, so the router it spawns holds the
	// node open for as long as it lives.
	router
}

/// Starts an iroh node with the given secret key and returns the endpoint.
///
/// # Errors
/// The node fails to start or there is an error during initialization.
async fn start_iroh_node(key: SecretKey) -> Result<Endpoint> {
	debug!("Starting iroh node as {}", key.public().fmt_short());
	let endpoint = Endpoint::builder(presets::N0)
		.secret_key(key)
		.bind()
		.await?;
	endpoint.online().await;
	Ok(endpoint)
}
