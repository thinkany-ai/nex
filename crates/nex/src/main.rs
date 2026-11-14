use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use nex::{config, forward_tcp, ExposedService, Host, NexClient};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

/// nex: reach your devices and the agents on them from anywhere.
#[derive(Parser)]
#[command(name = "nex", version)]
struct Cli {
    /// Credential file (default: ~/.config/nex/config.json).
    #[arg(long, global = true, env = "NEX_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an account and register this machine as its first device.
    Init {
        /// Relay base URL, e.g. https://nex.example.com
        #[arg(long, env = "NEX_SERVER")]
        server: String,
        #[arg(long)]
        account: String,
        /// Device name (default: hostname).
        #[arg(long)]
        name: Option<String>,
        #[arg(long, env = "NEX_ADMIN_KEY", hide_env_values = true)]
        admin_key: Option<String>,
        /// Product label stored with the device (e.g. termany).
        #[arg(long)]
        app: Option<String>,
        /// Overwrite an existing credential.
        #[arg(long)]
        force: bool,
    },
    /// Create a one-time code for adding another device to this account.
    Pair,
    /// Add this machine to an account with a pairing code.
    Join {
        code: String,
        #[arg(long, env = "NEX_SERVER")]
        server: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// List the devices in this account.
    Devices {
        #[arg(long)]
        json: bool,
    },
    /// Revoke a device (id or name). It is disconnected immediately.
    Revoke { device: String },
    /// Go online and expose local services to the account's other devices.
    Up {
        /// NAME=tcp://HOST:PORT or NAME=ws://HOST:PORT/PATH (repeatable).
        #[arg(long = "expose", value_name = "NAME=URL")]
        expose: Vec<String>,
        /// Also expose the built-in `echo` service (for `nex ping` and tests).
        #[arg(long)]
        echo: bool,
    },
    /// Measure round-trip time to a device through the relay (needs `nex up --echo` there).
    Ping {
        /// Device id or name.
        device: String,
        #[arg(short = 'c', long, default_value_t = 10)]
        count: u32,
        /// Seconds between pings.
        #[arg(short = 'i', long, default_value_t = 0.5)]
        interval: f64,
        /// A `ws` service that echoes messages back.
        #[arg(long, default_value = "echo")]
        service: String,
    },
    /// Listen on a local port and tunnel connections to DEVICE/SERVICE (tcp services).
    Forward {
        /// DEVICE/SERVICE, where DEVICE is an id or name.
        target: String,
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: String,
    },
    /// Print a one-time WebSocket URL for DEVICE/SERVICE (valid ~60s).
    Ticket { target: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nex=info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(config::default_path);

    match cli.cmd {
        Cmd::Init {
            server,
            account,
            name,
            admin_key,
            app,
            force,
        } => {
            refuse_overwrite(&path, force)?;
            let name = name.unwrap_or_else(hostname);
            let credential = NexClient::bootstrap(
                &server,
                &account,
                &name,
                app.as_deref(),
                admin_key.as_deref(),
            )
            .await?;
            config::save(
                &path,
                &config::StoredConfig {
                    server,
                    credential: credential.clone(),
                },
            )?;
            println!(
                "account {}  device {} ({name})",
                credential.account_id, credential.device_id
            );
            println!("saved to {}", path.display());
        }
        Cmd::Join {
            code,
            server,
            name,
            app,
            force,
        } => {
            refuse_overwrite(&path, force)?;
            let name = name.unwrap_or_else(hostname);
            let credential = NexClient::claim(&server, &code, &name, app.as_deref()).await?;
            config::save(
                &path,
                &config::StoredConfig {
                    server,
                    credential: credential.clone(),
                },
            )?;
            println!(
                "joined account {} as device {} ({name})",
                credential.account_id, credential.device_id
            );
        }
        cmd => {
            let stored = config::load(&path)?;
            let client = NexClient::new(&stored.server, stored.credential)?;
            run(client, cmd, &stored.server).await?;
        }
    }
    Ok(())
}

async fn run(client: NexClient, cmd: Cmd, server: &str) -> Result<()> {
    match cmd {
        Cmd::Pair => {
            let code = client.create_pairing_code().await?;
            let minutes = (code.expires_at - now()).max(0) / 60;
            println!("{}", code.code);
            eprintln!(
                "on the new device, within {minutes} min:\n  nex join {} --server {server}",
                code.code
            );
        }
        Cmd::Devices { json } => {
            let devices = client.devices().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&devices)?);
                return Ok(());
            }
            for d in devices {
                let services: Vec<_> = d
                    .services
                    .iter()
                    .map(|s| format!("{}({:?})", s.name, s.kind).to_lowercase())
                    .collect();
                println!(
                    "{} {:<20} {:<10} {:<8} {}{}",
                    d.id,
                    d.name,
                    d.app.as_deref().unwrap_or("-"),
                    if d.online { "online" } else { "offline" },
                    services.join(" "),
                    if d.is_self { "  <- this device" } else { "" },
                );
            }
        }
        Cmd::Revoke { device } => {
            let devices = client.devices().await?;
            let matches: Vec<_> = devices
                .iter()
                .filter(|d| d.id == device || d.name == device)
                .collect();
            match matches.as_slice() {
                [d] => {
                    client.revoke(&d.id).await?;
                    println!("revoked {} ({})", d.id, d.name);
                }
                [] => bail!("no device '{device}'"),
                _ => bail!("several devices are named '{device}'; use the id"),
            }
        }
        Cmd::Up { expose, echo } => {
            let mut services = expose
                .iter()
                .map(|s| ExposedService::parse(s))
                .collect::<Result<Vec<_>>>()?;
            if echo {
                services.push(ExposedService::echo());
            }
            if services.is_empty() {
                bail!("nothing to expose: pass --expose NAME=URL and/or --echo");
            }
            for s in &services {
                eprintln!("exposing {} -> {:?}", s.name, s.target);
            }
            let cancel = shutdown_token();
            Host::new(client, services).run(cancel).await?;
        }
        Cmd::Forward { target, listen } => {
            let (device, service) = split_target(&target)?;
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            eprintln!(
                "forwarding {} -> {device}/{service}",
                listener.local_addr()?
            );
            forward_tcp(client, device, service, listener, shutdown_token()).await?;
        }
        Cmd::Ticket { target } => {
            let (device, service) = split_target(&target)?;
            let ticket = client.ticket(&device, &service).await?;
            println!("{}", client.ws_url(&ticket.path)?);
        }
        Cmd::Ping {
            device,
            count,
            interval,
            service,
        } => {
            ping(&client, &device, &service, count, interval).await?;
        }
        Cmd::Init { .. } | Cmd::Join { .. } => unreachable!(),
    }
    Ok(())
}

