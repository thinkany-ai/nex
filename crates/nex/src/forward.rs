use crate::{client::NexClient, pipe};
use anyhow::Result;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Accepts local TCP connections on `listener` and tunnels each one to the
/// TCP `service` on `device` (id or name), each over its own stream.
pub async fn forward_tcp(
    client: NexClient,
    device: String,
    service: String,
    listener: TcpListener,
    cancel: CancellationToken,
) -> Result<()> {
    loop {
        let (socket, peer) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            accepted = listener.accept() => accepted?,
        };
        let client = client.clone();
        let (device, service) = (device.clone(), service.clone());
        tokio::spawn(async move {
            match client.connect(&device, &service).await {
                Ok(ws) => pipe::ws_tcp(ws, socket).await,
                Err(e) => tracing::warn!(%peer, "cannot reach {device}/{service}: {e:#}"),
            }
        });
    }
}
