use std::{fs::File, os::unix::process::CommandExt, process::Command, time::Duration};

use anyhow::{Context, Result};
use tokio::net::UnixStream;

use crate::{
    auth,
    proto::{self, Cmd, Msg},
};

/// Connects to the daemon, spawning it detached if it is not running.
/// A daemon older than this client is stopped and replaced.
pub async fn connect() -> Result<proto::Conn> {
    if let Some(conn) = try_connect().await? {
        return Ok(conn);
    }
    // Serializes concurrent clients so only one of them spawns the daemon.
    let lock = File::create(proto::runtime_path("hum.lock"))?;
    lock.lock()?;
    if let Some(conn) = try_connect().await? {
        return Ok(conn);
    }
    // First start, or a new pinned Electron: install it while still in the terminal.
    crate::setup::ensure_ecs().await?;
    let dir = auth::state_dir();
    std::fs::create_dir_all(&dir)?;
    let log = File::create(dir.join("daemon.log"))?;
    Command::new(std::env::current_exe()?)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("spawn daemon")?;
    // Spirc startup includes a full Spotify login, so allow it some time.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(conn) = try_connect().await? {
            return Ok(conn);
        }
    }
    anyhow::bail!("daemon did not start, see {}", dir.join("daemon.log").display())
}

/// `None` if no daemon is listening, or if the one listening was older and got stopped.
async fn try_connect() -> Result<Option<proto::Conn>> {
    let Ok(stream) = UnixStream::connect(proto::socket_path()).await else { return Ok(None) };
    let mut conn = proto::frame(stream);
    // A daemon predating the handshake fails to parse here and counts as older.
    let version = match proto::recv::<Msg>(&mut conn).await {
        Ok(Some(Msg::Hello { version })) if !proto::older(&version, proto::VERSION) => return Ok(Some(conn)),
        Ok(Some(Msg::Hello { version })) => version,
        _ => "unknown".into(),
    };
    eprintln!("hum: restarting daemon {version} -> {}", proto::VERSION);
    stop(conn).await?;
    Ok(None)
}

/// `hum kill`: stops the daemon, whatever its version.
pub async fn kill() -> Result<()> {
    match UnixStream::connect(proto::socket_path()).await {
        Ok(stream) => stop(proto::frame(stream)).await,
        Err(_) => {
            // No socket, but a hung daemon may still own the pid file.
            kill_pid();
            println!("hum: daemon not running");
            Ok(())
        }
    }
}

/// Asks the daemon to shut down; kills it by pid if it does not exit within 5 s.
async fn stop(mut conn: proto::Conn) -> Result<()> {
    let _ = proto::send(&mut conn, &Cmd::Shutdown).await;
    drop(conn);
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if UnixStream::connect(proto::socket_path()).await.is_err() {
            return Ok(());
        }
    }
    kill_pid();
    let _ = std::fs::remove_file(proto::socket_path());
    Ok(())
}

fn kill_pid() {
    let pid_path = proto::runtime_path("hum.pid");
    if let Ok(pid) = std::fs::read_to_string(&pid_path) {
        let _ = Command::new("kill").args(["-9", pid.trim()]).status();
        let _ = std::fs::remove_file(pid_path);
    }
}
