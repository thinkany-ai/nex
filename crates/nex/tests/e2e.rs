//! End-to-end: a real relay, three devices in one account reaching each
//! other's TCP and WebSocket services at the same time, plus pairing,
//! cross-account isolation, and revocation.

use futures_util::{SinkExt, StreamExt};
use nex::{forward_tcp, ExposedService, Host, NexClient};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

async fn start_relay() -> String {
    let state = nex_server::AppState::new(
        nex_server::db::Db::open(":memory:").unwrap(),
        nex_server::Config::default(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nex_server::serve(listener, state));
    format!("http://{addr}")
}

/// TCP echo server that prefixes replies, so we can tell services apart.
async fn tcp_echo(prefix: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let mut reply = prefix.as_bytes().to_vec();
                    reply.extend_from_slice(&buf[..n]);
                    if sock.write_all(&reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// WebSocket echo server, standing in for an agl endpoint.
async fn ws_echo(prefix: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        let reply = format!("{prefix}{}", t.as_str());
                        if ws.send(Message::Text(reply.into())).await.is_err() {
                            break;
                        }
                    }
                }
            });
        }
    });
    format!("ws://{addr}/ws")
}

fn spawn_host(
    client: &NexClient,
    services: Vec<ExposedService>,
) -> (CancellationToken, JoinHandle<anyhow::Result<()>>) {
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(Host::new(client.clone(), services).run(cancel.clone()));
    (cancel, handle)
}

