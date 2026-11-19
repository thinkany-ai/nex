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
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const HOST_QUEUE_CAPACITY: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

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

    let conn_id = random_id("conn");
    let (tx, mut rx) = mpsc::channel(HOST_QUEUE_CAPACITY);
    let guard = state.guard(&device_id);
    {
        let _registration = state.registration.lock().await;
        match state.claim_host(&device_id, &conn_id, &services).await {
            Ok(true) => {}
            Ok(false) => return close(socket, close_code::REVOKED, "device revoked").await,
            Err(err) => {
                tracing::warn!(%err, "could not register host");
                return;
            }
        }
        lock(&state.hosts).insert(
            device_id.clone(),
            HostEntry {
                conn_id: conn_id.clone(),
                tx,
            },
        );
    }
    let _ = state.db.touch(&device_id).await;
    tracing::info!(%device_id, %conn_id, "host online");

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
                send_close(&mut socket, close_msg(close_code::REVOKED, "device revoked")).await;
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
                    send_close(&mut socket, close_msg(code, reason)).await;
                    break;
                }
            },
            incoming = socket.recv() => {
                last_heard = tokio::time::Instant::now();
                match incoming {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<HostMessage>(&text) {
                    Ok(HostMessage::Hello { services, .. }) => match validate_services(&services) {
                        Ok(()) => {
                            if let Err(err) = state.update_host(&device_id, &conn_id, &services).await {
                                tracing::warn!(%err, "could not update host services");
                                break;
                            }
                        }
                        Err(reason) => {
                            let err = ServerMessage::Error { message: reason };
                            ok = send_json(&mut socket, &err).await;
                        }
                    },
                    Ok(HostMessage::Reject { stream_id, reason }) => {
                        let mut pending = lock(&state.pending);
                        if pending.get(&stream_id).is_some_and(|p| p.host_device_id == device_id && p.host_conn_id == conn_id) {
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
                    tracing::info!(%device_id, %conn_id, "host silent, dropping");
                    break;
                }
                ok = send_frame(&mut socket, Message::Ping(Default::default()), WRITE_TIMEOUT).await;
            }
        }
    }

    {
        let mut hosts = lock(&state.hosts);
        if hosts.get(&device_id).is_some_and(|h| h.conn_id == conn_id) {
            hosts.remove(&device_id);
        }
    }
    lock(&state.pending).retain(|_, p| p.host_conn_id != conn_id);
    if let Err(err) = state.release_host(&device_id, &conn_id).await {
        lock(&state.closed_hosts).insert(conn_id.clone());
        tracing::warn!(%err, "could not release host");
    }
    let _ = state.db.touch(&device_id).await;
    tracing::info!(%device_id, %conn_id, "host offline");
}

// ---------- stream accept (host side) ----------

#[derive(Deserialize)]
pub struct AcceptQuery {
    pub(crate) stream_id: String,
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
    pub(crate) ticket: String,
}

pub async fn connect_ws(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ConnectQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // Single use: removed whether or not the rest succeeds.
    let entry = state
        .consume_ticket(&hash_secret(&q.ticket))
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    if entry.expires_at <= now()
        || !state.db.device_active(&entry.from_device_id).await?
        || !state.db.device_active(&entry.target_device_id).await?
    {
        return Err(ApiError::unauthorized());
    }
    Ok(limit(ws, &state).on_upgrade(move |socket| run_client(state, entry, socket)))
}

async fn run_client(state: Arc<AppState>, entry: TicketEntry, socket: WebSocket) {
    let stream_id = random_id("str");
    let a = state.guard(&entry.from_device_id);
    let b = state.guard(&entry.target_device_id);
    match state.reserve_stream(&stream_id, &entry).await {
        Ok(true) => {}
        Ok(false) => return close(socket, close_code::BUSY, "too many open streams").await,
        Err(err) => {
            tracing::warn!(%err, "could not reserve stream");
            return close(socket, close_code::OFFLINE, "relay unavailable").await;
        }
    }
    relay_stream(&state, &entry, &stream_id, socket, &a, &b).await;
    if let Err(err) = state.release_stream(&stream_id).await {
        lock(&state.closed_streams).insert(stream_id);
        tracing::warn!(%err, "could not release stream reservation");
    }
}

