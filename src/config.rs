//! User configuration: `config.ron` in `auth::config_dir()`, over `config.default.ron`.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::{auth, tui::keys::Keys};

pub const DEFAULT: &str = include_str!("config.default.ron");

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    // Field-level, not container-level, defaults: these read `defaults()`, and a container default
    // would be built eagerly while `defaults()` itself is being parsed.
    #[serde(default)]
    pub keymap: Keys,
}

/// `config.default.ron`; it sets every field, so parsing it never falls back on `defaults()`.
pub fn defaults() -> &'static Config {
    static DEFAULTS: std::sync::LazyLock<Config> = std::sync::LazyLock::new(|| parse(DEFAULT).expect("config.default.ron"));
    &DEFAULTS
}

/// The user's config; defaults when the file is missing.
pub fn load() -> Result<Config> {
    let path = auth::config_dir().join("config.ron");
    match std::fs::read_to_string(&path) {
        Ok(s) => parse(&s).with_context(|| format!("{}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("{}", path.display())),
    }
}

pub fn parse(s: &str) -> Result<Config> {
    // `timeout: 500` instead of `timeout: Some(500)`.
    let opts = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
    Ok(opts.from_str(s)?)
}
