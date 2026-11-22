//! Telegram notifications. Every alert first goes to the `outbox` table, then
//! `flush` tries to send everything that is waiting. A failed send stays in the
//! table and is retried in the next cycle; monitoring itself never stops.
use crate::{config::Config, db, log::Log};
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::time::Duration;

/// Sends one text message. Tests replace it with a fake.
pub type Send<'a> = dyn FnMut(&str) -> Result<()> + 'a;

pub fn telegram(cfg: &Config) -> impl FnMut(&str) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        cfg.telegram_bot_token
    );
    let chat_id = cfg.telegram_chat_id.clone();
    move |text| {
        let body = serde_json::json!({ "chat_id": chat_id, "text": text });
        // Errors are rebuilt by hand: ureq's own messages contain the URL, i.e. the token.
        match agent.post(&url).send_json(body) {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, response)) => {
                let reply: serde_json::Value = response.into_json().unwrap_or_default();
                let why = reply["description"].as_str().unwrap_or("no description");
                bail!("Telegram answered HTTP {code}: {why}")
            }
            Err(ureq::Error::Transport(t)) => bail!("Telegram unreachable: {}", t.kind()),
        }
    }
}

/// Sends waiting alerts in order and stops at the first failure.
pub fn flush(db: &Connection, log: &Log, send: &mut Send) -> Result<()> {
    let pending = db::pending_alerts(db)?;
    let waiting = pending.len();
    for (id, text) in pending {
        // "#7 FIM: INCIDENT MODIFIED style.css" - title, type and file of the message.
        let lines: Vec<&str> = text.lines().take(3).collect();
        let summary = format!("#{id} {}", lines.join(" "));
        match send(&text) {
            Ok(()) => {
                db::alert_sent(db, id)?;
                log.write("ALERT_SENT", &summary);
            }
            Err(e) => {
                db::alert_failed(db, id, &format!("{e:#}"))?;
                let why = format!("{summary}: {e:#} ({waiting} waiting, will retry)");
                log.write("ALERT_FAILED", &why);
                break;
            }
        }
    }
    Ok(())
}
