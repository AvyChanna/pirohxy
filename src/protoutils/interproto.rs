use color_eyre::eyre::Result;
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};

use super::stream::{ReadStructExt, WriteStructExt};

/// Spawns an interactive message handler with bidi message passing.
/// This creates a pair of tokio channels for bidi communication
///
/// # Errors
/// Returns an error if the channels cannot be created.
pub async fn spawn_interactive<F, T>(
	mut recv_stream: RecvStream,
	mut send_stream: SendStream,
) -> Result<(tokio::sync::mpsc::Receiver<F>, tokio::sync::mpsc::Sender<T>)>
where
	F: Send + 'static + Serialize + for<'de> Deserialize<'de>,
	T: Send + Sync + 'static + Serialize + for<'de> Deserialize<'de>,
{
	let (tx_from_remote, rx_from_remote) = tokio::sync::mpsc::channel::<F>(32);
	let (tx_to_remote, mut rx_to_remote) = tokio::sync::mpsc::channel::<T>(32);

	drop(tokio::spawn(async move {
		while let Ok(msg) = recv_stream.read_struct::<F>().await {
			if tx_from_remote.send(msg).await.is_err() {
				break;
			}
		}
	}));

	drop(tokio::spawn(async move {
		while let Some(msg) = rx_to_remote.recv().await {
			if send_stream.write_struct(&msg).await.is_err() {
				break;
			}
		}
	}));

	Ok((rx_from_remote, tx_to_remote))
}
