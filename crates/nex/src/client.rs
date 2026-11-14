use anyhow::{anyhow, Result};
use nex_proto::*;
use reqwest::Method;
use serde::{de::DeserializeOwned, Serialize};
use std::fmt;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, client::IntoClientRequest, http::header::AUTHORIZATION},
    MaybeTlsStream, WebSocketStream,
};
use url::Url;

pub type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A non-2xx response from the relay.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "relay returned {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// An authenticated device talking to one relay.
#[derive(Clone)]
pub struct NexClient {
    http: reqwest::Client,
    server: Url,
    credential: DeviceCredential,
}

impl NexClient {
    pub fn new(server: &str, credential: DeviceCredential) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::new(),
            server: base_url(server)?,
            credential,
        })
    }

    pub fn server(&self) -> &Url {
        &self.server
    }

    pub fn credential(&self) -> &DeviceCredential {
        &self.credential
    }

    pub fn device_id(&self) -> &str {
        &self.credential.device_id
    }

    /// Creates an account and registers the calling machine as its first device.
    pub async fn bootstrap(
        server: &str,
        account_name: &str,
        device_name: &str,
        app: Option<&str>,
        admin_key: Option<&str>,
    ) -> Result<DeviceCredential> {
        let url = base_url(server)?.join("v1/bootstrap")?;
        let mut req = reqwest::Client::new().post(url).json(&BootstrapRequest {
            account_name: account_name.into(),
            device_name: device_name.into(),
            app: app.map(Into::into),
        });
        if let Some(key) = admin_key {
            req = req.header(ADMIN_KEY_HEADER, key);
        }
        decode(req.send().await?).await
    }

    /// Registers a new device into an existing account with a pairing code.
    pub async fn claim(
        server: &str,
        code: &str,
        device_name: &str,
        app: Option<&str>,
    ) -> Result<DeviceCredential> {
        let url = base_url(server)?.join("v1/devices/claim")?;
        let resp = reqwest::Client::new()
            .post(url)
            .json(&ClaimRequest {
                code: code.into(),
                device_name: device_name.into(),
                app: app.map(Into::into),
            })
            .send()
            .await?;
        decode(resp).await
    }

    pub async fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let list: DeviceList = self.request(Method::GET, "v1/devices", None::<&()>).await?;
        Ok(list.devices)
    }

    /// The account and device this credential belongs to. Fails with
    /// [`ApiError`] status 401 once the device is revoked.
    pub async fn whoami(&self) -> Result<WhoAmI> {
        self.request(Method::GET, "v1/me", None::<&()>).await
    }

    pub async fn rename(&self, device_id: &str, name: &str) -> Result<()> {
        let body = RenameRequest { name: name.into() };
        let _: serde_json::Value = self
            .request(
                Method::PATCH,
                &format!("v1/devices/{device_id}"),
                Some(&body),
            )
            .await?;
        Ok(())
    }

    pub async fn create_pairing_code(&self) -> Result<PairingCode> {
        self.request(Method::POST, "v1/pairing-codes", None::<&()>)
            .await
    }

    pub async fn revoke(&self, device_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                Method::DELETE,
                &format!("v1/devices/{device_id}"),
                None::<&()>,
            )
            .await?;
        Ok(())
    }

    /// A single-use permission to open one stream. `device` is an id or an
    /// exact device name.
    pub async fn ticket(&self, device: &str, service: &str) -> Result<Ticket> {
        let body = TicketRequest {
            device: device.into(),
            service: service.into(),
        };
        self.request(Method::POST, "v1/tickets", Some(&body)).await
    }

    /// Opens a stream to `service` on `device`.
    ///
    /// For a `tcp` service, send and receive binary frames carrying the raw
    /// bytes. For a `ws` service, frames are passed to the remote WebSocket
    /// server unchanged.
    pub async fn connect(&self, device: &str, service: &str) -> Result<WsStream> {
        let ticket = self.ticket(device, service).await?;
        let url = self.ws_url(&ticket.path)?;
        let (ws, _) = connect_async(url.as_str()).await?;
        Ok(ws)
    }

    /// Relay URL for a path, with `ws://` / `wss://`.
    pub fn ws_url(&self, path: &str) -> Result<Url> {
        let mut url = self.server.join(path.trim_start_matches('/'))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| anyhow!("cannot use {scheme}:// for {url}"))?;
        Ok(url)
    }

    /// WebSocket to the relay authenticated with this device's token.
    pub(crate) async fn ws_authed(
        &self,
        path: &str,
    ) -> std::result::Result<WsStream, tungstenite::Error> {
        let url = self.ws_url(path).map_err(|e| {
            tungstenite::Error::Url(tungstenite::error::UrlError::UnableToConnect(e.to_string()))
        })?;
        let mut req = url.as_str().into_client_request()?;
        let auth = format!("Bearer {}", self.credential.device_token)
            .parse()
            .map_err(|_| tungstenite::Error::Url(tungstenite::error::UrlError::EmptyHostName))?;
        req.headers_mut().insert(AUTHORIZATION, auth);
        let (ws, _) = connect_async(req).await?;
        Ok(ws)
    }

    async fn request<T: DeserializeOwned, B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        let mut req = self
            .http
            .request(method, self.server.join(path)?)
            .bearer_auth(&self.credential.device_token);
        if let Some(body) = body {
            req = req.json(body);
        }
        decode(req.send().await?).await
    }
}

