//! Plain-text log: one line per event, `<time> <EVENT> <message>`.
use chrono::{SecondsFormat, Utc};
use chrono_tz::Tz;
use std::{fs::OpenOptions, io::Write, path::PathBuf};

pub struct Log {
    pub path: PathBuf,
    pub tz: Tz,
}

impl Log {
    pub fn now(&self) -> String {
        Utc::now()
            .with_timezone(&self.tz)
            .to_rfc3339_opts(SecondsFormat::Secs, false)
    }

    /// Appends a line and also prints it (systemd puts stdout into the journal).
    /// A broken log file is reported on stderr but does not stop monitoring.
    pub fn write(&self, event: &str, message: &str) {
        let line = format!("{} {event} {message}", self.now());
        println!("{line}");
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = result {
            eprintln!("fim: cannot write log {}: {e}", self.path.display());
        }
    }
}