async fn ping(
    client: &NexClient,
    device: &str,
    service: &str,
    count: u32,
    interval: f64,
) -> Result<()> {
    let started = Instant::now();
    let mut ws = client.connect(device, service).await?;
    println!(
        "connected to {device}/{service} via {} in {:.0} ms",
        client.server(),
        ms(started.elapsed())
    );

    let mut rtts = Vec::new();
    for seq in 0..count {
        if seq > 0 {
            tokio::time::sleep(Duration::from_secs_f64(interval)).await;
        }
        let payload = format!("nex-ping {seq} {}", now());
        let sent = Instant::now();
        ws.send(Message::Text(payload.clone().into())).await?;
        loop {
            let msg = match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
                Err(_) => bail!("seq={seq}: no reply within 10s"),
                Ok(None) => bail!("stream ended"),
                Ok(Some(msg)) => msg?,
            };
            match msg {
                Message::Text(text) if text.as_str() == payload => break,
                Message::Close(frame) => match frame {
                    Some(f) => bail!("stream closed: {} {}", u16::from(f.code), f.reason),
                    None => bail!("stream closed"),
                },
                _ => {}
            }
        }
        let rtt = ms(sent.elapsed());
        println!("seq={seq} rtt={rtt:.1} ms");
        rtts.push(rtt);
    }
    let _ = ws.close(None).await;
    if !rtts.is_empty() {
        let min = rtts.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = rtts.iter().cloned().fold(0.0, f64::max);
        let avg = rtts.iter().sum::<f64>() / rtts.len() as f64;
        println!(
            "{} pings: min {min:.1} / avg {avg:.1} / max {max:.1} ms",
            rtts.len()
        );
    }
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn split_target(target: &str) -> Result<(String, String)> {
    match target.rsplit_once('/') {
        Some((d, s)) if !d.is_empty() && !s.is_empty() => Ok((d.into(), s.into())),
        _ => bail!("expected DEVICE/SERVICE, got '{target}'"),
    }
}

fn refuse_overwrite(path: &std::path::Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to replace it",
            path.display()
        );
    }
    Ok(())
}

fn shutdown_token() -> CancellationToken {
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        t.cancel();
    });
    token
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().trim_end_matches(".local").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "device".into())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
