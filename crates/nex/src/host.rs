//! Keeps a device online on the relay and serves streams to its local services.

use crate::{client::NexClient, pipe};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use nex_proto::{
    close_code, valid_service_name, HostMessage, ServerMessage, ServiceInfo, ServiceKind,
    PROTOCOL_VERSION,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, Message},
};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Where a service lives on this machine.
#[derive(Debug, Clone)]
pub enum Target {
    /// `host:port` of a local TCP server.
    Tcp(String),
    /// URL of a local WebSocket server (`ws://` or `wss://`).
    Ws(Url),
    /// Built-in WebSocket echo, for connectivity and latency tests.
    Echo,
}

#[derive(Debug, Clone)]
pub struct ExposedService {
    pub name: String,
    pub target: Target,
}

impl ExposedService {
    pub fn tcp(name: &str, addr: &str) -> Result<Self> {
        Self::check_name(name)?;
        Ok(Self {
            name: name.into(),
            target: Target::Tcp(addr.into()),
        })
    }

    pub fn ws(name: &str, url: &str) -> Result<Self> {
        Self::check_name(name)?;
        Ok(Self {
            name: name.into(),
            target: Target::Ws(Url::parse(url)?),
        })
    }

    /// The built-in `echo` service (a `ws` service that sends every message back).
    pub fn echo() -> Self {
        Self {
            name: "echo".into(),
            target: Target::Echo,
        }
    }

    /// `name=tcp://127.0.0.1:22` or `name=ws://127.0.0.1:8790/ws`.
    pub fn parse(spec: &str) -> Result<Self> {
        let (name, target) = spec
            .split_once('=')
            .ok_or_else(|| anyhow!("expected NAME=URL, got '{spec}'"))?;
        if let Some(addr) = target.strip_prefix("tcp://") {
            Self::tcp(name, addr.trim_end_matches('/'))
        } else if target.starts_with("ws://") || target.starts_with("wss://") {
            Self::ws(name, target)
        } else {
            bail!("unsupported target '{target}': use tcp://HOST:PORT or ws://HOST:PORT/PATH")
        }
    }

    fn check_name(name: &str) -> Result<()> {
        if !valid_service_name(name) {
            bail!("invalid service name '{name}': use 1-64 chars of A-Z a-z 0-9 . _ -");
        }
        Ok(())
    }

    pub fn info(&self) -> ServiceInfo {
        ServiceInfo {
            name: self.name.clone(),
            kind: match self.target {
                Target::Tcp(_) => ServiceKind::Tcp,
                Target::Ws(_) | Target::Echo => ServiceKind::Ws,
            },
        }
    }
}

enum SessionEnd {
    Cancelled,
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

pub struct Host {
    client: NexClient,
    services: Vec<ExposedService>,
}

impl Host {
    pub fn new(client: NexClient, services: Vec<ExposedService>) -> Self {
        Self { client, services }
    }

