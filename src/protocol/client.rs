use color_eyre::eyre::Result;
use iroh::{Endpoint, NodeId, endpoint::VarInt};
use tokio::net::TcpListener;

use crate::stream;

use super::ALPN;

pub(crate) async fn handle_req(
	endpoint: Endpoint,
	server_key: NodeId,
	client_listener: &TcpListener,
) -> Result<()> {
	let (client_socket, _) = client_listener.accept().await?;
	tokio::spawn(async move {
		let conn = endpoint.connect(server_key, ALPN).await?;

		let (to1, from1) = conn.open_bi().await?;
		let (from2, to2) = client_socket.into_split();

		stream::copy_bidi_stream(from2, to2, from1, to1).await?;
		conn.close(VarInt::from_u32(1), b"BiDi exit");
		Result::<()>::Ok(())
	})
	.await?
}
