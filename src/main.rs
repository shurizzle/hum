mod api;
mod auth;
mod cache;
mod daemon;
mod meta;
mod player;
mod proto;
mod spawn;
mod tui;

use anyhow::{Result, bail};

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => {
            auth::ensure_login().await?;
            tui::run(spawn::connect().await?).await
        }
        Some("kill") => spawn::kill().await,
        Some("daemon") => {
            daemon::init_logger();
            daemon::run().await
        }
        Some(_) => bail!("usage: hum [kill | daemon]"),
    }
}
