//! Web console at `/admin`: console users sign in, create apps (one per
//! agent product that needs networking), manage each app's keys, and see
//! and revoke the app's devices. Administrators also manage users and
//! whether sign-up is open.
//!
//! The page is one static HTML file; it talks to the JSON API under
//! `/admin/api`. Sessions are an HttpOnly cookie; every state-changing call
//! must carry `x-csrf-token` (derived from the session) and come from the
//! console's own origin.

use crate::{
    api::{device_info, ApiError},
    db::{AppRow, KeyError, UserRow, DEFAULT_APP_ID, ROLE_ADMIN, ROLE_MEMBER},
    util::hash_secret,
    AppState,
};
use axum::{
    extract::{FromRequestParts, Path, State},
    http::{
        header::{CACHE_CONTROL, COOKIE, HOST, ORIGIN, SET_COOKIE},
        request::Parts,
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

const COOKIE_NAME: &str = "nex_admin";
const SESSION_TTL: i64 = 8 * 3600;
const LIMIT_WINDOW: i64 = 15 * 60;
const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 256;
const SIGNUP_SETTING: &str = "signup_open";
/// Accounts (with their devices) shown on an app's page.
const ACCOUNTS_SHOWN: i64 = 200;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin", get(page))
        .route("/admin/", get(page))
        .route("/admin/api/session", get(session_info))
        .route("/admin/api/login", post(login))
        .route("/admin/api/signup", post(signup))
        .route("/admin/api/logout", post(logout))
        .route("/admin/api/password", post(change_password))
        .route("/admin/api/apps", get(list_apps).post(create_app))
        .route(
            "/admin/api/apps/{id}",
            get(get_app).patch(update_app).delete(delete_app),
        )
        .route("/admin/api/apps/{id}/keys", post(create_key))
        .route("/admin/api/apps/{id}/keys/{key_id}", delete(revoke_key))
        .route("/admin/api/apps/{id}/devices/{device_id}", delete(revoke_device))
        .route("/admin/api/users", get(list_users).post(create_user))
        .route("/admin/api/users/{id}", axum::routing::patch(update_user).delete(delete_user))
        .route("/admin/api/settings", put(update_settings))
}

async fn page() -> Response {
    let mut res = Html(include_str!("admin.html")).into_response();
    let h = res.headers_mut();
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             img-src 'self' data:; connect-src 'self'; form-action 'none'; base-uri 'none'; \
             frame-ancestors 'none'",
        ),
    );
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

// ---------- passwords ----------

fn derive(password: &str, salt_hex: &str) -> [u8; 64] {
    // N = 32768, r = 8, p = 1, 64 bytes: the same as imc and connany, so
    // hashes can move between them.
    let params = scrypt::Params::new(15, 8, 1, 64).expect("valid scrypt params");
    let mut out = [0u8; 64];
    scrypt::scrypt(password.as_bytes(), salt_hex.as_bytes(), &params, &mut out)
        .expect("64-byte output");
    out
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Returns the stored form of `password` ("scrypt:<salt hex>:<key hex>").
pub async fn hash_password(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        let salt_hex = to_hex(&salt);
        let key = derive(&password, &salt_hex);
        format!("scrypt:{salt_hex}:{}", to_hex(&key))
    })
    .await
    .map_err(Into::into)
}

/// A sign-in for an unknown email checks against this, so it takes as long
/// as a real one.
const DUMMY_HASH: &str = "scrypt:00000000000000000000000000000000:00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

