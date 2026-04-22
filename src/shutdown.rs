use std::io;

use tokio::signal;
use tracing::trace;

#[cfg(unix)]
pub(crate) async fn shutdown_signal() -> Result<(), io::Error> {
	let ctrl_c = async { signal::ctrl_c().await };

	let terminate = async {
		let _ = signal::unix::signal(signal::unix::SignalKind::terminate())?
			.recv()
			.await;
		Ok::<(), io::Error>(())
	};

	tokio::select! {
		res = ctrl_c => {
			res?
		},
		res = terminate => {
			res?
		},
	}

	trace!("got shutdown signal");
	Ok(())
}

#[cfg(not(unix))]
pub(crate) async fn shutdown_signal() -> Result<(), io::Error> {
	signal::ctrl_c().await?;
	trace!("got shutdown signal");
	Ok(())
}
