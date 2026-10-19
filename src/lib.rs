#![cfg_attr(not(target_os = "linux"), allow(unused))]
#[cfg(not(target_os = "linux"))]
compile_error!("fim requires Linux >= 5.6 (openat2); use a Linux VM, not native Windows.");

pub mod alerts;
pub mod baseline;
pub mod config;
pub mod daily;
pub mod journal;
pub mod monitor;
pub mod publisher;
pub mod scan;
pub mod secure;

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

pub type Issues = BTreeMap<String, Issue>;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub kind: String,
    // Raw pathname bytes encoded as hex: no collisions for non-UTF-8 names.
    pub path_hex: Option<String>,
    pub path: Option<String>,
    pub detail: String,
}
impl Issue {
    pub fn new(kind: &str, path: Option<&Path>, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            path_hex: path.map(secure::path_key),
            path: path.map(|p| format!("{p:?}")),
            detail: detail.into(),
        }
    }
    pub fn key(&self) -> String {
        format!(
            "{}:{}",
            self.kind,
            self.path_hex.as_deref().unwrap_or("@health")
        )
    }
}
pub fn add(issues: &mut Issues, issue: Issue) {
    issues.insert(issue.key(), issue);
}