async fn verify_password(password: String, stored: String) -> bool {
    tokio::task::spawn_blocking(move || {
        let parts: Vec<&str> = stored.split(':').collect();
        let [scheme, salt, want] = parts.as_slice() else {
            return false;
        };
        if *scheme != "scrypt" || salt.len() != 32 || want.len() != 128 {
            return false;
        }
        let got = to_hex(&derive(&password, salt));
        // Constant time over the hex digests.
        got.bytes()
            .zip(want.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    })
    .await
    .unwrap_or(false)
}

pub fn valid_password(p: &str) -> Result<(), ApiError> {
    if p.len() < MIN_PASSWORD || p.len() > MAX_PASSWORD {
        return Err(ApiError::bad_request("密码需为 8–256 个字符。"));
    }
    Ok(())
}

pub fn normalize_email(raw: &str) -> Result<String, ApiError> {
    let email = raw.trim().to_lowercase();
    let ok = email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && match email.split_once('@') {
            Some((local, domain)) => {
                !local.is_empty()
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
                    && !domain.contains('@')
            }
            None => false,
        };
    if !ok {
        return Err(ApiError::bad_request("邮箱格式不正确。"));
    }
    Ok(email)
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Derived from the session, so it needs no storage.
fn csrf_token(session_token: &str) -> String {
    hash_secret(&format!("admin-csrf:{session_token}"))
}

// ---------- sessions ----------

pub struct Session {
    pub user: UserRow,
    token: String,
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE_NAME)
        .map(|(_, v)| v.to_string())
        .filter(|v| v.len() == 43)
}

/// State-changing calls must come from the console's own origin: the
/// configured public URL, or (unset, local development) the request's host.
fn same_origin(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else {
        // Non-browser clients send no Origin; the CSRF token still applies.
        return true;
    };
    match &state.config.public_url {
        Some(public) => origin.trim_end_matches('/') == public.trim_end_matches('/'),
        None => {
            let host = headers.get(HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
            origin
                .split_once("://")
                .is_some_and(|(_, rest)| rest.trim_end_matches('/') == host)
        }
    }
}

impl FromRequestParts<Arc<AppState>> for Session {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, ApiError> {
        let token = cookie_token(&parts.headers)
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "请先登录。"))?;
        let user = state
            .db
            .session_user(&hash_secret(&token))
            .await?
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "请先登录。"))?;
        if parts.method != Method::GET {
            let given = parts.headers.get("x-csrf-token").and_then(|v| v.to_str().ok());
            if given != Some(csrf_token(&token).as_str()) || !same_origin(state, &parts.headers) {
                return Err(ApiError::new(StatusCode::FORBIDDEN, "页面已失效，请刷新后重试。"));
            }
        }
        Ok(Session { user, token })
    }
}

/// A session of an administrator.
pub struct AdminSession(Session);

impl FromRequestParts<Arc<AppState>> for AdminSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, ApiError> {
        let s = Session::from_request_parts(parts, state).await?;
        if !s.user.is_admin() {
            return Err(ApiError::new(StatusCode::FORBIDDEN, "需要管理员权限。"));
        }
        Ok(AdminSession(s))
    }
}

fn session_cookie(state: &AppState, token: &str, max_age: i64) -> HeaderValue {
    let secure = state
        .config
        .public_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
    let value = format!(
        "{COOKIE_NAME}={token}; Path=/admin; HttpOnly; SameSite=Lax; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&value).expect("cookie is ASCII")
}

fn user_json(u: &UserRow) -> Value {
    json!({ "id": u.id, "email": u.email, "role": u.role, "created_at": u.created_at })
}

async fn signup_open(state: &AppState) -> Result<bool, ApiError> {
    Ok(state.db.setting(SIGNUP_SETTING).await?.as_deref() == Some("true"))
}

async fn session_info(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let signup = signup_open(&state).await?;
    let mut out = json!({ "signup_open": signup, "user": null });
    if let Some(token) = cookie_token(&headers) {
        if let Some(user) = state.db.session_user(&hash_secret(&token)).await? {
            out["user"] = user_json(&user);
            out["csrf"] = json!(csrf_token(&token));
        }
    }
    Ok(Json(out))
}

/// Counts an attempt; fails with 429 when `bucket` is over `max` in 15 minutes.
async fn limit(state: &AppState, bucket: &str, max: i64) -> Result<(), ApiError> {
    if state.db.attempt(bucket, LIMIT_WINDOW).await? > max {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "尝试次数过多，请 15 分钟后再试。"));
    }
    Ok(())
}

async fn start_session(state: &AppState, user: &UserRow) -> Result<Response, ApiError> {
    let token = new_token();
    state
        .db
        .create_session(&hash_secret(&token), &user.id, SESSION_TTL)
        .await?;
    let mut res = Json(json!({ "user": user_json(user), "csrf": csrf_token(&token) })).into_response();
    res.headers_mut()
        .insert(SET_COOKIE, session_cookie(state, &token, SESSION_TTL));
    Ok(res)
}

#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
}

