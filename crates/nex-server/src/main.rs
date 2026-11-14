use anyhow::Result;
use clap::Parser;
use nex_server::{db::Db, AppState, Config};

/// nex relay server.
#[derive(Parser)]
#[command(name = "nex-server", version)]
struct Args {
    /// Address to listen on. Put TLS (wss://) in front with a reverse proxy
    /// or load balancer in production.
    #[arg(long, env = "NEX_LISTEN", default_value = "127.0.0.1:8080")]
    listen: String,

    /// SQLite database path (":memory:" for an ephemeral relay).
    #[arg(long, env = "NEX_DB", default_value = "nex.db")]
    db: String,

    /// Admin key (header x-nex-admin-key). Required to create accounts and
    /// enables the /v1/admin API for product backends.
    #[arg(long, env = "NEX_ADMIN_KEY", hide_env_values = true)]
    admin_key: Option<String>,

    /// Concurrent streams one device may take part in.
    #[arg(long, env = "NEX_MAX_STREAMS", default_value_t = Config::default().max_streams_per_device)]
    max_streams: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nex_server=info,tower_http=warn".into()),
        )
        .init();

    let args = Args::parse();
    if args.admin_key.is_none() {
        tracing::warn!("no admin key set: anyone who can reach this relay can create accounts");
    }
    let config = Config {
        admin_key: args.admin_key,
        max_streams_per_device: args.max_streams,
        ..Config::default()
    };
    let state = AppState::new(Db::open(&args.db)?, config);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!("nex relay listening on {}", listener.local_addr()?);
    nex_server::serve(listener, state).await?;
    Ok(())
}
