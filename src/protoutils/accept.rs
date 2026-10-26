//! The accept loop every server protocol shares.
//!
//! A handler is handed a connection the auth layer has already approved, and each of them
//! does the same thing with it: name the peer, accept streams until the connection closes,
//! and run every stream as its own task so a slow or broken stream cannot hold up the next
//! one. Only the stream handler and the label it reports under differ, so the loop lives
//! here once instead of six times.
//!
//! The tasks one connection spawns are tracked in a [`Tasks`] set, which is what lets the
//! loop notice when it has taken on too much and what ends the streams still running when
//! the connection is gone. If the router cancels this future instead - that is how
//! `Router::shutdown` stops an accept loop - the set is dropped with it and aborts them.

use core::fmt::Display;

use iroh::{
	endpoint::{Connection, RecvStream, SendStream},
	protocol::AcceptError,
};
use tracing::{debug, trace};

use super::tasks::Tasks;

/// Accepts bidirectional streams on `conn` until the connection closes.
///
/// `handle` runs one stream, and it runs in its own task: the loop keeps accepting while
/// the stream is in flight, which is what lets several forwards share one connection.
/// `name` labels the stream in the line written when a handler fails, so a log still says
/// which protocol it came from. It is `&'static str` because the task that writes that line
/// outlives this loop.
///
/// Streams still in flight when the connection closes are given the drain grace period in
/// [`Tasks::drain`], then reset.
///
/// # Errors
/// Accepting failed, which means `conn` has closed and there is nothing left to serve.
pub async fn serve_bi_streams<F, Fut, E>(
	conn: &Connection,
	name: &'static str,
	handle: F,
) -> Result<(), AcceptError>
where
	F: Fn(RecvStream, SendStream) -> Fut + Clone + Send + 'static,
	Fut: Future<Output = Result<(), E>> + Send + 'static,
	E: Display,
{
	trace!("Accepted connection from {}", conn.remote_id().fmt_short());
	let mut streams = Tasks::new(name);
	let accepted = loop {
		match conn.accept_bi().await {
			Ok((send, recv)) => {
				// One clone of the handler per stream: the task owns its own copy, so the
				// loop is free to accept the next stream immediately. The future is boxed
				// because a protocol handler carries a lot of state, and this task should
				// not carry it on its own frame.
				let handle = handle.clone();
				streams
					.spawn(async move {
						let handler = Box::pin(handle(recv, send));
						if let Err(err) = handler.await {
							debug!("Failed to process {name} stream: {err}");
						}
					})
					.await;
			}
			Err(err) => break Err(err.into()),
		}
	};
	streams.drain().await;
	accepted
}

/// Accepts unidirectional streams on `conn` until the connection closes.
///
/// The [`serve_bi_streams`] form, for the protocols whose request arrives on a one-way
/// stream. `handle` gets the stream only; whatever it needs from `conn` it takes before
/// the stream task starts.
///
/// # Errors
/// Accepting failed, which means `conn` has closed and there is nothing left to serve.
pub async fn serve_uni_streams<F, Fut, E>(
	conn: &Connection,
	name: &'static str,
	handle: F,
) -> Result<(), AcceptError>
where
	F: Fn(RecvStream) -> Fut + Clone + Send + 'static,
	Fut: Future<Output = Result<(), E>> + Send + 'static,
	E: Display,
{
	trace!("Accepted connection from {}", conn.remote_id().fmt_short());
	let mut streams = Tasks::new(name);
	let accepted = loop {
		match conn.accept_uni().await {
			Ok(recv) => {
				let handle = handle.clone();
				streams
					.spawn(async move {
						let handler = Box::pin(handle(recv));
						if let Err(err) = handler.await {
							debug!("Failed to process {name} stream: {err}");
						}
					})
					.await;
			}
			Err(err) => break Err(err.into()),
		}
	};
	streams.drain().await;
	accepted
}