async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<Credentials>,
) -> Result<Response, ApiError> {
    if !same_origin(&state, &headers) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "请求来源不正确。"));
    }
    let invalid = || ApiError::new(StatusCode::UNAUTHORIZED, "邮箱或密码不正确。");
    let email = normalize_email(&req.email).map_err(|_| invalid())?;
    if req.password.is_empty() || req.password.len() > MAX_PASSWORD {
        return Err(invalid());
    }
    let bucket = format!("login:{}", hash_secret(&email));
    limit(&state, "login:global", 100).await?;
    limit(&state, &bucket, 10).await?;
    let user = state.db.user_by_email(&email).await?;
    let stored = user
        .as_ref()
        .map(|u| u.password_hash.clone())
        .unwrap_or_else(|| DUMMY_HASH.to_string());
    if !verify_password(req.password, stored).await {
        return Err(invalid());
    }
    let Some(user) = user else { return Err(invalid()) };
    state.db.clear_attempts(&bucket).await?;
    start_session(&state, &user).await
}

async fn signup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<Credentials>,
) -> Result<Response, ApiError> {
    if !same_origin(&state, &headers) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "请求来源不正确。"));
    }
    if !signup_open(&state).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "当前未开放注册，请联系管理员创建账号。"));
    }
    let email = normalize_email(&req.email)?;
    valid_password(&req.password)?;
    limit(&state, "signup", 30).await?;
    let hash = hash_password(req.password).await?;
    let user = state
        .db
        .create_user(&email, &hash, ROLE_MEMBER)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "该邮箱已注册。"))?;
    tracing::info!(user_id = %user.id, "console user signed up");
    start_session(&state, &user).await
}

async fn logout(State(state): State<Arc<AppState>>, s: Session) -> Result<Response, ApiError> {
    state.db.delete_session(&hash_secret(&s.token)).await?;
    let mut res = Json(json!({ "ok": true })).into_response();
    res.headers_mut().insert(SET_COOKIE, session_cookie(&state, "", 0));
    Ok(res)
}

#[derive(Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
}

async fn change_password(
    State(state): State<Arc<AppState>>,
    s: Session,
    Json(req): Json<PasswordChange>,
) -> Result<Json<Value>, ApiError> {
    limit(&state, &format!("password:{}", s.user.id), 10).await?;
    if !verify_password(req.current, s.user.password_hash.clone()).await {
        return Err(ApiError::bad_request("当前密码不正确。"));
    }
    valid_password(&req.new)?;
    let hash = hash_password(req.new).await?;
    state.db.set_password(&s.user.id, &hash).await?;
    // Other browsers signed in as this user are signed out.
    state
        .db
        .delete_other_sessions(&s.user.id, &hash_secret(&s.token))
        .await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- apps ----------

fn clean_app_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request("应用名称需为 1–64 个字符。"));
    }
    Ok(name.to_string())
}

/// The app if the session may manage it: its owner, or any administrator.
async fn load_app(state: &AppState, s: &Session, id: &str) -> Result<AppRow, ApiError> {
    let app = state.db.get_app(id).await?;
    match app {
        Some(app) if s.user.is_admin() || app.owner_id.as_deref() == Some(&s.user.id) => Ok(app),
        _ => Err(ApiError::not_found("应用不存在。")),
    }
}

fn app_json(app: &AppRow) -> Value {
    json!({
        "id": app.id,
        "name": app.name,
        "owner_id": app.owner_id,
        "disabled": app.disabled,
        "builtin": app.id == DEFAULT_APP_ID,
        "created_at": app.created_at,
    })
}

async fn list_apps(State(state): State<Arc<AppState>>, s: Session) -> Result<Json<Value>, ApiError> {
    let owner = (!s.user.is_admin()).then_some(s.user.id.as_str());
    let apps = state.db.list_apps(owner).await?;
    let apps: Vec<Value> = apps
        .into_iter()
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "owner_id": a.owner_id,
                "owner_email": a.owner_email,
                "disabled": a.disabled,
                "builtin": a.id == DEFAULT_APP_ID,
                "created_at": a.created_at,
                "accounts": a.accounts,
                "devices": a.devices,
            })
        })
        .collect();
    Ok(Json(json!({ "apps": apps })))
}

#[derive(Deserialize)]
struct NewApp {
    name: String,
}

async fn create_app(
    State(state): State<Arc<AppState>>,
    s: Session,
    Json(req): Json<NewApp>,
) -> Result<Json<Value>, ApiError> {
    let name = clean_app_name(&req.name)?;
    let app = state.db.create_app(&s.user.id, &name).await?;
    let (key_row, key) = match state.db.create_key(&app.id, "default").await? {
        Ok(k) => k,
        Err(_) => return Err(anyhow::anyhow!("new app refused its first key").into()),
    };
    tracing::info!(app_id = %app.id, owner = %s.user.id, "app created");
    Ok(Json(json!({ "app": app_json(&app), "key": key, "key_id": key_row.id })))
}

