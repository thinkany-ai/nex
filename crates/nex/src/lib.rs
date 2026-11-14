//! nex client SDK.
//!
//! - [`NexClient`]: REST calls (devices, pairing, revoke, tickets) and
//!   opening streams to services on other devices.
//! - [`Host`]: keeps this device online and serves streams for the local
//!   services it exposes. Embed it in a product daemon (dscode, termany,
//!   agl) or run it with `nex up`.
//! - [`NexAdmin`]: the admin API, for product backends that register
//!   devices for their own users (server side only: it holds the admin key).
//! - [`forward_tcp`]: listens on a local port and tunnels each connection to
//!   a TCP service on another device.
//!
//! Web, iOS, and Android clients do not need this crate: they use the REST
//! API plus a plain WebSocket. See `docs/INTEGRATION.md`.

pub mod client;
pub mod config;
pub mod forward;
pub mod host;
pub mod pipe;

pub use client::{ApiError, NexAdmin, NexClient, WsStream};
pub use forward::forward_tcp;
pub use host::{ExposedService, Host, Target};
pub use nex_proto as proto;
