//! Installs the pinned castLabs Electron (the playback engine) before the first daemon start:
//! the VMP-signed build from this repository's `ecs-<version>` release on macOS, castLabs' own
//! build on Linux (no signing needed there).

use std::{io::Write, path::Path, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;

use crate::{auth, player, soloist};

/// The playback engine the daemon runs. See docs/playback.md.
pub enum Engine {
    /// Spotify Web Playback SDK in castLabs Electron (Widevine).
    Ecs,
    /// Spotify Soloist, paired and driven over its WebSocket API.
    Soloist { bin: PathBuf, key: String },
}

/// The engine already set up, without installing or pairing anything (used by the daemon). An
/// existing Soloist pairing wins; otherwise an existing ECS install.
pub fn engine() -> Option<Engine> {
    if let (Some(key), Some(bin)) = (soloist::key(), soloist::binary()) {
        return Some(Engine::Soloist { bin, key });
    }
    (std::env::var_os("HUM_ECS").is_some() || player::ecs_binary().exists()).then_some(Engine::Ecs)
}

/// Picks and, when nothing is set up yet, installs the playback engine (foreground, before the
/// daemon starts): a prior setup wins; else an installed Soloist is paired; else ECS is downloaded.
pub async fn ensure_engine() -> Result<Engine> {
    if let (Some(key), Some(bin)) = (soloist::key(), soloist::binary()) {
        return Ok(Engine::Soloist { bin, key });
    }
    if std::env::var_os("HUM_ECS").is_some() || player::ecs_binary().exists() {
        ensure_ecs().await?;
        return Ok(Engine::Ecs);
    }
    if let Some(bin) = soloist::binary() {
        soloist::setup(&bin).await?;
        let key = soloist::key().context("Soloist pairing did not store a key")?;
        return Ok(Engine::Soloist { bin, key });
    }
    ensure_ecs().await?;
    Ok(Engine::Ecs)
}

/// Ensures `<data dir>/ecs` holds `player::ECS_VERSION`. Skipped with `$HUM_ECS` (dev override) and
/// for an install without a `VERSION` file (manual or dev install, left alone).
pub async fn ensure_ecs() -> Result<()> {
    if std::env::var_os("HUM_ECS").is_some() {
        return Ok(());
    }
    let dir = auth::data_dir().join("ecs");
    match std::fs::read_to_string(dir.join("VERSION")) {
        Ok(v) if v.trim() == player::ECS_VERSION => return Ok(()),
        Err(_) if player::ecs_binary().exists() => return Ok(()),
        _ => {}
    }
    install(&dir).await
}

/// Download URL of the Electron build for this OS and CPU.
fn url(os: &str, arch: &str, version: &str) -> Result<String> {
    Ok(match (os, arch) {
        ("macos", "aarch64" | "x86_64") => {
            format!("https://github.com/shurizzle/hum/releases/download/ecs-{version}/hum-ecs-macos-{arch}.zip")
        }
        ("linux", "x86_64" | "aarch64") => {
            let arch = if arch == "x86_64" { "x64" } else { "arm64" };
            format!("https://github.com/castlabs/electron-releases/releases/download/{version}/electron-{version}-linux-{arch}.zip")
        }
        _ => bail!("no Electron build for {os}/{arch}"),
    })
}

async fn install(dir: &Path) -> Result<()> {
    let version = player::ECS_VERSION;
    let url = url(std::env::consts::OS, std::env::consts::ARCH, version)?.replace('+', "%2B");
    let data = auth::data_dir();
    std::fs::create_dir_all(&data)?;
    let (zip, staging) = (data.join("ecs.zip.part"), data.join("ecs.new"));
    // Leftovers of an interrupted install.
    let _ = std::fs::remove_dir_all(&staging);

    let res = reqwest::get(&url).await?.error_for_status().with_context(|| format!("download {url}"))?;
    let total = res.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(&zip)?;
    let (mut done, mut shown) = (0u64, u64::MAX);
    let mut body = res.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("download interrupted")?;
        file.write_all(&chunk)?;
        done += chunk.len() as u64;
        let pct = (done * 100).checked_div(total).unwrap_or(0);
        if pct != shown {
            shown = pct;
            eprint!("\rhum: downloading Electron {version} ({} MB): {pct}%", total / 1_000_000);
        }
    }
    eprintln!();
    drop(file);

    std::fs::create_dir_all(&staging)?;
    // ditto keeps the framework symlinks of the macOS app bundle.
    let mut extract = if cfg!(target_os = "macos") {
        let mut c = Command::new("ditto");
        c.arg("-x").arg("-k").arg(&zip).arg(&staging);
        c
    } else {
        let mut c = Command::new("unzip");
        c.arg("-q").arg(&zip).arg("-d").arg(&staging);
        c
    };
    let status = extract.status().context("extract Electron (is `unzip` installed?)")?;
    if !status.success() {
        bail!("extract Electron: {status}");
    }
    std::fs::write(staging.join("VERSION"), version)?;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&staging, dir)?;
    let _ = std::fs::remove_file(&zip);
    eprintln!("hum: Electron {version} installed in {}", dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_per_platform() {
        let v = "v45.2.1+wvcus";
        assert_eq!(url("macos", "aarch64", v).unwrap(), "https://github.com/shurizzle/hum/releases/download/ecs-v45.2.1+wvcus/hum-ecs-macos-aarch64.zip");
        assert!(url("macos", "x86_64", v).unwrap().ends_with("/hum-ecs-macos-x86_64.zip"));
        assert!(url("linux", "x86_64", v).unwrap().ends_with("/v45.2.1+wvcus/electron-v45.2.1+wvcus-linux-x64.zip"));
        assert!(url("windows", "x86_64", v).is_err());
    }
}
