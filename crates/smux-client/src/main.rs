mod app;
mod connect;
mod metrics;
mod session_bar;
mod terminal_view;
mod theme;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use app::SmuxApp;

#[derive(Parser, Debug)]
#[command(name = "smux-client", about = "smux remote terminal client")]
struct Cli {
    /// Accept self-signed / invalid TLS certificates (enabled by default for dev)
    #[arg(long)]
    accept_invalid_certs: bool,
}

fn main() -> iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("smux_client=info".parse().unwrap()),
        )
        .init();

    let _cli = Cli::parse();

    iced::application("smux -- remote terminal", SmuxApp::update, SmuxApp::view)
        .subscription(SmuxApp::subscription)
        .theme(SmuxApp::theme)
        .window_size((1024.0, 768.0))
        .run_with(|| (SmuxApp::new(), iced::Task::none()))
}
