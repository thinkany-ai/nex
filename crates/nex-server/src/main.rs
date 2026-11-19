use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use nex_server::{
    console,
    db::{Db, KeyError, ROLE_ADMIN, ROLE_MEMBER},
    AppState, Config,
};
use std::{io::{BufRead, IsTerminal, Write}, net::{IpAddr, SocketAddr}};

/// nex relay server. Without a subcommand it serves.
#[derive(Parser)]
#[command(name = "nex-server", version)]
struct Args {
    /// Postgres URL (postgres://user:pass@host:5432/db?sslmode=…).
    #[arg(long, env = "NEX_DATABASE_URL", hide_env_values = true, global = true)]
    database_url: Option<String>,

    /// Keep every table in this schema (created if missing) instead of
    /// `public`. Tests use it to share one database.
    #[arg(long, env = "NEX_DATABASE_SCHEMA", global = true)]
    database_schema: Option<String>,

    #[command(flatten)]
    serve: ServeArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Args)]
struct ServeArgs {
    /// Address to listen on. Put TLS (wss://) in front with a reverse proxy
    /// or load balancer in production.
    #[arg(long, env = "NEX_LISTEN", default_value = "127.0.0.1:8080")]
    listen: String,

    /// The console's public origin, e.g. https://api.nex.surf.
    #[arg(long, env = "NEX_PUBLIC_URL")]
    public_url: Option<String>,

    /// Legacy: an extra app key for the built-in default app. Prefer app
    /// keys created in the console.
    #[arg(long, env = "NEX_ADMIN_KEY", hide_env_values = true)]
    admin_key: Option<String>,

    /// Let anyone who reaches the relay create accounts (in the default app)
    /// without a key. Local development and tests only.
    #[arg(long, env = "NEX_OPEN_BOOTSTRAP")]
    open_bootstrap: bool,

    /// Pod IP advertised to other Nex replicas. Omit for standalone operation.
    #[arg(long, env = "NEX_PEER_IP")]
    peer_ip: Option<IpAddr>,

    /// Private listener port for replica-to-replica WebSocket upgrades.
    #[arg(long, env = "NEX_PEER_PORT", default_value_t = 8081)]
    peer_port: u16,

    /// Concurrent streams one device may take part in.
    #[arg(long, env = "NEX_MAX_STREAMS", default_value_t = Config::default().max_streams_per_device)]
    max_streams: usize,
}

#[derive(Subcommand)]
enum Command {
    /// Console users.
    #[command(subcommand)]
    User(UserCommand),
    /// Apps (one per agent product).
    #[command(subcommand)]
    App(AppCommand),
}

#[derive(Subcommand)]
enum UserCommand {
    /// Creates a console user. The password is read from stdin.
    Create {
        email: String,
        /// admin or member.
        #[arg(long, default_value = ROLE_ADMIN)]
        role: String,
    },
    /// Sets a console user's password, read from stdin.
    Password { email: String },
}

#[derive(Subcommand)]
enum AppCommand {
    /// Creates an app and prints its first key (shown once).
    Create {
        /// Email of the console user who owns it.
        #[arg(long)]
        owner: String,
        #[arg(long)]
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nex_server=info,tower_http=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let url = args
        .database_url
        .context("set NEX_DATABASE_URL (or --database-url) to a Postgres URL")?;
    let db = Db::connect(&url, args.database_schema.as_deref()).await?;

    match args.command {
        None => serve(db, args.serve).await,
        Some(Command::User(UserCommand::Create { email, role })) => {
            if role != ROLE_ADMIN && role != ROLE_MEMBER {
                bail!("role must be admin or member");
            }
            let email = console::normalize_email(&email).map_err(|_| anyhow::anyhow!("invalid email"))?;
            let password = read_password()?;
            let hash = console::hash_password(password).await?;
            match db.create_user(&email, &hash, &role).await? {
                Some(u) => println!("created {role} {} ({})", u.email, u.id),
                None => bail!("{email} already exists"),
            }
            Ok(())
        }
        Some(Command::User(UserCommand::Password { email })) => {
            let email = console::normalize_email(&email).map_err(|_| anyhow::anyhow!("invalid email"))?;
            let user = db.user_by_email(&email).await?.context("no such user")?;
            let password = read_password()?;
            db.set_password(&user.id, &console::hash_password(password).await?).await?;
            println!("password updated for {email}");
            Ok(())
        }
        Some(Command::App(AppCommand::Create { owner, name })) => {
            let owner = console::normalize_email(&owner).map_err(|_| anyhow::anyhow!("invalid email"))?;
            let user = db.user_by_email(&owner).await?.context("no such owner")?;
            let app = db.create_app(&user.id, name.trim()).await?;
            let key = match db.create_key(&app.id, "default").await? {
                Ok((_, key)) => key,
                Err(KeyError::TooMany | KeyError::NoSuchApp) => bail!("could not create a key"),
            };
            println!("app_id={}\napp_key={key}", app.id);
            Ok(())
        }
    }
}

fn read_password() -> Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("password (8+ characters, echoed): ");
        std::io::stderr().flush()?;
    }
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let password = line.trim_end_matches(['\r', '\n']).to_string();
    console::valid_password(&password).map_err(|_| anyhow::anyhow!("password must be 8-256 characters"))?;
    Ok(password)
}

async fn serve(db: Db, args: ServeArgs) -> Result<()> {
    if args.open_bootstrap {
        tracing::warn!("open bootstrap: anyone who can reach this relay can create accounts");
    }
    let config = Config {
        admin_key: args.admin_key,
        open_bootstrap: args.open_bootstrap,
        public_url: args.public_url.map(|u| u.trim_end_matches('/').to_string()),
        max_streams_per_device: args.max_streams,
        peer_addr: args.peer_ip.map(|ip| SocketAddr::new(ip, args.peer_port)),
        ..Config::default()
    };
    let state = AppState::new(db, config);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!("nex relay listening on {} (console at /admin)", listener.local_addr()?);
    nex_server::serve(listener, state).await?;
    Ok(())
}
