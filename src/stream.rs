use color_eyre::eyre::Result;
use iroh::endpoint::{RecvStream, SendStream};
use tokio::{
	io::{AsyncRead, AsyncWrite, AsyncWriteExt},
	task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::shutdown_signal;

enum CopyResult {
	Completed,
	Cancelled,
}

/// Copies data between two streams in both directions until one of them is closed or an error occurs.
pub(super) async fn copy_bidi_stream(
	from1: impl AsyncRead + Send + Sync + Unpin + 'static,
	to1: impl AsyncWrite + Send + Sync + Unpin + 'static,
	from2: RecvStream,
	to2: SendStream,
) {
	let token = CancellationToken::new();
	let token1 = token.clone();
	let token2 = token.clone();
	let mut set = JoinSet::new();
	let _forward_from_stdin = set.spawn(async move { copy_forward(from1, to2, token1).await });
	let _forward_to_stdout = set.spawn(async move { copy_backward(from2, to1, token2).await });

	let shutdown = shutdown_signal();
	tokio::pin!(shutdown);
	let mut shutdown_observed = false;

	while !set.is_empty() {
		tokio::select! {
			res = set.join_next(), if !set.is_empty() => {
				if let Some(res) = res {
					match res {
						Ok(Ok(CopyResult::Completed)) => debug!("Copy task completed"),
						Ok(Ok(CopyResult::Cancelled)) => debug!("Copy task cancelled"),
						Ok(Err(err)) => debug!("Error in task: {err}"),
						Err(err) => debug!("Error in task join: {err}"),
					}

					// Stop the opposite copy direction as soon as one finishes or fails.
					token.cancel();
				}
			}
			res = &mut shutdown, if !shutdown_observed => {
				shutdown_observed = true;
				if let Err(err) = res {
					debug!("Failed while waiting for shutdown signal: {err}");
				}
				token.cancel();
			}
		}
	}
}

async fn copy_forward(
	mut from1: impl AsyncRead + Unpin,
	mut to2: SendStream,
	token: CancellationToken,
) -> Result<CopyResult> {
	tokio::select! {
		res = tokio::io::copy(&mut from1, &mut to2) => {
			let _ = res?;
			to2.finish()?;
			Ok(CopyResult::Completed)
		}
		() = token.cancelled() => {
			let _ = to2.reset(0u8.into()).ok();
			Ok(CopyResult::Cancelled)
		}
	}
}

async fn copy_backward(
	mut from2: RecvStream,
	mut to1: impl AsyncWrite + Unpin,
	token: CancellationToken,
) -> Result<CopyResult> {
	tokio::select! {
		res = tokio::io::copy(&mut from2, &mut to1) => {
			let _ = res?;
			to1.shutdown().await?;
			Ok(CopyResult::Completed)
		}
		() = token.cancelled() => {
			let _ = from2.stop(0u8.into()).ok();
			Ok(CopyResult::Cancelled)
		}
	}
}