async fn get_app(
    State(state): State<Arc<AppState>>,
    s: Session,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let app = load_app(&state, &s, &id).await?;
    let keys = state.db.list_keys(&app.id).await?;
    let accounts = state.db.list_accounts(&app.id, ACCOUNTS_SHOWN).await?;
    let ids: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
    let devices = state.db.devices_of_accounts(&app.id, &ids).await?;
    let mut by_account: HashMap<String, Vec<Value>> = HashMap::new();
    {
        let hosts = state.online_hosts(&devices).await?;
        for d in devices {
            let account_id = d.account_id.clone();
            let info = device_info(d, &hosts, None);
            by_account
                .entry(account_id)
                .or_default()
                .push(serde_json::to_value(info).unwrap_or_default());
        }
    }
    let accounts: Vec<Value> = accounts
        .into_iter()
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "external_id": a.external_id,
                "created_at": a.created_at,
                "devices": by_account.remove(&a.id).unwrap_or_default(),
            })
        })
        .collect();
    let keys: Vec<Value> = keys
        .into_iter()
        .map(|k| {
            json!({
                "id": k.id,
                "name": k.name,
                "prefix": k.key_prefix,
                "created_at": k.created_at,
                "last_used_at": k.last_used_at,
                "revoked_at": k.revoked_at,
            })
        })
        .collect();
    let owner_email = match &app.owner_id {
        Some(owner) => state.db.user_by_id(owner).await?.map(|u| u.email),
        None => None,
    };
    let mut app_value = app_json(&app);
    app_value["owner_email"] = json!(owner_email);
    Ok(Json(json!({
        "app": app_value,
        "keys": keys,
        "accounts": accounts,
        "accounts_limit": ACCOUNTS_SHOWN,
        "legacy_key": app.id == DEFAULT_APP_ID && state.config.admin_key.is_some(),
    })))
}

#[derive(Deserialize)]
struct AppPatch {
    name: Option<String>,
    disabled: Option<bool>,
}

async fn update_app(
    State(state): State<Arc<AppState>>,
    s: Session,
    Path(id): Path<String>,
    Json(req): Json<AppPatch>,
) -> Result<Json<Value>, ApiError> {
    let app = load_app(&state, &s, &id).await?;
    let name = req.name.as_deref().map(clean_app_name).transpose()?;
    state
        .db
        .update_app(&app.id, name.as_deref(), req.disabled)
        .await?;
    if req.disabled == Some(true) && !app.disabled {
        // A disabled app's devices are cut off at once.
        for device_id in state.db.app_device_ids(&app.id).await? {
            state.drop_device(&device_id);
        }
        tracing::info!(app_id = %app.id, "app disabled");
    }
    Ok(Json(json!({ "ok": true })))
}

async fn delete_app(
    State(state): State<Arc<AppState>>,
    s: Session,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let app = load_app(&state, &s, &id).await?;
    if app.id == DEFAULT_APP_ID {
        return Err(ApiError::bad_request("内置应用不能删除，可以停用。"));
    }
    for device_id in state.db.delete_app(&app.id).await? {
        state.drop_device(&device_id);
    }
    tracing::info!(app_id = %app.id, by = %s.user.id, "app deleted");
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct NewKey {
    #[serde(default)]
    name: String,
}

async fn create_key(
    State(state): State<Arc<AppState>>,
    s: Session,
    Path(id): Path<String>,
    Json(req): Json<NewKey>,
) -> Result<Json<Value>, ApiError> {
    let app = load_app(&state, &s, &id).await?;
    let name = req.name.trim();
    if name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request("密钥名称最多 64 个字符。"));
    }
    match state.db.create_key(&app.id, name).await? {
        Ok((row, key)) => Ok(Json(json!({ "key": key, "key_id": row.id }))),
        Err(KeyError::TooMany) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "每个应用最多 2 个有效密钥，请先吊销一个旧密钥。",
        )),
        Err(KeyError::NoSuchApp) => Err(ApiError::not_found("应用不存在。")),
    }
}

