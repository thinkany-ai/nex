//! REST endpoints.

use crate::{
    db::{AppRow, DeviceRow, DEFAULT_APP_ID},
    cluster::OnlineHost,
    util::{hash_secret, normalize_code, now, random_secret},
    AppState, TicketEntry,
};
use axum::{
    extract::{FromRequestParts, Path, State},
    http::{header::AUTHORIZATION, request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use nex_proto::*;
use std::{collections::HashMap, sync::Arc};

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "invalid or revoked credentials")
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!("internal error: {err:#}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

/// The calling device, from `Authorization: Bearer <device_token>`.
pub struct AuthDevice(pub DeviceRow);

impl FromRequestParts<Arc<AppState>> for AuthDevice {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, ApiError> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(ApiError::unauthorized)?;
        let device = state
            .db
            .authenticate(token.trim())
            .await?
            .ok_or_else(ApiError::unauthorized)?;
        Ok(AuthDevice(device))
    }
}

fn clean_name(raw: &str, what: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(format!(
            "{what} must be 1-100 characters without control characters"
        )));
    }
    Ok(name.to_string())
}

fn clean_app(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(app) if valid_app_name(app) => Ok(Some(app.to_string())),
        Some(_) => Err(ApiError::bad_request(
            "app must be 1-64 characters of A-Z a-z 0-9 . _ -",
        )),
    }
}

/// The app key a product backend sent: `x-nex-admin-key` (what the SDKs
/// send) or `Authorization: Bearer nexk_…`.
fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(key) = headers.get(ADMIN_KEY_HEADER).and_then(|v| v.to_str().ok()) {
        return Some(key.trim());
    }
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| k.starts_with("nexk_"))
}

/// Resolves the presented key to its app. `NEX_ADMIN_KEY`, when set, is an
/// extra key of the built-in default app.
async fn app_for_key(state: &AppState, key: &str) -> Result<Option<AppRow>, ApiError> {
    if let Some(legacy) = &state.config.admin_key {
        // Compare digests so the comparison time does not depend on the key.
        if hash_secret(key) == hash_secret(legacy) {
            return Ok(state.db.get_app(DEFAULT_APP_ID).await?.filter(|a| !a.disabled));
        }
    }
    Ok(state.db.app_by_key(key).await?)
}

/// Guards `/v1/admin/*`: the caller's app, from its app key.
async fn require_app(state: &AppState, headers: &HeaderMap) -> Result<AppRow, ApiError> {
    let Some(key) = presented_key(headers) else {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "app key required"));
    };
    app_for_key(state, key)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::FORBIDDEN, "invalid or revoked app key"))
}

pub(crate) fn device_info(d: DeviceRow, hosts: &HashMap<String, OnlineHost>, me: Option<&str>) -> DeviceInfo {
    let host = hosts.get(&d.id);
    DeviceInfo {
        online: host.is_some(),
        services: host.map(|h| h.services.0.clone()).unwrap_or_default(),
        is_self: Some(d.id.as_str()) == me,
        id: d.id,
        name: d.name,
        app: d.app,
        created_at: d.created_at,
        last_seen_at: d.last_seen_at,
    }
}

pub async fn bootstrap(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BootstrapRequest>,
) -> Result<Json<DeviceCredential>, ApiError> {
    // The new account belongs to the app whose key was presented. Without a
    // key, only a relay started with --open-bootstrap (local development)
    // accepts it, into the default app.
    let app_id = match presented_key(&headers) {
        Some(_) => require_app(&state, &headers).await?.id,
        None if state.config.open_bootstrap => DEFAULT_APP_ID.to_string(),
        None => return Err(ApiError::new(StatusCode::FORBIDDEN, "app key required")),
    };
    let account_name = clean_name(&req.account_name, "account_name")?;
    let device_name = clean_name(&req.device_name, "device_name")?;
    let app = clean_app(req.app.as_deref())?;
    let (account_id, device_id, device_token) = state
        .db
        .bootstrap(&app_id, &account_name, &device_name, app.as_deref())
        .await?;
    tracing::info!(%app_id, %account_id, %device_id, "account bootstrapped");
    Ok(Json(DeviceCredential {
        account_id,
        device_id,
        device_token,
    }))
}

pub async fn create_pairing_code(
    State(state): State<Arc<AppState>>,
    AuthDevice(me): AuthDevice,
) -> Result<Json<PairingCode>, ApiError> {
    let ttl = state.config.pairing_ttl.as_secs() as i64;
    let (code, expires_at) = state.db.create_pairing_code(&me.account_id, &me.id, ttl).await?;
    Ok(Json(PairingCode { code, expires_at }))
}

pub async fn claim(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ClaimRequest>,
) -> Result<Json<DeviceCredential>, ApiError> {
    let device_name = clean_name(&req.device_name, "device_name")?;
    let app = clean_app(req.app.as_deref())?;
    let code = normalize_code(&req.code);
    let claimed = state
        .db
        .claim_pairing_code(&code, &device_name, app.as_deref())
        .await?;
    let Some((account_id, device_id, device_token)) = claimed else {
        return Err(ApiError::not_found(
            "invalid, used, or expired pairing code",
        ));
    };
    tracing::info!(%account_id, %device_id, "device paired");
    Ok(Json(DeviceCredential {
        account_id,
        device_id,
        device_token,
    }))
}

