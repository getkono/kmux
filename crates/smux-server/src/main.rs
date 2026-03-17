mod app;
mod auth;
mod connection;
mod relay;
mod scrollback;
mod tls;

use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use app::ServerApp;
use auth::generate_token;

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

    let acceptor = tls::make_acceptor(tls_config);

    let token = generate_token();
    println!("Auth token: {token}");

    let app = Arc::new(ServerApp::new(token));

    let addr = format!("{}:{}", cli.bind, cli.port);
    let listener = TcpListener::bind(&addr).await?;
    info!("Listening on wss://{addr}");

    loop {
        let (tcp_stream, peer_addr) = listener.accept().await?;
        info!("TCP connection from {peer_addr}");

        let acceptor = acceptor.clone();
        let app = Arc::clone(&app);

        tokio::spawn(async move {
            match acceptor.accept(tcp_stream).await {
                Ok(tls_stream) => match tokio_tungstenite::accept_async(tls_stream).await {
                    Ok(ws_stream) => {
                        info!("WebSocket connection from {peer_addr}");
                        connection::handle(ws_stream, app).await;
                    }
                    Err(e) => error!("WebSocket upgrade from {peer_addr} failed: {e}"),
                },
                Err(e) => error!("TLS handshake from {peer_addr} failed: {e}"),
            }
        });
    }
}
