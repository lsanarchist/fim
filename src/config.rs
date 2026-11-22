//! Configuration: `config.json` is parsed strictly and checked before any work starts.
use anyhow::{ensure, Context, Result};
use chrono_tz::Tz;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

// Deliberately no `Debug`: the struct holds the bot token, which must never be printed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub site_dir: PathBuf,
    pub database: PathBuf,
    pub log_file: PathBuf,
    pub template: PathBuf,
    pub namedays: PathBuf,
    pub timezone: Tz,
    pub interval_seconds: u64,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
}

impl Config {
    /// Reads the file, resolves relative paths against the config's directory and validates it.
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let mut c: Config = serde_json::from_str(&text).context("invalid config.json")?;
        let base = path.parent().unwrap_or(Path::new("."));
        c.site_dir = fs::canonicalize(base.join(&c.site_dir)).context("site_dir does not exist")?;
        c.template = base.join(&c.template);
        c.namedays = base.join(&c.namedays);
        c.database = absolute_file(base, &c.database)?;
        c.log_file = absolute_file(base, &c.log_file)?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.site_dir.is_dir(), "site_dir is not a directory");
        ensure!(
            (1..=86_400).contains(&self.interval_seconds),
            "interval_seconds must be between 1 and 86400"
        );
        for (name, p) in [("database", &self.database), ("log_file", &self.log_file)] {
            ensure!(
                !p.starts_with(&self.site_dir),
                "{name} must be outside site_dir"
            );
        }
        // Bot API tokens look like `123456789:AA...`. The value itself is never echoed back.
        let token_ok = self
            .telegram_bot_token
            .split_once(':')
            .is_some_and(|(id, key)| {
                !id.is_empty()
                    && id.bytes().all(|b| b.is_ascii_digit())
                    && key.len() >= 20
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            });
        ensure!(token_ok, "telegram_bot_token has an invalid format");
        let chat = self
            .telegram_chat_id
            .strip_prefix('-')
            .unwrap_or(&self.telegram_chat_id);
        ensure!(
            !chat.is_empty() && chat.bytes().all(|b| b.is_ascii_digit()),
            "telegram_chat_id must be a numeric chat id"
        );
        Ok(())
    }
}

/// The file may not exist yet, but its directory must; returns an absolute path.
fn absolute_file(base: &Path, file: &Path) -> Result<PathBuf> {
    let full = base.join(file);
    let name = full.file_name().context("path has no file name")?;
    let dir = full.parent().filter(|d| !d.as_os_str().is_empty());
    let dir = fs::canonicalize(dir.unwrap_or(Path::new(".")))
        .with_context(|| format!("directory of {} does not exist", full.display()))?;
    Ok(dir.join(name))
}
