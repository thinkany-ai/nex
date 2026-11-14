//! WebSocket endpoints: host control channel, stream accept, stream connect.

use crate::{
    api::{ApiError, AuthDevice},
    lock,
    util::{hash_secret, now, random_id},
    AppState, HostEntry, PendingStream, TicketEntry,
};
use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::Response,
};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use nex_proto::{close_code, valid_service_name, HostMessage, ServerMessage, ServiceInfo};
use serde::Deserialize;
use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

// ---------- host control channel ----------

pub async fn host_ws(
    State(state): State<Arc<AppState>>,
    AuthDevice(me): AuthDevice,
    ws: WebSocketUpgrade,
) -> Response {
    limit(ws, &state).on_upgrade(move |socket| run_host(state, me.id, socket))
}

fn validate_services(services: &[ServiceInfo]) -> Result<(), String> {
    if services.len() > 64 {
        return Err("too many services".into());
    }
    let mut seen = HashSet::new();
    for s in services {
        if !valid_service_name(&s.name) {
            return Err(format!("invalid service name '{}'", s.name));
        }
        if !seen.insert(&s.name) {
            return Err(format!("duplicate service '{}'", s.name));
        }
    }
    Ok(())
}

async fn run_host(state: Arc<AppState>, device_id: String, mut socket: WebSocket) {
    let services = match tokio::time::timeout(Duration::from_secs(10), socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<HostMessage>(&text) {
            Ok(HostMessage::Hello { services, .. }) => services,
            _ => return close(socket, close_code::BAD_REQUEST, "expected hello").await,
        },
        _ => return,
    };
    if let Err(reason) = validate_services(&services) {
        return close(socket, close_code::BAD_REQUEST, &reason).await;
    }

    let conn_id = state.next_conn.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel();
    // A newer connection from the same device replaces the old one; dropping
    // the old sender ends the old loop.
    lock(&state.hosts).insert(
        device_id.clone(),
        HostEntry {
            conn_id,
            tx,
            services,
        },
    );
    let guard = state.guard(&device_id);
    let _ = state.db.touch(&device_id);
    tracing::info!(%device_id, conn_id, "host online");

    let ready = ServerMessage::Ready {
        device_id: device_id.clone(),
    };
    let mut ok = send_json(&mut socket, &ready).await;
    let mut ping = tokio::time::interval(state.config.ping_interval);
    ping.tick().await;
    // Pongs (and any other frame) prove the host is still there. Three
    // silent ping periods means it slept or lost the network without a
    // close: take it offline so clients get 4404 instead of waiting.
    let mut last_heard = tokio::time::Instant::now();
    let silence_limit = state.config.ping_interval * 3;

    while ok {
        tokio::select! {
            _ = guard.cancelled() => {
                let _ = socket
                    .send(close_msg(close_code::REVOKED, "device revoked"))
                    .await;
                break;
            }
            out = rx.recv() => match out {
                Some(msg) => ok = send_json(&mut socket, &msg).await,
                // Our sender was dropped: revoked, or a newer connection for
                // this device replaced us. Say which, so the host can stop
                // instead of reconnecting and taking the slot back.
                None => {
                    let (code, reason) = if guard.is_cancelled() {
                        (close_code::REVOKED, "device revoked")
                    } else {
                        (close_code::REPLACED, "another connection for this device took over")
                    };
                    let _ = socket.send(close_msg(code, reason)).await;
                    break;
                }
            },
            incoming = socket.recv() => {
                last_heard = tokio::time::Instant::now();
                match incoming {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<HostMessage>(&text) {
                    Ok(HostMessage::Hello { services, .. }) => match validate_services(&services) {
                        Ok(()) => {
                            if let Some(entry) = lock(&state.hosts).get_mut(&device_id) {
                                if entry.conn_id == conn_id {
                                    entry.services = services;
                                }
                            }
                        }
                        Err(reason) => {
                            let err = ServerMessage::Error { message: reason };
                            ok = send_json(&mut socket, &err).await;
                        }
                    },
                    Ok(HostMessage::Reject { stream_id, reason }) => {
                        let mut pending = lock(&state.pending);
                        if pending.get(&stream_id).is_some_and(|p| p.host_device_id == device_id) {
                            if let Some(p) = pending.remove(&stream_id) {
                                let _ = p.tx.send(Err(reason));
                            }
                        }
                    }
                    Err(e) => tracing::debug!(%device_id, "ignoring bad host frame: {e}"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }},
            _ = ping.tick() => {
                if last_heard.elapsed() > silence_limit {
                    tracing::info!(%device_id, conn_id, "host silent, dropping");
                    break;
                }
                ok = socket.send(Message::Ping(Default::default())).await.is_ok();
            }
        }
    }

    let went_offline = {
        let mut hosts = lock(&state.hosts);
        let ours = hosts.get(&device_id).is_some_and(|h| h.conn_id == conn_id);
        if ours {
            hosts.remove(&device_id);
        }
        ours
    };
    if went_offline {
        // Streams still waiting for this host can no longer be accepted:
        // dropping their senders tells each waiting client "offline" now
        // instead of after the open timeout.
        lock(&state.pending).retain(|_, p| p.host_device_id != device_id);
    }
    let _ = state.db.touch(&device_id);
    tracing::info!(%device_id, conn_id, "host offline");
}

// ---------- stream accept (host side) ----------

#[derive(Deserialize)]
pub struct AcceptQuery {
    stream_id: String,
}

pub async fn accept_ws(
    State(state): State<Arc<AppState>>,
    AuthDevice(me): AuthDevice,
    Query(q): Query<AcceptQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let known = lock(&state.pending)
        .get(&q.stream_id)
        .is_some_and(|p| p.host_device_id == me.id);
    if !known {
        return Err(ApiError::not_found("unknown or expired stream"));
    }
    Ok(limit(ws, &state).on_upgrade(move |socket| async move {
        let pending = {
            let mut pending = lock(&state.pending);
            match pending.get(&q.stream_id) {
                Some(p) if p.host_device_id == me.id => pending.remove(&q.stream_id),
                _ => None,
            }
        };
        match pending {
            Some(p) => {
                // The client task takes ownership and splices.
                let _ = p.tx.send(Ok(socket));
            }
            None => close(socket, close_code::TIMEOUT, "stream expired").await,
        }
    }))
}

// ---------- stream connect (client side) ----------

#[derive(Deserialize)]
pub struct ConnectQuery {
    ticket: String,
}

pub async fn connect_ws(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ConnectQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // Single use: removed whether or not the rest succeeds.
    let entry = lock(&state.tickets)
        .remove(&hash_secret(&q.ticket))
        .ok_or_else(ApiError::unauthorized)?;
    if entry.expires_at <= now()
        || !state.db.device_active(&entry.from_device_id)?
        || !state.db.device_active(&entry.target_device_id)?
    {
        return Err(ApiError::unauthorized());
    }
    Ok(limit(ws, &state).on_upgrade(move |socket| run_client(state, entry, socket)))
}

async fn run_client(state: Arc<AppState>, entry: TicketEntry, socket: WebSocket) {
    let Some(_slots) = state.acquire_streams(&entry.from_device_id, &entry.target_device_id) else {
        return close(socket, close_code::BUSY, "too many open streams").await;
    };
    let host_tx = lock(&state.hosts)
        .get(&entry.target_device_id)
        .map(|h| h.tx.clone());
    let Some(host_tx) = host_tx else {
        return close(socket, close_code::OFFLINE, "device offline").await;
    };

    let stream_id = random_id("str");
    let (tx, rx) = oneshot::channel();
    lock(&state.pending).insert(
        stream_id.clone(),
        PendingStream {
            host_device_id: entry.target_device_id.clone(),
            tx,
        },
    );
    let open = ServerMessage::Open {
        stream_id: stream_id.clone(),
        service: entry.service.clone(),
        from_device_id: entry.from_device_id.clone(),
    };
    if host_tx.send(open).is_err() {
        lock(&state.pending).remove(&stream_id);
        return close(socket, close_code::OFFLINE, "device offline").await;
    }

    match tokio::time::timeout(state.config.open_timeout, rx).await {
        Ok(Ok(Ok(host_socket))) => {
            let a = state.guard(&entry.from_device_id);
            let b = state.guard(&entry.target_device_id);
            tracing::debug!(%stream_id, service = %entry.service, "stream open");
            splice(socket, host_socket, a, b).await;
            tracing::debug!(%stream_id, "stream closed");
        }
        Ok(Ok(Err(reason))) => close(socket, close_code::REJECTED, &reason).await,
        Ok(Err(_)) => close(socket, close_code::OFFLINE, "device went offline").await,
        Err(_) => {
            lock(&state.pending).remove(&stream_id);
            close(
                socket,
                close_code::TIMEOUT,
                "device did not accept the stream",
            )
            .await
        }
    }
}

// ---------- helpers ----------

async fn pump<R, W>(rx: &mut R, tx: &mut W)
where
    R: Stream<Item = Result<Message, axum::Error>> + Unpin,
    W: Sink<Message> + Unpin,
{
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Close(_) => break,
            // Keepalives are per-hop; do not forward them.
            Message::Ping(_) | Message::Pong(_) => {}
            data => {
                if tx.send(data).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Forwards frames both ways until either side closes or either device is revoked.
async fn splice(client: WebSocket, host: WebSocket, a: CancellationToken, b: CancellationToken) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut host_tx, mut host_rx) = host.split();
    tokio::select! {
        _ = pump(&mut client_rx, &mut host_tx) => {}
        _ = pump(&mut host_rx, &mut client_tx) => {}
        _ = a.cancelled() => {}
        _ = b.cancelled() => {}
    }
    let _ = client_tx.send(Message::Close(None)).await;
    let _ = host_tx.send(Message::Close(None)).await;
}

fn limit(ws: WebSocketUpgrade, state: &AppState) -> WebSocketUpgrade {
    let max = state.config.max_message_bytes;
    ws.max_message_size(max).max_frame_size(max)
}

fn close_msg(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: truncate_reason(reason).to_string().into(),
    }))
}

/// A close frame's reason may be at most 123 bytes (125-byte control frame
/// minus the code). Host-supplied reject reasons can be longer; cut them on
/// a UTF-8 boundary instead of producing an invalid frame.
pub(crate) fn truncate_reason(reason: &str) -> &str {
    const MAX: usize = 123;
    if reason.len() <= MAX {
        return reason;
    }
    let mut end = MAX;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    &reason[..end]
}

async fn close(mut socket: WebSocket, code: u16, reason: &str) {
    let _ = socket.send(close_msg(code, reason)).await;
}

async fn send_json<T: serde::Serialize>(socket: &mut WebSocket, msg: &T) -> bool {
    match serde_json::to_string(msg) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_reason;

    #[test]
    fn reasons_fit_in_a_close_frame() {
        assert_eq!(truncate_reason("short"), "short");
        let long = "本地服务不可用".repeat(20);
        let cut = truncate_reason(&long);
        assert!(cut.len() <= 123 && long.starts_with(cut) && !cut.is_empty());
    }
}
