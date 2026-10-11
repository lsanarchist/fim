use anyhow::{ensure, Context, Result};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::{fs, path::{Component, Path, PathBuf}};
use crate::{daily::{self, Names}, secure};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub root: PathBuf, pub database: PathBuf, pub calendar: PathBuf, pub template: PathBuf,
    pub state_dir: PathBuf, pub staging_dir: PathBuf, pub timezone: String,
    pub interval_seconds: u64, pub midnight_grace_seconds: u32,
    pub sendmail: PathBuf, pub mail_from: String, pub mail_to: String,
    pub alert_timeout_seconds: u64, pub retry_seconds: u64, pub reminder_seconds: u64,
    pub max_file_bytes: u64, pub max_index_bytes: u64, pub max_entries: usize,
    pub scan_budget_seconds: u64, pub log_max_bytes: u64,
}
#[derive(Clone)]
pub struct Guard { pub path: PathBuf, pub digest: String, pub limit: u64 }
impl Guard {
    pub fn capture(path: &Path, limit: u64, demo: bool) -> Result<(Self, Vec<u8>)> {
        secure::trusted(path, demo)?;
        let bytes = secure::read_small(path, limit)?;
        Ok((Self { path: path.into(), digest: secure::hash(&bytes), limit }, bytes))
    }
    pub fn verify(&self, demo: bool) -> Result<()> {
        secure::trusted(&self.path, demo)?;
        ensure!(secure::hash(&secure::read_small(&self.path, self.limit)?) == self.digest,
            "Trusted input changed: {:?}; pinned in-memory data are still in use", self.path);
        Ok(())
    }
}
#[derive(Clone)]
pub struct ContextData {
    pub c: Config, pub tz: Tz, pub names: Names, pub template: String,
    pub guards: Vec<Guard>, pub demo: bool,
}
fn clean(path: &Path) -> Result<PathBuf> {
    let full = if path.is_absolute() { path.into() } else { std::env::current_dir()?.join(path) };
    let mut out = PathBuf::new();
    for part in full.components() {
        match part { Component::CurDir => {}, Component::ParentDir => { out.pop(); }, _ => out.push(part.as_os_str()) }
    }
    Ok(out)
}
impl ContextData {
    pub fn load(path: &Path, demo: bool) -> Result<Self> {
        let path = clean(path)?;
        let (guard, bytes) = Guard::capture(&path, 1024*1024, demo)?;
        let mut c: Config = serde_json::from_slice(&bytes)?;
        let base = path.parent().context("Config has no parent")?;
        for p in [&mut c.root, &mut c.database, &mut c.calendar, &mut c.template,
                  &mut c.state_dir, &mut c.staging_dir, &mut c.sendmail] {
            *p = clean(&base.join(&*p))?;
        }
        ensure!((1..=86400).contains(&c.interval_seconds), "Invalid interval");
        ensure!(c.midnight_grace_seconds <= 300, "Grace must be <= 300 seconds");
        ensure!((1..=60).contains(&c.alert_timeout_seconds), "Mail timeout must be 1..60 seconds");
        ensure!((1..=3600).contains(&c.retry_seconds), "Retry must be 1..3600 seconds");
        ensure!(c.reminder_seconds >= 60, "Reminder must be >= 60 seconds");
        ensure!((1..=1_000_000).contains(&c.max_entries), "Invalid entry limit");
        ensure!((1..=300).contains(&c.scan_budget_seconds), "Scan budget must be 1..300 seconds");
        ensure!((1024..=16*1024*1024).contains(&c.max_index_bytes), "Invalid index size limit");
        ensure!((c.max_index_bytes..=1_099_511_627_776).contains(&c.max_file_bytes), "Invalid file limit");
        ensure!(c.log_max_bytes >= 65536, "Log rotation size is too small");
        // sendmail is often a root-controlled alternatives symlink; execute the resolved target.
        if c.sendmail.try_exists()? { c.sendmail = fs::canonicalize(&c.sendmail)?; }
        let tz: Tz = c.timezone.parse().context("Unknown IANA timezone")?;
        let _: lettre::message::Mailbox = c.mail_from.parse().context("Invalid sender")?;
        let _: lettre::message::Mailbox = c.mail_to.parse().context("Invalid recipient")?;
        ensure!(c.root.parent().is_some(), "Filesystem root is not supported");
        secure::private_dir(&c.state_dir)?;
        secure::private_dir(&c.staging_dir)?;
        for p in [&c.root, &c.state_dir, &c.staging_dir] {
            secure::trusted(p.parent().context("No parent")?, demo)?;
        }
        // The public tree and all mutable/private storage must be disjoint.
        let paths = [&path, &c.database, &c.calendar, &c.template, &c.state_dir, &c.staging_dir];
        for p in &paths {
            ensure!(!p.starts_with(&c.root) && !c.root.starts_with(p), "Private path overlaps website: {p:?}");
        }
        for (i, a) in paths.iter().enumerate() {
            for b in paths.iter().skip(i+1) {
                ensure!(!a.starts_with(b) && !b.starts_with(a), "Private paths overlap: {a:?}, {b:?}");
            }
        }
        secure::trusted(c.database.parent().context("Baseline has no parent")?, demo)?;
        let (cal_guard, cal) = Guard::capture(&c.calendar, 1024*1024, demo)?;
        let names = serde_json::from_slice(&cal)?;
        daily::validate_calendar(&names)?;
        let (tpl_guard, tpl) = Guard::capture(&c.template, c.max_index_bytes, demo)?;
        let template = String::from_utf8(tpl)?;
        ensure!(template.matches(daily::TOKEN).count() == 1, "Template needs one DAILY token");
        let day = chrono::Utc::now().with_timezone(&tz).date_naive();
        ensure!(daily::normalize_dates(&template.replace(daily::TOKEN, &daily::value(&names, day)?),
            &names, &[day])? == template, "Invalid template markers");
        let mut guards = vec![guard, cal_guard, tpl_guard];
        if !demo {
            let exe = fs::canonicalize(std::env::current_exe()?)?;
            guards.push(Guard::capture(&exe, 256*1024*1024, false)?.0);
        }
        Ok(Self { c, tz, names, template, guards, demo })
    }
}
