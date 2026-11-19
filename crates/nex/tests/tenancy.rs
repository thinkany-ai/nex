//! Multi-tenancy: console users sign in, create apps and keys; each app's
//! accounts and devices are invisible to every other app; disabling an app
//! cuts its devices off.

use nex::{NexAdmin, NexClient};
use reqwest::header::{HeaderMap, HeaderValue, COOKIE, SET_COOKIE};
use serde_json::{json, Value};
use tokio::net::TcpListener;

async fn start() -> (String, nex_server::db::Db) {
    let db = nex_server::db::Db::connect_for_test().await.unwrap();
    let state = nex_server::AppState::new(db.clone(), nex_server::Config::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nex_server::serve(listener, state));
    (format!("http://{addr}"), db)
}

fn status_of(err: anyhow::Error) -> u16 {
    err.downcast::<nex::ApiError>().expect("an ApiError").status
}

/// A signed-in console browser: the session cookie and CSRF token.
struct Console {
    base: String,
    http: reqwest::Client,
    cookie: String,
    csrf: String,
}

impl Console {
    async fn login(base: &str, email: &str, password: &str) -> Result<Self, u16> {
        let http = reqwest::Client::new();
        let res = http
            .post(format!("{base}/admin/api/login"))
            .json(&json!({ "email": email, "password": password }))
            .send()
            .await
            .unwrap();
        if !res.status().is_success() {
            return Err(res.status().as_u16());
        }
        let cookie = res.headers()[SET_COOKIE].to_str().unwrap().to_string();
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        let cookie = cookie.split(';').next().unwrap().to_string();
        let body: Value = res.json().await.unwrap();
        Ok(Self {
            base: base.to_string(),
            http,
            cookie,
            csrf: body["csrf"].as_str().unwrap().to_string(),
        })
    }

    fn headers(&self, csrf: bool) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_str(&self.cookie).unwrap());
        if csrf {
            h.insert("x-csrf-token", HeaderValue::from_str(&self.csrf).unwrap());
        }
        h
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let is_get = method == reqwest::Method::GET;
        let mut req = self
            .http
            .request(method, format!("{}/admin/api{path}", self.base))
            .headers(self.headers(!is_get));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(Value::Null))
    }

    async fn ok(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let (status, v) = self.call(method, path, body).await;
        assert_eq!(status, 200, "{path}: {v}");
        v
    }
}

