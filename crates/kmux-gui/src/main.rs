mod app;
mod session_bar;
mod shortcut;
mod status_bar;
mod terminal_view;
mod theme;

use clap::Parser;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("kmux_gui=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    iced::application("kmux -- remote terminal", kmuxApp::update, kmuxApp::view)
        .subscription(kmuxApp::subscription)
        .theme(kmuxApp::theme)
        .window_size((1024.0, 768.0))
        .run_with(move || (kmuxApp::new(cli.accept_invalid_certs), iced::Task::none()))
}