/// The relay's admin API, for product backends that map their own users
/// onto nex accounts. Holds the admin key: use it on servers only.
#[derive(Clone)]
pub struct NexAdmin {
    http: reqwest::Client,
    server: Url,
    admin_key: String,
}

impl NexAdmin {
    pub fn new(server: &str, admin_key: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::new(),
            server: base_url(server)?,
            admin_key: admin_key.into(),
        })
    }

    /// Registers a device for the user `external_account_id`, creating the
    /// nex account on first use. Hand the credential to that device.
    pub async fn issue_device(&self, req: &IssueDeviceRequest) -> Result<DeviceCredential> {
        let url = self.server.join("v1/admin/devices")?;
        self.send(self.http.post(url).json(req)).await
    }

    pub async fn devices(&self, external_account_id: &str) -> Result<Vec<DeviceInfo>> {
        let url = self.account_url(external_account_id, &[])?;
        let list: DeviceList = self.send(self.http.get(url)).await?;
        Ok(list.devices)
    }

    pub async fn revoke(&self, external_account_id: &str, device_id: &str) -> Result<()> {
        let url = self.account_url(external_account_id, &[device_id])?;
        let _: serde_json::Value = self.send(self.http.delete(url)).await?;
        Ok(())
    }

    /// `v1/admin/accounts/{id}/devices[/{extra}]`, with each segment escaped.
    fn account_url(&self, external_account_id: &str, extra: &[&str]) -> Result<Url> {
        let mut url = self.server.clone();
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|_| anyhow!("relay URL cannot be a base"))?;
            segs.pop_if_empty()
                .extend(["v1", "admin", "accounts", external_account_id, "devices"])
                .extend(extra);
        }
        Ok(url)
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T> {
        decode(req.header(ADMIN_KEY_HEADER, &self.admin_key).send().await?).await
    }
}

/// Normalizes the relay base URL so relative joins keep any path prefix.
fn base_url(server: &str) -> Result<Url> {
    let mut url = Url::parse(server)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!("relay URL must start with http:// or https://"));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp.json().await?);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ErrorBody>(&body)
        .map(|e| e.error)
        .unwrap_or(body);
    Err(ApiError {
        status: status.as_u16(),
        message,
    }
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(server: &str) -> NexClient {
        NexClient::new(
            server,
            DeviceCredential {
                account_id: "a".into(),
                device_id: "d".into(),
                device_token: "t".into(),
            },
        )
        .unwrap()
    }

    #[test]
    fn ws_url_keeps_prefix_and_scheme() {
        let c = client("https://relay.example.com/nex");
        assert_eq!(
            c.ws_url("/v1/connect?ticket=abc").unwrap().as_str(),
            "wss://relay.example.com/nex/v1/connect?ticket=abc"
        );
        let c = client("http://127.0.0.1:8080");
        assert_eq!(
            c.ws_url("v1/host").unwrap().as_str(),
            "ws://127.0.0.1:8080/v1/host"
        );
    }
}
