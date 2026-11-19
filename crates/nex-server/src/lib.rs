//! nex relay.
//!
//! Responsibilities:
//! - tenancy (Postgres): console users own apps, an app (one agent product)
//!   owns accounts (its end users), an account owns devices
//! - web console at `/admin`: apps, app keys, devices, users
//! - account and device registry, pairing codes, device revocation
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

pub mod console;
pub mod db;

mod api;
mod cluster;
mod peer;
mod relay;
mod util;

use axum::{
    middleware,
    serve::ListenerExt,
    routing::{delete, get, post},
    Router,
};
use nex_proto::ServerMessage;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

#[derive(Debug, Clone)]
pub struct Config {
    /// Legacy single key: when set, it is an extra app key of the built-in
    /// default app. Products should use app keys from the console instead.
    pub admin_key: Option<String>,
    /// Let `POST /v1/bootstrap` without any key create accounts in the
    /// default app. Local development and tests only.
    pub open_bootstrap: bool,
    /// The console's public origin (`https://…`): console calls must come
    /// from it, and https makes the session cookie Secure. None: the
    /// request's own host is trusted (local development).
    pub public_url: Option<String>,
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
    /// Address advertised to other relay instances for private upgrades.
    pub peer_addr: Option<SocketAddr>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            admin_key: None,
            open_bootstrap: false,
            public_url: None,
            pairing_ttl: Duration::from_secs(600),
            ticket_ttl: Duration::from_secs(60),
            open_timeout: Duration::from_secs(15),
            ping_interval: Duration::from_secs(20),
            max_streams_per_device: 128,
            max_message_bytes: 16 << 20,
            peer_addr: None,
        }
    }
}

pub(crate) struct HostEntry {
    pub conn_id: String,
    pub tx: mpsc::Sender<ServerMessage>,
}

pub(crate) struct PendingStream {
    pub host_device_id: String,
    pub host_conn_id: String,
    /// Ok(host socket) when the host accepts, Err(reason) when it rejects.
    pub tx: oneshot::Sender<Result<axum::extract::ws::WebSocket, String>>,
}

#[derive(sqlx::FromRow)]
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
    pub(crate) instance_id: String,
    pub(crate) closed_hosts: Mutex<HashSet<String>>,
    pub(crate) closed_streams: Mutex<HashSet<String>>,
    pub(crate) registration: tokio::sync::Mutex<()>,
    /// Active local control channels and streams hold a strong reference.
    pub(crate) guards: Mutex<HashMap<String, Weak<CancellationToken>>>,
}

impl AppState {
    pub fn new(db: db::Db, config: Config) -> Arc<Self> {
        Arc::new(Self {
            db,
            config,
            hosts: Mutex::default(),
            pending: Mutex::default(),
            instance_id: util::random_id("relay"),
            closed_hosts: Mutex::default(),
            closed_streams: Mutex::default(),
            registration: tokio::sync::Mutex::default(),
            guards: Mutex::default(),
        })
    }

    pub(crate) fn guard(&self, device_id: &str) -> Arc<CancellationToken> {
        let mut guards = lock(&self.guards);
        if let Some(token) = guards.get(device_id).and_then(Weak::upgrade) {
            return token;
        }
        let token = Arc::new(CancellationToken::new());
        guards.insert(device_id.to_string(), Arc::downgrade(&token));
        token
    }

    /// Disconnect locally immediately. Other replicas reconcile against Postgres.
    pub(crate) fn drop_device(&self, device_id: &str) {
        if let Some(token) = lock(&self.guards)
            .remove(device_id)
            .and_then(|g| g.upgrade())
        {
            token.cancel();
        }
        lock(&self.hosts).remove(device_id);
        lock(&self.pending).retain(|_, p| p.host_device_id != device_id);
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
        .merge(console::routes())
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
        .merge(
            stream_routes().route_layer(middleware::from_fn_with_state(state.clone(), peer::route)),
        )
        .route("/v1/admin/devices", post(api::admin_issue_device))
        .route(
            "/v1/admin/accounts/{external_id}/devices",
            get(api::admin_list_devices),
        )
        .route(
            "/v1/admin/accounts/{external_id}/devices/{device_id}",
            delete(api::admin_revoke_device),
        )
        // /v1 uses bearer tokens, no cookies: permissive CORS lets web
        // clients call it. The console's cookie is never sent cross-site
        // with credentials (CorsLayer::permissive does not allow them).
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn stream_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/accept", get(relay::accept_ws))
        .route("/v1/connect", get(relay::connect_ws))
}

pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> anyhow::Result<()> {
    let peer = match state.config.peer_addr {
        Some(addr) => {
            let bind_ip = match addr.ip() {
                std::net::IpAddr::V4(_) => std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                std::net::IpAddr::V6(_) => std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
            };
            Some(tokio::net::TcpListener::bind((bind_ip, addr.port())).await?)
        }
        None => None,
    };
    serve_with_peer(listener, peer, state).await
}

/// Run the public listener and, in cluster mode, the private peer listener.
pub async fn serve_with_peer(
    listener: tokio::net::TcpListener,
    peer: Option<tokio::net::TcpListener>,
    state: Arc<AppState>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        peer.is_some() == state.config.peer_addr.is_some(),
        "peer listener and address must be configured together"
    );
    if state.config.admin_key.is_some() || state.config.open_bootstrap {
        state.db.ensure_default_app().await?;
    }
    state.renew_lease().await?;
    // Streams carry interactive traffic (keystrokes, TLS handshakes): never
    // let Nagle hold a small write back waiting for a delayed ACK.
    let nodelay = |tcp: &mut tokio::net::TcpStream| {
        if let Err(err) = tcp.set_nodelay(true) {
            tracing::debug!(%err, "could not set TCP_NODELAY");
        }
    };
    let internal = async {
        if let Some(peer) = peer {
            axum::serve(peer.tap_io(nodelay), stream_routes().with_state(state.clone())).await
        } else {
            std::future::pending::<std::io::Result<()>>().await
        }
    };
    tokio::select! {
        result = axum::serve(listener.tap_io(nodelay), router(state.clone())) => result?,
        result = internal => result?,
        result = cluster::maintain(state.clone()) => result?,
    }
    Ok(())
}