async fn make_user(db: &nex_server::db::Db, email: &str, role: &str) {
    let hash = nex_server::console::hash_password("password123".into()).await.unwrap();
    db.create_user(email, &hash, role).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apps_are_isolated_tenants() {
    use reqwest::Method;
    let (relay, db) = start().await;
    make_user(&db, "admin@example.com", "admin").await;
    make_user(&db, "alice@example.com", "member").await;
    make_user(&db, "bob@example.com", "member").await;

    assert_eq!(Console::login(&relay, "alice@example.com", "wrong-pass").await.err(), Some(401));
    let alice = Console::login(&relay, "Alice@Example.com", "password123").await.unwrap();
    let bob = Console::login(&relay, "bob@example.com", "password123").await.unwrap();
    let admin = Console::login(&relay, "admin@example.com", "password123").await.unwrap();

    // Without the CSRF header a state-changing call is refused.
    let res = alice
        .http
        .post(format!("{relay}/admin/api/apps"))
        .headers(alice.headers(false))
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 403);

    let a = alice.ok(Method::POST, "/apps", Some(json!({ "name": "dscode" }))).await;
    let b = bob.ok(Method::POST, "/apps", Some(json!({ "name": "termany" }))).await;
    let (a_id, a_key) = (a["app"]["id"].as_str().unwrap(), a["key"].as_str().unwrap());
    let (b_id, b_key) = (b["app"]["id"].as_str().unwrap(), b["key"].as_str().unwrap());
    assert!(a_key.starts_with("nexk_"));

    // Members see only their apps; admins see all; nobody else's app opens.
    let mine = alice.ok(Method::GET, "/apps", None).await;
    assert_eq!(mine["apps"].as_array().unwrap().len(), 1);
    assert_eq!(bob.call(Method::GET, &format!("/apps/{a_id}"), None).await.0, 404);
    let all = admin.ok(Method::GET, "/apps", None).await;
    assert_eq!(all["apps"].as_array().unwrap().len(), 2);
    assert_eq!(bob.call(Method::GET, "/users", None).await.0, 403);

    // Same external user id in two apps: two unrelated accounts.
    let admin_a = NexAdmin::new(&relay, a_key).unwrap();
    let admin_b = NexAdmin::new(&relay, b_key).unwrap();
    let issue = |device: &str| nex::proto::IssueDeviceRequest {
        external_account_id: "user/42".into(),
        account_name: None,
        device_name: device.into(),
        app: None,
    };
    let da = admin_a.issue_device(&issue("mac")).await.unwrap();
    let db_ = admin_b.issue_device(&issue("phone")).await.unwrap();
    assert_ne!(da.account_id, db_.account_id);
    assert_eq!(admin_a.devices("user/42").await.unwrap().len(), 1);
    // B's key can't revoke A's device, even knowing its id.
    assert_eq!(status_of(admin_b.revoke("user/42", &da.device_id).await.unwrap_err()), 404);
    let ca = NexClient::new(&relay, da.clone()).unwrap();
    let names: Vec<String> = ca.devices().await.unwrap().into_iter().map(|d| d.name).collect();
    assert_eq!(names, ["mac"]);

    // Bootstrap with an app key lands in that app; without a key it is refused.
    let boot = NexClient::bootstrap(&relay, "team", "laptop", None, Some(a_key)).await.unwrap();
    assert_eq!(status_of(NexClient::bootstrap(&relay, "t", "l", None, None).await.unwrap_err()), 403);
    let detail = alice.ok(Method::GET, &format!("/apps/{a_id}"), None).await;
    let accounts: Vec<&str> = detail["accounts"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert!(accounts.contains(&boot.account_id.as_str()) && accounts.contains(&da.account_id.as_str()));

    // Keys: at most two active; a revoked key stops working at once.
    let k2 = alice.ok(Method::POST, &format!("/apps/{a_id}/keys"), Some(json!({ "name": "next" }))).await;
    assert_eq!(alice.call(Method::POST, &format!("/apps/{a_id}/keys"), Some(json!({}))).await.0, 409);
    let first_key_id = a["key_id"].as_str().unwrap();
    alice.ok(Method::DELETE, &format!("/apps/{a_id}/keys/{first_key_id}"), None).await;
    assert_eq!(status_of(admin_a.devices("user/42").await.unwrap_err()), 403);
    let admin_a2 = NexAdmin::new(&relay, k2["key"].as_str().unwrap()).unwrap();
    assert_eq!(admin_a2.devices("user/42").await.unwrap().len(), 1);

    // Console revoke of a device.
    alice.ok(Method::DELETE, &format!("/apps/{a_id}/devices/{}", boot.device_id), None).await;
    let cb = NexClient::new(&relay, boot).unwrap();
    assert_eq!(status_of(cb.devices().await.unwrap_err()), 401);

    // Disabling an app rejects its keys and device credentials; enabling restores them.
    alice.ok(Method::PATCH, &format!("/apps/{a_id}"), Some(json!({ "disabled": true }))).await;
    assert_eq!(status_of(ca.devices().await.unwrap_err()), 401);
    assert_eq!(status_of(admin_a2.devices("user/42").await.unwrap_err()), 403);
    alice.ok(Method::PATCH, &format!("/apps/{a_id}"), Some(json!({ "disabled": false }))).await;
    assert_eq!(ca.devices().await.unwrap().len(), 1);

    // Deleting an app removes everything in it; the other app is untouched.
    alice.ok(Method::DELETE, &format!("/apps/{a_id}"), None).await;
    assert_eq!(status_of(ca.devices().await.unwrap_err()), 401);
    assert_eq!(admin_b.devices("user/42").await.unwrap().len(), 1);
    let _ = b_id;

    // Users: the last administrator can't be demoted; deleting a user deletes their apps.
    let users = admin.ok(Method::GET, "/users", None).await;
    let id_of = |email: &str| {
        users["users"].as_array().unwrap().iter().find(|u| u["email"] == email).unwrap()["id"].as_str().unwrap().to_string()
    };
    let admin_id = id_of("admin@example.com");
    assert_eq!(admin.call(Method::PATCH, &format!("/users/{admin_id}"), Some(json!({ "role": "member" }))).await.0, 400);
    admin.ok(Method::DELETE, &format!("/users/{}", id_of("bob@example.com")), None).await;
    assert_eq!(status_of(admin_b.devices("user/42").await.unwrap_err()), 403);

    // Sign-up is closed until an administrator opens it.
    let http = reqwest::Client::new();
    let signup = |email: &'static str| {
        let http = http.clone();
        let url = format!("{relay}/admin/api/signup");
        async move { http.post(url).json(&json!({ "email": email, "password": "password123" })).send().await.unwrap().status().as_u16() }
    };
    assert_eq!(signup("carol@example.com").await, 403);
    admin.ok(Method::PUT, "/settings", Some(json!({ "signup_open": true }))).await;
    assert_eq!(signup("carol@example.com").await, 200);
    assert_eq!(signup("carol@example.com").await, 409);

    // Password change signs other sessions out.
    let alice2 = Console::login(&relay, "alice@example.com", "password123").await.unwrap();
    alice.ok(Method::POST, "/password", Some(json!({ "current": "password123", "new": "new-password-1" }))).await;
    assert_eq!(alice2.call(Method::GET, "/apps", None).await.0, 401);
    assert_eq!(alice.call(Method::GET, "/apps", None).await.0, 200);

    // The console page itself is served.
    let page = reqwest::get(format!("{relay}/admin")).await.unwrap();
    assert_eq!(page.status().as_u16(), 200);
}
