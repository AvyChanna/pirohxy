use color_eyre::eyre::{Result, eyre};
use iroh::endpoint::{RecvStream, SendStream};
use tokio::{
	io::{AsyncRead, AsyncWrite},
	signal,
	task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub(super) async fn copy_bidi_stream(
	from1: impl AsyncRead + Send + Sync + Unpin + 'static,
	to1: impl AsyncWrite + Send + Sync + Unpin + 'static,
	from2: RecvStream,
	to2: SendStream,
) -> Result<()> {
	let token1 = CancellationToken::new();
	let token2 = token1.clone();
	let token3 = token1.clone();
	let mut set = JoinSet::new();
	let _forward_from_stdin = set.spawn(async move {
		copy_forward(from1, to2, token1.clone())
			.await
			.map_err(cancel_token(token1))
	});
	let _forward_to_stdout = set.spawn(async move {
		copy_backward(from2, to1, token2.clone())
			.await
			.map_err(cancel_token(token2))
	});
	let _control_c = tokio::spawn(async move {
		signal::ctrl_c().await?;
		token3.cancel();
		Result::<()>::Ok(())
	});
	while let Some(res) = set.join_next().await {
		match res {
			Ok(Ok(_)) => (),
			Ok(Err(err)) => warn!("Error in task: {err}"),
			Err(err) => warn!("Error in task join: {err}"),
		}
	}
	Ok(())
}

fn cancel_token<T>(token: CancellationToken) -> impl Fn(T) -> T {
	move |x| {
		token.cancel();
		x
	}
}

async fn copy_forward(
	mut from1: impl AsyncRead + Unpin,
	mut to2: SendStream,
	token: CancellationToken,
) -> Result<u64> {
	tokio::select! {
		res = tokio::io::copy(&mut from1, &mut to2) => {
			let size = res?;
			to2.finish()?;
			Ok(size)
		}
		() = token.cancelled() => {
			let _ = to2.reset(0u8.into()).ok();
			Err(eyre!("Operation cancelled"))
		}
	}
}

async fn copy_backward(
	mut from2: RecvStream,
	mut to1: impl AsyncWrite + Unpin,
	token: CancellationToken,
) -> Result<u64> {
	tokio::select! {
		res = tokio::io::copy(&mut from2, &mut to1) => {
			Ok(res?)
		}
		() = token.cancelled() => {
			let _ = from2.stop(0u8.into()).ok();
			Err(eyre!("Operation cancelled"))
		}
	}
}
