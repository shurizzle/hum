mod api;
mod auth;
mod cache;
mod cli;
mod config;
mod daemon;
mod media;
mod meta;
mod player;
mod proto;
mod setup;
mod soloist;
mod spawn;
mod tui;

use anyhow::{Context, Result};
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

fn main() -> Result<()> {
    // Only ring is compiled in; install it explicitly so a dependency enabling another backend
    // later cannot make rustls refuse to guess. Process-wide, before any TLS connection.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // Now Playing remote commands arrive on the main run loop, so the daemon runs beside it.
    #[cfg(target_os = "macos")]
    if matches!(args.cmd, Some(Sub::Daemon)) {
        std::thread::spawn(move || {
            let code = match rt.block_on(run(args)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Error: {e:?}");
                    1
                }
            };
            // `exit` skips drops: shutting the runtime down drops the tasks, so `kill_on_drop` reaps the player child.
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
            std::process::exit(code);
        });
        media::main_loop();
    }
    rt.block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    match args.cmd {
        None => {
            // Before login, so a bad config fails at once.
            let keymap = tui::keys::Keymap::new(&config::load()?.keymap).context("config.ron")?;
            auth::ensure_login().await?;
            tui::run(spawn::connect().await?, keymap).await
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