async fn revoke_key(
    State(state): State<Arc<AppState>>,
    s: Session,
    Path((id, key_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let app = load_app(&state, &s, &id).await?;
    if !state.db.revoke_key(&app.id, &key_id).await? {
        return Err(ApiError::not_found("密钥不存在或已吊销。"));
    }
    Ok(Json(json!({ "ok": true })))
}

async fn revoke_device(
    State(state): State<Arc<AppState>>,
    s: Session,
    Path((id, device_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let app = load_app(&state, &s, &id).await?;
    if !state.db.revoke_app_device(&app.id, &device_id).await? {
        return Err(ApiError::not_found("设备不存在或已吊销。"));
    }
    state.drop_device(&device_id);
    tracing::info!(%device_id, by = %s.user.id, "device revoked from the console");
    Ok(Json(json!({ "ok": true })))
}

// ---------- users and settings (administrators) ----------

async fn list_users(State(state): State<Arc<AppState>>, _a: AdminSession) -> Result<Json<Value>, ApiError> {
    let users: Vec<Value> = state
        .db
        .list_users()
        .await?
        .into_iter()
        .map(|u| json!({ "id": u.id, "email": u.email, "role": u.role, "created_at": u.created_at, "apps": u.apps }))
        .collect();
    Ok(Json(json!({ "users": users, "signup_open": signup_open(&state).await? })))
}

#[derive(Deserialize)]
struct NewUser {
    email: String,
    password: String,
    #[serde(default)]
    role: Option<String>,
}

fn clean_role(role: Option<&str>) -> Result<&'static str, ApiError> {
    match role.unwrap_or(ROLE_MEMBER) {
        ROLE_ADMIN => Ok(ROLE_ADMIN),
        ROLE_MEMBER => Ok(ROLE_MEMBER),
        _ => Err(ApiError::bad_request("角色只能是 admin 或 member。")),
    }
}

async fn create_user(
    State(state): State<Arc<AppState>>,
    _a: AdminSession,
    Json(req): Json<NewUser>,
) -> Result<Json<Value>, ApiError> {
    let email = normalize_email(&req.email)?;
    valid_password(&req.password)?;
    let role = clean_role(req.role.as_deref())?;
    let hash = hash_password(req.password).await?;
    let user = state
        .db
        .create_user(&email, &hash, role)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "该邮箱已注册。"))?;
    Ok(Json(json!({ "user": user_json(&user) })))
}

#[derive(Deserialize)]
struct UserPatch {
    role: String,
}

async fn update_user(
    State(state): State<Arc<AppState>>,
    _a: AdminSession,
    Path(id): Path<String>,
    Json(req): Json<UserPatch>,
) -> Result<Json<Value>, ApiError> {
    let role = clean_role(Some(&req.role))?;
    if !state.db.set_role(&id, role).await? {
        return Err(ApiError::bad_request("用户不存在，或这是最后一个管理员。"));
    }
    Ok(Json(json!({ "ok": true })))
}

async fn delete_user(
    State(state): State<Arc<AppState>>,
    AdminSession(s): AdminSession,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if id == s.user.id {
        return Err(ApiError::bad_request("不能删除自己。"));
    }
    let Some(devices) = state.db.delete_user(&id).await? else {
        return Err(ApiError::bad_request("用户不存在，或这是最后一个管理员。"));
    };
    for device_id in devices {
        state.drop_device(&device_id);
    }
    tracing::info!(user_id = %id, by = %s.user.id, "console user deleted");
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct Settings {
    signup_open: bool,
}

async fn update_settings(
    State(state): State<Arc<AppState>>,
    _a: AdminSession,
    Json(req): Json<Settings>,
) -> Result<Json<Value>, ApiError> {
    state
        .db
        .set_setting(SIGNUP_SETTING, if req.signup_open { "true" } else { "false" })
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn password_roundtrip() {
        let stored = hash_password("correct horse".into()).await.unwrap();
        assert!(stored.starts_with("scrypt:"));
        assert!(verify_password("correct horse".into(), stored.clone()).await);
        assert!(!verify_password("wrong horse".into(), stored).await);
        assert!(!verify_password("x".into(), DUMMY_HASH.into()).await);
    }

    #[test]
    fn emails() {
        assert_eq!(normalize_email(" A@B.co ").unwrap(), "a@b.co");
        for bad in ["", "a", "a@b", "@b.co", "a b@c.co", "a@.co"] {
            assert!(normalize_email(bad).is_err(), "{bad}");
        }
    }
}
