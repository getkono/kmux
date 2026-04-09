mod app;
mod session_bar;
mod shortcut;
mod status_bar;
mod terminal_view;
mod theme;

use clap::Parser;
use rand::RngCore;
use tracing_subscriber::EnvFilter;

use app::kmuxApp;

#[derive(Parser, Debug)]
#[command(name = "kmux-gui", about = "kmux remote terminal client (GUI)")]
struct Cli {
    /// Accept self-signed / invalid TLS certificates (enabled by default for dev)
    #[arg(long)]
    accept_invalid_certs: bool,
}

fn main() -> iced::Result {
    let instance_id = generate_instance_id();

    // Log to a persistent file; fall back to stderr if the path can't be opened.
    match kmux_protocol::dirs::client_log_path().and_then(|p| {
        Ok(std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)?)
    }) {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive("kmux_gui=info".parse().unwrap()),
                )
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive("kmux_gui=info".parse().unwrap()),
                )
                .init();
        }
    }
    tracing::info!(instance_id = %instance_id, "kmux-gui started");

    let _span = tracing::info_span!("instance", id = %instance_id).entered();

    let cli = Cli::parse();

    iced::application("kmux -- remote terminal", kmuxApp::update, kmuxApp::view)
        .subscription(kmuxApp::subscription)
        .theme(kmuxApp::theme)
        .window_size((1024.0, 768.0))
        .run_with(move || {
            (
                kmuxApp::new(cli.accept_invalid_certs, instance_id),
                iced::Task::none(),
            )
        })
}

fn generate_instance_id() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