async fn relay_stream(
    state: &AppState,
    entry: &TicketEntry,
    stream_id: &str,
    socket: WebSocket,
    a: &CancellationToken,
    b: &CancellationToken,
) {
    let owner = match state.local_host_connection(&entry.target_device_id).await {
        Ok(Some(owner)) => owner,
        Ok(None) => return close(socket, close_code::OFFLINE, "device offline").await,
        Err(err) => {
            tracing::warn!(%err, "could not look up host owner");
            return close(socket, close_code::OFFLINE, "relay unavailable").await;
        }
    };
    let local_host = || {
        lock(&state.hosts)
            .get(&entry.target_device_id)
            .filter(|host| host.conn_id == owner)
            .map(|host| (host.conn_id.clone(), host.tx.clone()))
    };
    let host = match local_host() {
        Some(host) => Some(host),
        None => {
            // The owner may have committed its claim and be about to insert
            // the local sender. Wait only on this registration slow path.
            let _registration = state.registration.lock().await;
            local_host()
        }
    };
    let Some((host_conn_id, host_tx)) = host else {
        return close(socket, close_code::OFFLINE, "device offline").await;
    };

    let (tx, rx) = oneshot::channel();
    lock(&state.pending).insert(
        stream_id.to_owned(),
        PendingStream {
            host_device_id: entry.target_device_id.clone(),
            host_conn_id,
            tx,
        },
    );
    let open = ServerMessage::Open {
        stream_id: stream_id.to_owned(),
        service: entry.service.clone(),
        from_device_id: entry.from_device_id.clone(),
    };
    let sent = host_tx.try_send(open);
    drop(host_tx);
    if let Err(error) = sent {
        lock(&state.pending).remove(stream_id);
        let (code, reason) = match error {
            mpsc::error::TrySendError::Full(_) => {
                (close_code::BUSY, "device control queue is full")
            }
            mpsc::error::TrySendError::Closed(_) => (close_code::OFFLINE, "device offline"),
        };
        return close(socket, code, reason).await;
    }

    let accepted = tokio::select! {
        result = tokio::time::timeout(state.config.open_timeout, rx) => Some(result),
        _ = a.cancelled() => None,
        _ = b.cancelled() => None,
    };
    lock(&state.pending).remove(stream_id);
    match accepted {
        Some(Ok(Ok(Ok(host_socket)))) => {
            tracing::debug!(%stream_id, service = %entry.service, "stream open");
            splice(socket, host_socket, a, b).await;
            tracing::debug!(%stream_id, "stream closed");
        }
        Some(Ok(Ok(Err(reason)))) => close(socket, close_code::REJECTED, &reason).await,
        Some(Ok(Err(_))) => close(socket, close_code::OFFLINE, "device went offline").await,
        Some(Err(_)) => {
            close(
                socket,
                close_code::TIMEOUT,
                "device did not accept the stream",
            )
            .await
        }
        None => close(socket, close_code::REVOKED, "device revoked").await,
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
                if !send_frame(tx, data, WRITE_TIMEOUT).await {
                    break;
                }
            }
        }
    }
}

/// Forwards frames both ways until either side closes or either device is revoked.
async fn splice(client: WebSocket, host: WebSocket, a: &CancellationToken, b: &CancellationToken) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut host_tx, mut host_rx) = host.split();
    tokio::select! {
        _ = pump(&mut client_rx, &mut host_tx) => {}
        _ = pump(&mut host_rx, &mut client_tx) => {}
        _ = a.cancelled() => {}
        _ = b.cancelled() => {}
    }
    tokio::join!(
        send_close(&mut client_tx, Message::Close(None)),
        send_close(&mut host_tx, Message::Close(None)),
    );
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

async fn send_frame<S: Sink<Message> + Unpin>(
    sink: &mut S,
    frame: Message,
    timeout: Duration,
) -> bool {
    matches!(
        tokio::time::timeout(timeout, sink.send(frame)).await,
        Ok(Ok(()))
    )
}

async fn send_close<S: Sink<Message> + Unpin>(sink: &mut S, frame: Message) {
    send_frame(sink, frame, CLOSE_TIMEOUT).await;
}

async fn close(mut socket: WebSocket, code: u16, reason: &str) {
    send_close(&mut socket, close_msg(code, reason)).await;
}

async fn send_json<T: serde::Serialize>(socket: &mut WebSocket, msg: &T) -> bool {
    match serde_json::to_string(msg) {
        Ok(text) => send_frame(socket, Message::Text(text.into()), WRITE_TIMEOUT).await,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{send_frame, truncate_reason};
    use axum::extract::ws::Message;
    use std::time::Duration;

    #[tokio::test]
    async fn stalled_socket_write_has_a_deadline() {
        let mut stalled = futures_util::sink::unfold((), |(), _: Message| {
            Box::pin(std::future::pending::<Result<(), std::io::Error>>())
        });
        assert!(
            !send_frame(
                &mut stalled,
                Message::Close(None),
                Duration::from_millis(20)
            )
            .await
        );
    }

    #[test]
    fn reasons_fit_in_a_close_frame() {
        assert_eq!(truncate_reason("short"), "short");
        let long = "本地服务不可用".repeat(20);
        let cut = truncate_reason(&long);
        assert!(cut.len() <= 123 && long.starts_with(cut) && !cut.is_empty());
    }
}
