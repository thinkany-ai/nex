//! Wire types shared by the nex relay (`nex-server`) and its clients.
//!
//! Everything is JSON. REST bodies are plain structs; the host control
//! channel (`GET /v1/host`, WebSocket) exchanges [`HostMessage`] and
//! [`ServerMessage`] as tagged JSON text frames.

use serde::{Deserialize, Serialize};

/// Bumped when the control-channel protocol changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Header carrying the relay's admin key. Required by the `/v1/admin/*`
/// endpoints (product backends), and by `POST /v1/bootstrap` when the relay
/// is started with an admin key.
pub const ADMIN_KEY_HEADER: &str = "x-nex-admin-key";

/// How a stream to a service is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    /// Raw bytes. Each WebSocket binary frame is a chunk of the TCP stream.
    Tcp,
    /// Message passthrough. The host proxies each frame to a local
    /// WebSocket server, so a browser can speak the service's own
    /// protocol directly over the relay.
    Ws,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    pub kind: ServiceKind,
}

/// Service names: 1-64 chars of `[A-Za-z0-9._-]`.
pub fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// App labels (which product registered a device, e.g. `termany`) follow
/// the same rules as service names.
pub fn valid_app_name(name: &str) -> bool {
    valid_service_name(name)
}

// ---------- REST ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapRequest {
    pub account_name: String,
    pub device_name: String,
    /// Product that registers the device, e.g. `termany` or `dscode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

/// `POST /v1/admin/devices`: a product backend registers a device for one of
/// its users. The account is created on first use and found by
/// `external_account_id` afterwards, so this is how a product maps its own
/// user ids onto nex accounts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueDeviceRequest {
    /// The product's (or the shared identity service's) user id.
    pub external_account_id: String,
    /// Display name used when the account is created. Defaults to the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_name: Option<String>,
    pub device_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

/// Returned once when a device is registered. `device_token` is a bearer
/// secret: store it like a password (Keychain / Keystore / 0600 file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCredential {
    pub account_id: String,
    pub device_id: String,
    pub device_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCode {
    /// Formatted as `XXXX-XXXX`. Dashes and case are ignored when claiming.
    pub code: String,
    /// Unix seconds.
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub code: String,
    pub device_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

/// `PATCH /v1/devices/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

/// `GET /v1/me`: who a device token belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmI {
    pub account_id: String,
    pub device_id: String,
    pub device_name: String,
    pub app: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// Product that registered the device, if it said.
    #[serde(default)]
    pub app: Option<String>,
    /// True while the device holds a host control connection.
    pub online: bool,
    /// Services the device currently exposes (empty when offline).
    pub services: Vec<ServiceInfo>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    /// True for the device making the request.
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceList {
    pub devices: Vec<DeviceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketRequest {
    /// Device id, or exact device name within the account.
    pub device: String,
    pub service: String,
}

/// A single-use, short-lived permission to open one stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub ticket: String,
    pub expires_at: i64,
    pub device_id: String,
    pub service: ServiceInfo,
    /// Path + query to open as a WebSocket on the relay, e.g.
    /// `/v1/connect?ticket=...`. Resolve it against the relay base URL with
    /// `ws://` / `wss://`.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

// ---------- Host control channel ----------

/// Host -> relay, JSON text frames on `GET /v1/host`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// First frame of a session; may be re-sent later to update services.
    Hello {
        protocol: u32,
        services: Vec<ServiceInfo>,
    },
    /// Decline a stream the relay offered (e.g. the local service is down).
    Reject { stream_id: String, reason: String },
}

/// Relay -> host, JSON text frames on `GET /v1/host`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The device is online and reachable.
    Ready {
        device_id: String,
    },
    /// Another device wants a stream. Accept it by opening
    /// `GET /v1/accept?stream_id=...` (WebSocket) within the open timeout.
    Open {
        stream_id: String,
        service: String,
        from_device_id: String,
    },
    Error {
        message: String,
    },
}

/// WebSocket close codes used by the relay (application range 4000-4999).
pub mod close_code {
    pub const BAD_REQUEST: u16 = 4400;
    pub const REVOKED: u16 = 4401;
    pub const OFFLINE: u16 = 4404;
    /// Another host connection for the same device took over (the device
    /// credential is in use by a second process). Do not reconnect.
    pub const REPLACED: u16 = 4409;
    /// Too many concurrent streams for one of the two devices.
    pub const BUSY: u16 = 4429;
    pub const REJECTED: u16 = 4502;
    pub const TIMEOUT: u16 = 4504;
}