async fn wait_online(observer: &NexClient, device_id: &str) {
    for _ in 0..100 {
        let devices = observer.devices().await.unwrap();
        if devices.iter().any(|d| d.id == device_id && d.online) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("device {device_id} never came online");
}

async fn tcp_roundtrip(client: &NexClient, device: &str, service: &str, payload: &str) -> String {
    let mut ws = client.connect(device, service).await.unwrap();
    ws.send(Message::Binary(payload.as_bytes().to_vec().into()))
        .await
        .unwrap();
    let mut got = Vec::new();
    while got.len() < payload.len() {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => got.extend_from_slice(&b),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    String::from_utf8(got).unwrap()
}

async fn ws_roundtrip(client: &NexClient, device: &str, service: &str, text: &str) -> String {
    let mut ws = client.connect(device, service).await.unwrap();
    ws.send(Message::Text(text.into())).await.unwrap();
    match ws.next().await {
        Some(Ok(Message::Text(t))) => t.to_string(),
        other => panic!("unexpected frame: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devices_reach_each_other() {
    let relay = start_relay().await;

    // Account with three devices: laptop bootstraps, phone and vps pair in.
    let laptop_cred = NexClient::bootstrap(&relay, "team", "laptop", None, None)
        .await
        .unwrap();
    let laptop = NexClient::new(&relay, laptop_cred).unwrap();

    let code = laptop.create_pairing_code().await.unwrap();
    assert_eq!(code.code.len(), 9, "XXXX-XXXX");
    let vps = NexClient::new(
        &relay,
        NexClient::claim(&relay, &code.code, "vps", None)
            .await
            .unwrap(),
    )
    .unwrap();

    // Codes are single use.
    let reuse = NexClient::claim(&relay, &code.code, "again", None).await;
    assert!(reuse.is_err(), "pairing code reused");

    // Case and dashes do not matter.
    let code2 = laptop.create_pairing_code().await.unwrap();
    let typed = code2.code.replace('-', "").to_lowercase();
    let phone = NexClient::new(
        &relay,
        NexClient::claim(&relay, &typed, "phone", None)
            .await
            .unwrap(),
    )
    .unwrap();

    // laptop exposes a TCP service and an agl-style WebSocket service;
    // vps exposes its own TCP service. Each device is both host and client.
    let laptop_shell = tcp_echo("laptop:").await;
    let laptop_agl = ws_echo("laptop-agl:").await;
    let vps_shell = tcp_echo("vps:").await;
    let (laptop_stop, laptop_task) = spawn_host(
        &laptop,
        vec![
            ExposedService::tcp("shell", &laptop_shell).unwrap(),
            ExposedService::ws("agl", &laptop_agl).unwrap(),
        ],
    );
    let (vps_stop, vps_task) = spawn_host(
        &vps,
        vec![ExposedService::tcp("shell", &vps_shell).unwrap()],
    );
    wait_online(&phone, laptop.device_id()).await;
    wait_online(&phone, vps.device_id()).await;

    let listed = phone.devices().await.unwrap();
    assert_eq!(listed.len(), 3);
    let me = listed.iter().find(|d| d.is_self).unwrap();
    assert_eq!(me.name, "phone");
    assert!(!me.online, "phone is a client only");
    let l = listed.iter().find(|d| d.name == "laptop").unwrap();
    assert_eq!(l.services.len(), 2);

    // Many streams at once, in every direction, addressed by name and by id.
    let (a, b, c, d, e) = tokio::join!(
        tcp_roundtrip(&phone, "laptop", "shell", "hello"),
        tcp_roundtrip(&phone, vps.device_id(), "shell", "hi"),
        ws_roundtrip(&phone, "laptop", "agl", "ping"),
        tcp_roundtrip(&vps, "laptop", "shell", "from-vps"),
        tcp_roundtrip(&laptop, "vps", "shell", "from-laptop"),
    );
    assert_eq!(a, "laptop:hello");
    assert_eq!(b, "vps:hi");
    assert_eq!(c, "laptop-agl:ping");
    assert_eq!(d, "laptop:from-vps");
    assert_eq!(e, "vps:from-laptop");

    // A long-lived stream carries many messages.
    let mut agl = phone.connect("laptop", "agl").await.unwrap();
    for i in 0..50 {
        agl.send(Message::Text(format!("m{i}").into()))
            .await
            .unwrap();
        match agl.next().await {
            Some(Ok(Message::Text(t))) => assert_eq!(t.as_str(), format!("laptop-agl:m{i}")),
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    // Port forwarding: a plain TCP client on the phone talks to the vps.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    let fwd_stop = CancellationToken::new();
    tokio::spawn(forward_tcp(
        phone.clone(),
        "vps".into(),
        "shell".into(),
        listener,
        fwd_stop.clone(),
    ));
    let mut sock = tokio::net::TcpStream::connect(local).await.unwrap();
    sock.write_all(b"fwd").await.unwrap();
    let mut buf = [0u8; 7];
    sock.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"vps:fwd");
    fwd_stop.cancel();

    // Errors: unknown service, offline device.
    let err = phone
        .ticket("laptop", "nope")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not expose"), "{err}");
    let err = laptop
        .ticket("phone", "shell")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("offline"), "{err}");

    // A ticket works exactly once.
    let ticket = phone.ticket("vps", "shell").await.unwrap();
    let url = phone.ws_url(&ticket.path).unwrap();
    let first = tokio_tungstenite::connect_async(url.as_str()).await;
    assert!(first.is_ok());
    let second = tokio_tungstenite::connect_async(url.as_str()).await;
    assert!(second.is_err(), "ticket reused");

    // Another account cannot see or reach these devices.
    let other = NexClient::new(
        &relay,
        NexClient::bootstrap(&relay, "other", "x", None, None)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(other.devices().await.unwrap().len(), 1);
    assert!(other.ticket(laptop.device_id(), "shell").await.is_err());
    assert!(other.revoke(laptop.device_id()).await.is_err());

    // Revoking the vps: its credential stops working, its host loop exits
    // with an error, open streams to it close, and it becomes unreachable.
    let mut open_stream = phone.connect("vps", "shell").await.unwrap();
    laptop.revoke(vps.device_id()).await.unwrap();
    let host_result = tokio::time::timeout(Duration::from_secs(5), vps_task)
        .await
        .unwrap()
        .unwrap();
    assert!(host_result.is_err(), "revoked host should stop");
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match open_stream.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "stream to revoked device stayed open");
    assert!(vps.devices().await.is_err());
    assert!(phone.ticket("vps", "shell").await.is_err());
    assert_eq!(phone.devices().await.unwrap().len(), 2);

    vps_stop.cancel();
    laptop_stop.cancel();
    assert!(laptop_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_reports_dead_local_service_and_reconnect_replaces_session() {
    let relay = start_relay().await;
    let a = NexClient::new(
        &relay,
        NexClient::bootstrap(&relay, "acct", "a", None, None)
            .await
            .unwrap(),
    )
    .unwrap();
    let code = a.create_pairing_code().await.unwrap();
    let b = NexClient::new(
        &relay,
        NexClient::claim(&relay, &code.code, "b", None)
            .await
            .unwrap(),
    )
    .unwrap();

    // Point a service at a port nothing listens on.
    let dead = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (stop1, t1) = spawn_host(
        &a,
        vec![ExposedService::tcp("db", &dead.to_string()).unwrap()],
    );
    wait_online(&b, a.device_id()).await;

    let mut ws = b.connect("a", "db").await.unwrap();
    match tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
    {
        Some(Ok(Message::Close(Some(frame)))) => {
            assert_eq!(u16::from(frame.code), nex::proto::close_code::REJECTED);
            assert!(frame.reason.contains("unavailable"), "{}", frame.reason);
        }
        other => panic!("expected a rejection close, got {other:?}"),
    }

    // A second session for the same device (e.g. app restarted) takes over.
    let echo = tcp_echo("v2:").await;
    let (stop2, _t2) = spawn_host(&a, vec![ExposedService::tcp("db", &echo).unwrap()]);
    let mut ok = false;
    for _ in 0..100 {
        let devices = b.devices().await.unwrap();
        let svc_ok = devices
            .iter()
            .find(|d| d.id == a.device_id())
            .is_some_and(|d| d.online);
        if svc_ok {
            if let Ok(mut ws) = b.connect("a", "db").await {
                ws.send(Message::Binary(b"x".to_vec().into()))
                    .await
                    .unwrap();
                if let Ok(Some(Ok(Message::Binary(r)))) =
                    tokio::time::timeout(Duration::from_secs(2), ws.next()).await
                {
                    if &r[..] == b"v2:x" {
                        ok = true;
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "newer session did not take over");
    // The replaced host stops for good instead of reconnecting and flapping.
    let err = tokio::time::timeout(Duration::from_secs(5), t1)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("another process"), "{err}");
    stop1.cancel();
    stop2.cancel();
}

async fn start_relay_with(config: nex_server::Config) -> String {
    let state = nex_server::AppState::new(nex_server::db::Db::open(":memory:").unwrap(), config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nex_server::serve(listener, state));
    format!("http://{addr}")
}

fn status_of(err: anyhow::Error) -> u16 {
    err.downcast::<nex::ApiError>().expect("an ApiError").status
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn product_backend_issues_devices_for_its_users() {
    let relay = start_relay_with(nex_server::Config {
        admin_key: Some("secret".into()),
        ..Default::default()
    })
    .await;
    let admin = nex::NexAdmin::new(&relay, "secret").unwrap();
    let issue = |user: &str, device: &str, app: &str| nex::proto::IssueDeviceRequest {
        external_account_id: user.into(),
        account_name: Some(format!("{user} account")),
        device_name: device.into(),
        app: Some(app.into()),
    };

    // Same external user -> same nex account, across products.
    let mac = admin
        .issue_device(&issue("user/42", "mac", "termany"))
        .await
        .unwrap();
    let phone = admin
        .issue_device(&issue("user/42", "phone", "dscode"))
        .await
        .unwrap();
    let stranger = admin
        .issue_device(&issue("user/7", "pc", "termany"))
        .await
        .unwrap();
    assert_eq!(mac.account_id, phone.account_id);
    assert_ne!(mac.account_id, stranger.account_id);

    let mac = NexClient::new(&relay, mac).unwrap();
    let phone = NexClient::new(&relay, phone).unwrap();
    let stranger = NexClient::new(&relay, stranger).unwrap();

    let me = phone.whoami().await.unwrap();
    assert_eq!(
        (me.device_name.as_str(), me.app.as_deref()),
        ("phone", Some("dscode"))
    );

    // Built-in echo, reached from the other device.
    let (cancel, _host) = spawn_host(&mac, vec![ExposedService::echo()]);
    wait_online(&phone, mac.device_id()).await;
    assert_eq!(ws_roundtrip(&phone, "mac", "echo", "hi").await, "hi");
    assert!(stranger.ticket(mac.device_id(), "echo").await.is_err());

    // Rename is visible to the account and to the admin API.
    phone.rename(mac.device_id(), "studio").await.unwrap();
    let listed = admin.devices("user/42").await.unwrap();
    let studio = listed.iter().find(|d| d.id == mac.device_id()).unwrap();
    assert_eq!(
        (studio.name.as_str(), studio.app.as_deref(), studio.online),
        ("studio", Some("termany"), true)
    );
    assert!(listed.iter().all(|d| !d.is_self));
    assert_eq!(
        status_of(stranger.rename(mac.device_id(), "x").await.unwrap_err()),
        404
    );

    // Admin revoke: the device is cut off at once.
    admin.revoke("user/42", phone.device_id()).await.unwrap();
    assert_eq!(status_of(phone.whoami().await.unwrap_err()), 401);
    assert_eq!(
        status_of(admin.revoke("user/7", mac.device_id()).await.unwrap_err()),
        404
    );
    assert_eq!(status_of(admin.devices("nobody").await.unwrap_err()), 404);

    // Wrong key, and the bootstrap endpoint, are both guarded.
    let wrong = nex::NexAdmin::new(&relay, "nope").unwrap();
    assert_eq!(status_of(wrong.devices("user/42").await.unwrap_err()), 403);
    let err = NexClient::bootstrap(&relay, "x", "y", None, None)
        .await
        .unwrap_err();
    assert_eq!(status_of(err), 403);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_api_is_off_without_a_key_and_streams_are_capped() {
    let relay = start_relay_with(nex_server::Config {
        max_streams_per_device: 2,
        ..Default::default()
    })
    .await;
    let admin = nex::NexAdmin::new(&relay, "anything").unwrap();
    assert_eq!(status_of(admin.devices("u").await.unwrap_err()), 403);

    let a = NexClient::new(
        &relay,
        NexClient::bootstrap(&relay, "acct", "a", Some("cli"), None)
            .await
            .unwrap(),
    )
    .unwrap();
    let code = a.create_pairing_code().await.unwrap();
    let b = NexClient::new(
        &relay,
        NexClient::claim(&relay, &code.code, "b", None)
            .await
            .unwrap(),
    )
    .unwrap();
    let (cancel, _host) = spawn_host(&a, vec![ExposedService::echo()]);
    wait_online(&b, a.device_id()).await;

    let mut s1 = b.connect("a", "echo").await.unwrap();
    let mut s2 = b.connect("a", "echo").await.unwrap();
    for s in [&mut s1, &mut s2] {
        s.send(Message::Text("x".into())).await.unwrap();
        assert!(matches!(s.next().await, Some(Ok(Message::Text(_)))));
    }
    let mut s3 = b.connect("a", "echo").await.unwrap();
    match s3.next().await {
        Some(Ok(Message::Close(Some(f)))) => {
            assert_eq!(u16::from(f.code), nex::proto::close_code::BUSY)
        }
        other => panic!("expected BUSY close, got {other:?}"),
    }

    // Closing one frees a slot.
    s1.close(None).await.unwrap();
    drop(s1);
    let mut ok = false;
    for _ in 0..50 {
        let mut s = b.connect("a", "echo").await.unwrap();
        s.send(Message::Text("y".into())).await.unwrap();
        if matches!(s.next().await, Some(Ok(Message::Text(_)))) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "slot was never released");
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_client_learns_at_once_when_the_host_goes_offline() {
    let relay = start_relay().await;
    let a = NexClient::new(
        &relay,
        NexClient::bootstrap(&relay, "acct", "a", None, None)
            .await
            .unwrap(),
    )
    .unwrap();
    let code = a.create_pairing_code().await.unwrap();
    let b = NexClient::new(
        &relay,
        NexClient::claim(&relay, &code.code, "b", None)
            .await
            .unwrap(),
    )
    .unwrap();

    // A local "WebSocket server" that accepts TCP but never answers the
    // handshake, so the host never gets to accept the stream.
    let stuck = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", stuck.local_addr().unwrap());
    let (stop, host) = spawn_host(&a, vec![ExposedService::ws("slow", &url).unwrap()]);
    wait_online(&b, a.device_id()).await;

    let mut ws = b.connect("a", "slow").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await; // the open is now pending
    stop.cancel();
    host.await.unwrap().unwrap();
    let started = std::time::Instant::now();
    match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
        Ok(Some(Ok(Message::Close(Some(f))))) => {
            assert_eq!(u16::from(f.code), nex::proto::close_code::OFFLINE)
        }
        other => panic!("expected an OFFLINE close, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(3));
    drop(stuck);
}

/// Copies `r` to `w` until EOF, swallowing bytes while `frozen` is set.
async fn copy_unless_frozen<R, W>(
    mut r: R,
    mut w: W,
    frozen: std::sync::Arc<std::sync::atomic::AtomicBool>,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if frozen.load(std::sync::atomic::Ordering::SeqCst) {
            continue;
        }
        if w.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
}

/// TCP proxy whose connections can be frozen: they stay open but carry
/// nothing, like a laptop that went to sleep.
async fn freezable_proxy(
    target: String,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let frozen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let flag = frozen.clone();
    tokio::spawn(async move {
        loop {
            let (inbound, _) = listener.accept().await.unwrap();
            let outbound = tokio::net::TcpStream::connect(&target).await.unwrap();
            let (ir, iw) = inbound.into_split();
            let (or, ow) = outbound.into_split();
            tokio::spawn(copy_unless_frozen(ir, ow, flag.clone()));
            tokio::spawn(copy_unless_frozen(or, iw, flag.clone()));
        }
    });
    (format!("http://{addr}"), frozen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_drops_a_host_that_went_silent() {
    let relay = start_relay_with(nex_server::Config {
        ping_interval: Duration::from_millis(100),
        ..Default::default()
    })
    .await;
    let (via_proxy, frozen) =
        freezable_proxy(relay.trim_start_matches("http://").to_string()).await;
    let cred = NexClient::bootstrap(&relay, "acct", "a", None, None)
        .await
        .unwrap();
    let a = NexClient::new(&relay, cred.clone()).unwrap();
    let code = a.create_pairing_code().await.unwrap();
    let b = NexClient::new(
        &relay,
        NexClient::claim(&relay, &code.code, "b", None)
            .await
            .unwrap(),
    )
    .unwrap();

    let a_via_proxy = NexClient::new(&via_proxy, cred).unwrap();
    let (stop, _host) = spawn_host(&a_via_proxy, vec![ExposedService::echo()]);
    wait_online(&b, a.device_id()).await;
    // Stays online across several ping periods while pongs flow.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(b
        .devices()
        .await
        .unwrap()
        .iter()
        .any(|d| d.id == a.device_id() && d.online));

    frozen.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut offline = false;
    for _ in 0..40 {
        if b.devices()
            .await
            .unwrap()
            .iter()
            .any(|d| d.id == a.device_id() && !d.online)
        {
            offline = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(offline, "relay still lists the silent host as online");
    assert_eq!(status_of(b.ticket("a", "echo").await.unwrap_err()), 404);
    stop.cancel();
}
