#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
mod app;
mod auth;
mod backend;
#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
mod connection;
mod daemon;
mod diff_engine;
#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
mod relay;
mod scrollback;
#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
mod term_state;
#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
mod tls;
#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
mod wordlist;

#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
use std::net::SocketAddr;
#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
use std::sync::Arc;
#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
use std::time::Instant;

#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
use clap::Parser;
#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
use app::ServerApp;
#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
use auth::{generate_token, persist_token};

#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
#[derive(Parser, Debug)]
#[command(name = "kmux-server", about = "kmux remote terminal server")]
struct Cli {
    /// Address to bind (default: all interfaces)
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Port to listen on (0 = pick a random available port)
    #[arg(long, default_value_t = 8443)]
    port: u16,

    /// Path to a PEM certificate file (required unless --self-signed)
    #[arg(long)]
    cert: Option<String>,

    /// Path to a PEM private key file (required unless --self-signed)
    #[arg(long)]
    key: Option<String>,

    /// Generate an in-memory self-signed certificate (for development)
    #[arg(long)]
    self_signed: bool,

    /// Run as a background daemon (double-fork, PID file, Unix socket control).
    /// Daemonization happens before the tokio runtime starts, so fork-safety is maintained.
    #[arg(long)]
    daemon: bool,
}

fn main() -> anyhow::Result<()> {
    #[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
    {
        // Parse CLI before daemonizing so --help/--version work in the foreground.
        let cli = Cli::parse();

        if cli.daemon {
            let pid_path = kmux_protocol::dirs::pid_path()?;
            daemon::daemonize_process(&pid_path)?;
            // After this point we are in the daemonized child process.
            // Logging to a file would be set up here in a production system;
            // for now we rely on tracing going to /dev/null (stdio is closed).
        }

        // Initialize tracing after daemonize (child process has fresh fds).
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive("kmux_server=info".parse()?),
            )
            .init();

        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async_main(cli))?;
    }

    #[cfg(not(any(feature = "backend-alacritty", feature = "backend-termwiz")))]
    {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .init();
        tracing::error!(
            "kmux-server requires a backend feature: backend-alacritty or backend-termwiz"
        );
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(any(feature = "backend-alacritty", feature = "backend-termwiz"))]
async fn async_main(cli: Cli) -> anyhow::Result<()> {
    info!(backend = term_state::BACKEND_NAME, "terminal backend");

    let tls_config = if cli.self_signed {
        let (cert, key) = tls::generate_self_signed()?;
        tls::build_tls_config(cert, key)?
    } else {
        let cert_path = cli
            .cert
            .ok_or_else(|| anyhow::anyhow!("--cert is required without --self-signed"))?;
        let key_path = cli
            .key
            .ok_or_else(|| anyhow::anyhow!("--key is required without --self-signed"))?;
        tls::load_tls_config(&cert_path, &key_path)?
    };

    let quinn_config = tls::build_quinn_config(tls_config)?;

    let token = generate_token();
    match persist_token(&token) {
        Ok(path) => info!("Auth token persisted to {}", path.display()),
        Err(e) => tracing::warn!("Failed to persist auth token: {e}"),
    }
    println!("Auth token: {token}");

    let app = Arc::new(ServerApp::new(token.clone()));

    let addr: SocketAddr = format!("{}:{}", cli.bind, cli.port).parse()?;
    let endpoint = quinn::Endpoint::server(quinn_config, addr)?;
    let actual_addr = endpoint.local_addr()?;
    let actual_port = actual_addr.port();
    info!("Listening on quic://{actual_addr}");

    if cli.daemon {
        let socket_path = kmux_protocol::dirs::socket_path()?;
        let pid_path = kmux_protocol::dirs::pid_path()?;
        let start_time = Instant::now();
        let token_clone = token.clone();
        tokio::spawn(async move {
            daemon::serve_control_socket(
                socket_path,
                pid_path,
                actual_port,
                token_clone,
                start_time,
            )
            .await;
        });
    }

    while let Some(incoming) = endpoint.accept().await {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    let remote = conn.remote_address();
                    info!("QUIC connection from {remote}");
                    connection::handle(conn, app).await;
                }
                Err(e) => error!("QUIC connection failed: {e}"),
            }
        });
    }

    Ok(())
}
