//! The one place this process listens for an exit signal.
//!
//! A signal handler is process-wide whether the code installing it means it to be or not:
//! the first registration replaces the platform's default behaviour for the rest of the
//! process, and dropping the listener does not put it back. Installing one per accepted
//! stream therefore means N listeners, N wakeups per Ctrl-C, and a second Ctrl-C that no
//! longer ends the process - the default behaviour was already replaced by the first one.
//!
//! So the listener lives here once. Every loop that needs to know the process is going
//! away is handed a [`CancellationToken`] from the same [`Shutdown`], and passes it to the
//! work it spawns.
//!
//! The second signal is the escape hatch. Since the default behaviour is gone, a second
//! Ctrl-C would otherwise be caught by the very listener that is already draining the
//! work, so it is taken to mean "stop now" and ends the process.

use std::{io, sync::OnceLock};

use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{trace, warn};

/// The process's exit signal, heard once and shared by every loop.
#[derive(Clone, Debug)]
pub(crate) struct Shutdown {
	token: CancellationToken,
}

/// The [`Shutdown`] every loop in this process shares.
///
/// It is built on first use, so a command that serves several channels - or a test that
/// runs several of them - still registers exactly one signal listener.
pub(crate) fn shutdown_handle() -> Shutdown {
	static SHUTDOWN: OnceLock<Shutdown> = OnceLock::new();
	SHUTDOWN.get_or_init(Shutdown::new).clone()
}

impl Shutdown {
	/// Installs the listener and starts the task that reports what it hears.
	///
	/// Called from inside a runtime: the listener is a task, and it is the only one. A
	/// failure to register is reported rather than returned, because a process that cannot
	/// hear Ctrl-C still works - it just has to be stopped some other way.
	fn new() -> Self {
		let token = CancellationToken::new();
		let closing = token.clone();

		let _listener = tokio::spawn(async move {
			match exit_signal().await {
				Ok(()) => trace!("got exit signal"),
				Err(err) => warn!("failed to listen for the exit signal: {err}"),
			}
			closing.cancel();

			// The default behaviour was replaced by the registration above, so this is the
			// only way a second Ctrl-C can end the process: it stops waiting for the work
			// still draining and exits.
			if exit_signal().await.is_ok() {
				warn!("second exit signal: ending the process without waiting");
				std::process::exit(1);
			}
		});

		Self { token }
	}

	/// A handle for one loop: cancelled as soon as the process is asked to exit.
	#[must_use]
	pub(crate) fn token(&self) -> CancellationToken {
		self.token.clone()
	}

	/// Resolves once the process is asked to exit.
	pub(crate) async fn cancelled(&self) {
		self.token.cancelled().await;
	}
}

/// Waits for the platform's exit signal: Ctrl-C everywhere, SIGTERM as well on Unix.
async fn exit_signal() -> Result<(), io::Error> {
	#[cfg(unix)]
	{
		let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;
		tokio::select! {
			res = signal::ctrl_c() => res?,
			_ = terminate.recv() => {}
		}
	}

	#[cfg(not(unix))]
	signal::ctrl_c().await?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::shutdown_handle;

	#[tokio::test]
	async fn every_loop_shares_one_exit_signal() {
		let first = shutdown_handle();
		let second = shutdown_handle();

		assert_eq!(
			first.token(),
			second.token(),
			"the handle is shared, so one Ctrl-C reaches every loop"
		);
		assert!(!first.token().is_cancelled(), "no signal was sent");
	}
}
