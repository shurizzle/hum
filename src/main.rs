mod api;
mod auth;
mod cache;
mod cli;
mod daemon;
mod meta;
mod player;
mod proto;
mod setup;
mod soloist;
mod spawn;
mod tui;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

/// Terminal Spotify client. Without a subcommand, opens the TUI.
#[derive(Parser)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Sub>,
}

#[derive(Subcommand)]
enum Sub {
    /// Start the daemon in the background, if not running
    Start,
    /// Stop the daemon
    Kill,
    /// Print a shell completion script (e.g. `hum completions zsh > _hum`)
    Completions { shell: clap_complete::Shell },
    /// Run the daemon in the foreground (spawned automatically by clients)
    #[command(hide = true)]
    Daemon,
    #[command(flatten)]
    Remote(cli::Remote),
}

#[tokio::main]
async fn main() -> Result<()> {
    // Only ring is compiled in; install it explicitly so a dependency enabling another backend
    // later cannot make rustls refuse to guess. Process-wide, before any TLS connection.
    let _ = rustls::crypto::ring::default_provider().install_default();
    match Args::parse().cmd {
        None => {
            auth::ensure_login().await?;
            tui::run(spawn::connect().await?).await
        }
        Some(Sub::Start) => {
            auth::ensure_login().await?;
            spawn::connect().await.map(drop)
        }
        Some(Sub::Kill) => spawn::kill().await,
        Some(Sub::Completions { shell }) => {
            clap_complete::generate(shell, &mut Args::command(), "hum", &mut std::io::stdout());
            Ok(())
        }
        Some(Sub::Daemon) => {
            daemon::init_logger();
            daemon::run().await
        }
        Some(Sub::Remote(remote)) => cli::run(remote).await,
    }
}