    /// Stays online until `cancel` fires, reconnecting with backoff.
    /// Returns an error only for unrecoverable conditions, such as the
    /// device being revoked.
    pub async fn run(self, cancel: CancellationToken) -> Result<()> {
        let host = Arc::new(self);
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            match host.clone().session(&cancel).await {
                SessionEnd::Cancelled => return Ok(()),
                SessionEnd::Fatal(e) => return Err(e),
                SessionEnd::Retry(e) => tracing::warn!("relay connection lost: {e:#}"),
            }
            if started.elapsed() > Duration::from_secs(30) {
                backoff = Duration::from_secs(1);
            }
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    async fn session(self: Arc<Self>, cancel: &CancellationToken) -> SessionEnd {
        let ws = match self.client.ws_authed("v1/host").await {
            Ok(ws) => ws,
            Err(tungstenite::Error::Http(resp)) if resp.status().as_u16() == 401 => {
                return SessionEnd::Fatal(anyhow!(
                    "the relay rejected this device's credential (revoked?)"
                ))
            }
            Err(e) => return SessionEnd::Retry(e.into()),
        };
        let (mut sink, mut stream) = ws.split();

        let hello = HostMessage::Hello {
            protocol: PROTOCOL_VERSION,
            services: self.services.iter().map(ExposedService::info).collect(),
        };
        if let Err(e) = sink.send(json(&hello)).await {
            return SessionEnd::Retry(e.into());
        }

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<HostMessage>();
        // The relay pings every 20s; silence for much longer means the
        // connection died without a close (sleep, network switch).
        const LIVENESS: Duration = Duration::from_secs(65);
        let mut deadline = tokio::time::Instant::now() + LIVENESS;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    return SessionEnd::Retry(anyhow!("no traffic from the relay for {}s", LIVENESS.as_secs()));
                }
                _ = cancel.cancelled() => {
                    let _ = sink.send(Message::Close(None)).await;
                    return SessionEnd::Cancelled;
                }
                Some(out) = out_rx.recv() => {
                    if let Err(e) = sink.send(json(&out)).await {
                        return SessionEnd::Retry(e.into());
                    }
                }
                msg = stream.next() => {
                    deadline = tokio::time::Instant::now() + LIVENESS;
                    match msg {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text) {
                        Ok(ServerMessage::Ready { device_id }) => {
                            tracing::info!(%device_id, "online");
                        }
                        Ok(ServerMessage::Open { stream_id, service, from_device_id }) => {
                            tracing::debug!(%stream_id, %service, %from_device_id, "incoming stream");
                            tokio::spawn(self.clone().serve_stream(stream_id, service, out_tx.clone()));
                        }
                        Ok(ServerMessage::Error { message }) => {
                            tracing::warn!("relay: {message}");
                        }
                        Err(e) => tracing::debug!("ignoring unknown relay frame: {e}"),
                    },
                    Some(Ok(Message::Close(frame))) => {
                        let reason = frame
                            .as_ref()
                            .map(|f| f.reason.to_string())
                            .unwrap_or_else(|| "closed".into());
                        let code = frame.map(|f| u16::from(f.code));
                        return match code {
                            Some(close_code::REVOKED) => SessionEnd::Fatal(anyhow!("this device was revoked")),
                            Some(close_code::BAD_REQUEST) => SessionEnd::Fatal(anyhow!("relay refused: {reason}")),
                            Some(close_code::REPLACED) => SessionEnd::Fatal(anyhow!(
                                "another process went online with this device's credential; each process needs its own device"
                            )),
                            _ => SessionEnd::Retry(anyhow!("relay closed the connection: {reason}")),
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return SessionEnd::Retry(e.into()),
                    None => return SessionEnd::Retry(anyhow!("relay connection ended")),
                }},
            }
        }
    }

    async fn serve_stream(
        self: Arc<Self>,
        stream_id: String,
        service: String,
        out: mpsc::UnboundedSender<HostMessage>,
    ) {
        let reject = |reason: String| {
            tracing::warn!(%stream_id, %service, "rejecting stream: {reason}");
            let _ = out.send(HostMessage::Reject {
                stream_id: stream_id.clone(),
                reason,
            });
        };
        let Some(svc) = self.services.iter().find(|s| s.name == service) else {
            return reject(format!("unknown service '{service}'"));
        };
        let accept_path = format!("v1/accept?stream_id={stream_id}");

        // Connect to the local service first so a dead service is reported
        // to the caller as a rejection instead of an empty stream.
        match &svc.target {
            Target::Tcp(addr) => {
                let local = match TcpStream::connect(addr).await.context(addr.clone()) {
                    Ok(s) => s,
                    Err(e) => return reject(format!("local service unavailable: {e:#}")),
                };
                match self.client.ws_authed(&accept_path).await {
                    Ok(ws) => pipe::ws_tcp(ws, local).await,
                    Err(e) => tracing::warn!(%stream_id, "accept failed: {e}"),
                }
            }
            Target::Ws(url) => {
                let local = match connect_async(url.as_str()).await {
                    Ok((s, _)) => s,
                    Err(e) => return reject(format!("local service unavailable: {e}")),
                };
                match self.client.ws_authed(&accept_path).await {
                    Ok(ws) => pipe::ws_ws(ws, local).await,
                    Err(e) => tracing::warn!(%stream_id, "accept failed: {e}"),
                }
            }
            Target::Echo => match self.client.ws_authed(&accept_path).await {
                Ok(ws) => pipe::echo(ws).await,
                Err(e) => tracing::warn!(%stream_id, "accept failed: {e}"),
            },
        }
    }
}

fn json<T: serde::Serialize>(msg: &T) -> Message {
    Message::Text(serde_json::to_string(msg).expect("serializable").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_specs() {
        let s = ExposedService::parse("ssh=tcp://127.0.0.1:22").unwrap();
        assert!(matches!(s.target, Target::Tcp(ref a) if a == "127.0.0.1:22"));
        let s = ExposedService::parse("agl=ws://127.0.0.1:8790/ws").unwrap();
        assert_eq!(s.info().kind, ServiceKind::Ws);
        assert!(ExposedService::parse("bad name=tcp://x:1").is_err());
        assert!(ExposedService::parse("x=http://x").is_err());
        assert!(ExposedService::parse("nothing").is_err());
    }
}
