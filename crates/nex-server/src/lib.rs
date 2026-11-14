//! nex relay.
//!
//! Responsibilities:
//! - account and device registry, pairing codes, device revocation (SQLite)
//! - host control channel: devices that are online and what they expose
//! - stream relay: splices a client WebSocket to a host WebSocket
//!
//! Stream setup:
//! ```text
//! client                      relay                         host (nex up)
//!   | POST /v1/tickets ------->  |                              |
//!   | <------------- ticket      |                              |
//!   | WS /v1/connect?ticket -->  | -- Open{stream_id} (ctrl) -> |
//!   |                            | <-- WS /v1/accept?stream_id  |
//!   | <========= bytes / messages spliced both ways =========> |
//! ```

pub mod db;

mod api;
mod relay;
mod util;

use axum::{
    routing::{delete, get, post},
    Router,
};
use nex_proto::{ServerMessage, ServiceInfo};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicU64, Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

#[derive(Debug, Clone)]
pub struct Config {
    /// When set, `POST /v1/bootstrap` requires this value in the
    /// `x-nex-admin-key` header. Leave unset only for local development.
    pub admin_key: Option<String>,
    pub pairing_ttl: Duration,
    pub ticket_ttl: Duration,
    /// How long a client waits for the host to accept a stream.
    pub open_timeout: Duration,
    /// Keepalive ping interval on host control connections.
    pub ping_interval: Duration,
    /// Concurrent streams one device may take part in (as client or host).
    pub max_streams_per_device: usize,
    /// Largest WebSocket message accepted on any connection.
    pub max_message_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            admin_key: None,
            pairing_ttl: Duration::from_secs(600),
            ticket_ttl: Duration::from_secs(60),
            open_timeout: Duration::from_secs(15),
            ping_interval: Duration::from_secs(20),
            max_streams_per_device: 128,
            max_message_bytes: 16 << 20,
        }
    }
}

pub(crate) struct HostEntry {
    pub conn_id: u64,
    pub tx: mpsc::UnboundedSender<ServerMessage>,
    pub services: Vec<ServiceInfo>,
}

pub(crate) struct PendingStream {
    pub host_device_id: String,
    /// Ok(host socket) when the host accepts, Err(reason) when it rejects.
    pub tx: oneshot::Sender<Result<axum::extract::ws::WebSocket, String>>,
}

pub(crate) struct TicketEntry {
    pub from_device_id: String,
    pub target_device_id: String,
    pub service: String,
    pub expires_at: i64,
}

pub struct AppState {
    pub(crate) db: db::Db,
    pub(crate) config: Config,
    pub(crate) hosts: Mutex<HashMap<String, HostEntry>>,
    pub(crate) pending: Mutex<HashMap<String, PendingStream>>,
    /// Keyed by SHA-256 of the ticket.
    pub(crate) tickets: Mutex<HashMap<String, TicketEntry>>,
    /// One token per device; cancelled on revocation to drop its control
    /// connection and every stream it is part of.
    pub(crate) guards: Mutex<HashMap<String, CancellationToken>>,
    /// Open streams per device, for `max_streams_per_device`.
    pub(crate) streams: Mutex<HashMap<String, usize>>,
    pub(crate) next_conn: AtomicU64,
}

/// Holds one stream slot on each of two devices; released on drop.
pub(crate) struct StreamSlots {
    state: Arc<AppState>,
    devices: [String; 2],
}

impl Drop for StreamSlots {
    fn drop(&mut self) {
        let mut streams = lock(&self.state.streams);
        for d in &self.devices {
            if let Some(n) = streams.get_mut(d) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    streams.remove(d);
                }
            }
        }
    }
}

impl AppState {
    pub fn new(db: db::Db, config: Config) -> Arc<Self> {
        Arc::new(Self {
            db,
            config,
            hosts: Mutex::default(),
            pending: Mutex::default(),
            tickets: Mutex::default(),
            guards: Mutex::default(),
            streams: Mutex::default(),
            next_conn: AtomicU64::new(1),
        })
    }

    pub(crate) fn guard(&self, device_id: &str) -> CancellationToken {
        lock(&self.guards)
            .entry(device_id.to_string())
            .or_default()
            .clone()
    }

    /// Reserves a stream slot on both devices, or None if either is at its limit.
    pub(crate) fn acquire_streams(self: &Arc<Self>, a: &str, b: &str) -> Option<StreamSlots> {
        let mut streams = lock(&self.streams);
        let max = self.config.max_streams_per_device;
        let used = |d: &str| streams.get(d).copied().unwrap_or(0);
        let need_a = if a == b { 2 } else { 1 };
        if used(a) + need_a > max || used(b) + 1 > max {
            return None;
        }
        *streams.entry(a.to_string()).or_default() += 1;
        *streams.entry(b.to_string()).or_default() += 1;
        Some(StreamSlots {
            state: self.clone(),
            devices: [a.to_string(), b.to_string()],
        })
    }

    /// Drops everything live for a revoked device.
    pub(crate) fn drop_device(&self, device_id: &str) {
        if let Some(token) = lock(&self.guards).remove(device_id) {
            token.cancel();
        }
        lock(&self.hosts).remove(device_id);
        lock(&self.pending).retain(|_, p| p.host_device_id != device_id);
        lock(&self.tickets)
            .retain(|_, t| t.from_device_id != device_id && t.target_device_id != device_id);
    }
}

/// Poison-tolerant lock: a panicked handler must not wedge the relay.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { axum::response::Html(include_str!("console.html")) }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/bootstrap", post(api::bootstrap))
        .route("/v1/pairing-codes", post(api::create_pairing_code))
        .route("/v1/devices/claim", post(api::claim))
        .route("/v1/devices", get(api::list_devices))
        .route("/v1/me", get(api::whoami))
        .route(
            "/v1/devices/{id}",
            delete(api::revoke_device).patch(api::rename_device),
        )
        .route("/v1/tickets", post(api::create_ticket))
        .route("/v1/host", get(relay::host_ws))
        .route("/v1/accept", get(relay::accept_ws))
        .route("/v1/connect", get(relay::connect_ws))
        .route("/v1/admin/devices", post(api::admin_issue_device))
        .route(
            "/v1/admin/accounts/{external_id}/devices",
            get(api::admin_list_devices),
        )
        .route(
            "/v1/admin/accounts/{external_id}/devices/{device_id}",
            delete(api::admin_revoke_device),
        )
        // Bearer tokens, no cookies: permissive CORS lets web clients call the API.
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}