pub async fn list_devices(
    State(state): State<Arc<AppState>>,
    AuthDevice(me): AuthDevice,
) -> Result<Json<DeviceList>, ApiError> {
    let rows = state.db.list_devices(&me.account_id).await?;
    let hosts = state.online_hosts(&rows).await?;
    let devices = rows
        .into_iter()
        .map(|d| device_info(d, &hosts, Some(&me.id)))
        .collect();
    Ok(Json(DeviceList { devices }))
}

pub async fn whoami(AuthDevice(me): AuthDevice) -> Json<WhoAmI> {
    Json(WhoAmI {
        account_id: me.account_id,
        device_id: me.id,
        device_name: me.name,
        app: me.app,
    })
}

pub async fn rename_device(
    State(state): State<Arc<AppState>>,
    AuthDevice(me): AuthDevice,
    Path(device_id): Path<String>,
    Json(req): Json<RenameRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = clean_name(&req.name, "name")?;
    if !state.db.rename_device(&me.account_id, &device_id, &name).await? {
        return Err(ApiError::not_found("no such device"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------- admin API (product backends) ----------

fn clean_external_id(raw: &str) -> Result<String, ApiError> {
    let id = raw.trim();
    if id.is_empty() || id.len() > 200 || id.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "external_account_id must be 1-200 bytes without control characters",
        ));
    }
    Ok(id.to_string())
}

pub async fn admin_issue_device(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<IssueDeviceRequest>,
) -> Result<Json<DeviceCredential>, ApiError> {
    let app_row = require_app(&state, &headers).await?;
    let external_id = clean_external_id(&req.external_account_id)?;
    let account_name = clean_name(
        req.account_name.as_deref().unwrap_or(&external_id),
        "account_name",
    )?;
    let device_name = clean_name(&req.device_name, "device_name")?;
    let app = clean_app(req.app.as_deref())?;
    let (account_id, device_id, device_token) = state
        .db
        .issue_device(
            &app_row.id,
            &external_id,
            &account_name,
            &device_name,
            app.as_deref(),
        )
        .await?;
    tracing::info!(app_id = %app_row.id, %account_id, %device_id, "device issued by admin API");
    Ok(Json(DeviceCredential {
        account_id,
        device_id,
        device_token,
    }))
}

async fn external_account(state: &AppState, app_id: &str, external_id: &str) -> Result<String, ApiError> {
    state
        .db
        .account_by_external_id(app_id, external_id)
        .await?
        .ok_or_else(|| ApiError::not_found("no such account"))
}

pub async fn admin_list_devices(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(external_id): Path<String>,
) -> Result<Json<DeviceList>, ApiError> {
    let app = require_app(&state, &headers).await?;
    let account_id = external_account(&state, &app.id, &external_id).await?;
    let rows = state.db.list_devices(&account_id).await?;
    let hosts = state.online_hosts(&rows).await?;
    let devices = rows
        .into_iter()
        .map(|d| device_info(d, &hosts, None))
        .collect();
    Ok(Json(DeviceList { devices }))
}

pub async fn admin_revoke_device(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((external_id, device_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let app = require_app(&state, &headers).await?;
    let account_id = external_account(&state, &app.id, &external_id).await?;
    if !state.db.revoke_device(&account_id, &device_id).await? {
        return Err(ApiError::not_found("no such device"));
    }
    state.drop_device(&device_id);
    tracing::info!(%device_id, "device revoked by admin API");
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn revoke_device(
    State(state): State<Arc<AppState>>,
    AuthDevice(me): AuthDevice,
    Path(device_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.db.revoke_device(&me.account_id, &device_id).await? {
        return Err(ApiError::not_found("no such device"));
    }
    state.drop_device(&device_id);
    tracing::info!(%device_id, by = %me.id, "device revoked");
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn create_ticket(
    State(state): State<Arc<AppState>>,
    AuthDevice(me): AuthDevice,
    Json(req): Json<TicketRequest>,
) -> Result<Json<Ticket>, ApiError> {
    let devices = state.db.list_devices(&me.account_id).await?;
    let target = match devices.iter().find(|d| d.id == req.device) {
        Some(d) => d,
        None => {
            let by_name: Vec<_> = devices.iter().filter(|d| d.name == req.device).collect();
            match by_name.as_slice() {
                [one] => *one,
                [] => return Err(ApiError::not_found(format!("no device '{}'", req.device))),
                _ => {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        format!(
                            "several devices are named '{}'; use the device id",
                            req.device
                        ),
                    ))
                }
            }
        }
    };

    let service = {
        let hosts = state.online_hosts(&devices).await?;
        let host = hosts
            .get(&target.id)
            .ok_or_else(|| ApiError::not_found(format!("device '{}' is offline", target.name)))?;
        host.services.0
            .iter()
            .find(|s| s.name == req.service)
            .cloned()
            .ok_or_else(|| {
                ApiError::not_found(format!(
                    "device '{}' does not expose service '{}'",
                    target.name, req.service
                ))
            })?
    };

    let ticket = random_secret("next");
    let expires_at = now() + state.config.ticket_ttl.as_secs() as i64;
    state.insert_ticket(
        &hash_secret(&ticket),
        &TicketEntry {
            from_device_id: me.id,
            target_device_id: target.id.clone(),
            service: service.name.clone(),
            expires_at,
        },
    ).await?;
    Ok(Json(Ticket {
        path: format!("/v1/connect?ticket={ticket}"),
        ticket,
        expires_at,
        device_id: target.id.clone(),
        service,
    }))
}
