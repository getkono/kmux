#[cfg(feature = "backend-alacritty")]
mod app;
mod auth;
mod backend;
#[cfg(feature = "backend-alacritty")]
mod connection;
mod diff_engine;
#[cfg(feature = "backend-alacritty")]
mod relay;
mod scrollback;
#[cfg(feature = "backend-alacritty")]
mod term_state;
#[cfg(feature = "backend-alacritty")]
mod tls;

#[cfg(feature = "backend-alacritty")]
use std::net::SocketAddr;
#[cfg(feature = "backend-alacritty")]
use std::sync::Arc;

#[cfg(feature = "backend-alacritty")]
use clap::Parser;
#[cfg(feature = "backend-alacritty")]
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[cfg(feature = "backend-alacritty")]
use app::ServerApp;
#[cfg(feature = "backend-alacritty")]
use auth::generate_token;

#[cfg(feature = "backend-alacritty")]
#[derive(Parser, Debug)]
#[command(name = "smux-server", about = "smux remote terminal server")]
struct Cli {
    /// Address to bind (default: all interfaces)
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Port to listen on
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("smux_server=info".parse()?))
        .init();

    #[cfg(feature = "backend-alacritty")]
    {
        let cli = Cli::parse();

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
        println!("Auth token: {token}");

        let app = Arc::new(ServerApp::new(token));

        let addr: SocketAddr = format!("{}:{}", cli.bind, cli.port).parse()?;
        let endpoint = quinn::Endpoint::server(quinn_config, addr)?;
        info!("Listening on quic://{addr}");

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
    }

    #[cfg(not(feature = "backend-alacritty"))]
    {
        tracing::error!("smux-server requires the backend-alacritty feature to run");
        std::process::exit(1);
    }

    Ok(())
}
