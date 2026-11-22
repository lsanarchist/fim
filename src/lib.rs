//! File Integrity Monitoring for a static website.
pub mod alert;
pub mod config;
pub mod daily;
pub mod db;
pub mod log;
pub mod monitor;
pub mod page;
pub mod scan;

use anyhow::Result;
use std::path::Path;

/// Everything a command needs, loaded once at start.
pub struct App {
    pub cfg: config::Config,
    pub names: daily::Names,
    pub log: log::Log,
}

impl App {
    pub fn load(config_path: &Path) -> Result<Self> {
        let cfg = config::Config::load(config_path)?;
        let names = daily::load_names(&cfg.namedays)?;
        let log = log::Log {
            path: cfg.log_file.clone(),
            tz: cfg.timezone,
        };
        Ok(Self { cfg, names, log })
    }
}

/// One finding: e.g. kind "MODIFIED", path "style.css", detail "content (SHA-256), size 10 -> 12".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub kind: String,
    pub path: String,
    pub detail: String,
}

impl Issue {
    pub fn new(kind: &str, path: &str, detail: String) -> Self {
        Self {
            kind: kind.into(),
            path: path.into(),
            detail,
        }
    }
    /// Identifies an incident across cycles, so the same one is alerted only once.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind, self.path)
    }
}
