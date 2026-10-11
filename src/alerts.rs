use crate::{
    config::{ContextData, MailContext},
    journal::{self, Logger},
    secure,
};
use anyhow::{bail, ensure, Context, Result};
use chrono::Utc;
use lettre::{message::header::ContentType, Message};
use rusqlite::{params, OptionalExtension};
use std::{
    io::{Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Sender {
    unreaped: Option<Child>,
}
impl Sender {
    pub fn send(&mut self, ctx: &ContextData, body: &str) -> Result<()> {
        self.send_mail(&MailContext::from_context(ctx), body)
    }
    pub fn send_mail(&mut self, ctx: &MailContext, body: &str) -> Result<()> {
        if let Some(child) = self.unreaped.as_mut() {
            ensure!(
                child.try_wait()?.is_some(),
                "Previously killed MTA is still uninterruptible; refusing to spawn more children"
            );
            self.unreaped = None;
        }
        let c = &ctx.c;
        ensure!(
            (1..=60).contains(&c.alert_timeout_seconds),
            "Mail timeout must be 1..60 seconds"
        );
        secure::trusted(&c.sendmail, ctx.demo)?;
        ensure!(c.sendmail.is_absolute(), "sendmail must be absolute");
        let email = Message::builder()
            .from(c.mail_from.parse()?)
            .to(c.mail_to.parse()?)
            .subject("FIM notification")
            .header(ContentType::TEXT_PLAIN)
            .body(format!(
                "Root: {:?}\nTime: {}\n\n{}",
                c.root,
                Utc::now().to_rfc3339(),
                body
            ))?;
        // A regular, unlinked temporary file avoids a blocking write to the child's stdin pipe.
        let mut input = tempfile::tempfile_in(&c.state_dir)?;
        input.write_all(&email.formatted())?;
        input.seek(SeekFrom::Start(0))?;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(c.alert_timeout_seconds))
            .context("Mail timeout exceeds the monotonic clock range")?;
        let mut child = Command::new(&c.sendmail)
            .args(["-i", "-t"])
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/bin")
            .env("LANG", "C")
            .stdin(Stdio::from(input))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .context("Could not launch sendmail")?;
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "sendmail exited with {status}");
                return Ok(());
            }
            if Instant::now() >= deadline {
                // SAFETY: the child was placed in a new group with PGID == its PID.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let cleanup = Instant::now() + Duration::from_secs(1);
                while Instant::now() < cleanup {
                    if child.try_wait()?.is_some() {
                        bail!("sendmail timed out; process group killed");
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                self.unreaped = Some(child);
                bail!("sendmail timed out and is not yet reapable; scanner remains independent");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
/// One due batch; retries survive restarts. Crash after SMTP acceptance can cause a duplicate (at-least-once).
pub fn deliver_one(ctx: &ContextData, logger: &Logger, sender: &mut Sender) -> Result<bool> {
    deliver_mail(&MailContext::from_context(ctx), logger, sender)
}
pub fn deliver_mail(ctx: &MailContext, logger: &Logger, sender: &mut Sender) -> Result<bool> {
    let Some(_lock) = secure::try_lock(&ctx.c.state_dir.join("delivery.lock"))? else {
        return Ok(false);
    };
    let db = journal::open_db(&ctx.c.state_dir)?;
    let now = Utc::now().timestamp();
    let row:Option<(String,String,i64)>=db.query_row(
        "SELECT id,body,attempts FROM outbox WHERE submitted_at IS NULL AND next_attempt<=?1 ORDER BY rowid LIMIT 1",
        [now],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((id, body, attempts)) = row else {
        return Ok(false);
    };
    match sender.send_mail(ctx, &body) {
        Ok(()) => {
            db.execute(
                "UPDATE outbox SET submitted_at=?1,last_error='' WHERE id=?2",
                params![Utc::now().timestamp(), id],
            )?;
            logger.status(
                "ALERT_QUEUED",
                format!("{id}: accepted by local MTA, not proof of inbox delivery"),
            );
        }
        Err(e) => {
            let wait = (ctx
                .c
                .retry_seconds
                .saturating_mul(1u64 << attempts.clamp(0, 10) as u32))
            .min(3600);
            db.execute(
                "UPDATE outbox SET attempts=attempts+1,next_attempt=?1,last_error=?2 WHERE id=?3",
                params![Utc::now().timestamp() + wait as i64, format!("{e:#}"), id],
            )?;
            logger.status("ALERT_FAILED", format!("{id}: {e:#}; retry in {wait}s"));
            return Err(e);
        }
    }
    Ok(true)
}
pub type MailHealth = Arc<Mutex<Option<String>>>;
pub fn worker(
    ctx: ContextData,
    logger: Logger,
    stop: Arc<AtomicBool>,
    health: MailHealth,
) -> thread::JoinHandle<()> {
    let ctx = MailContext::from_context(&ctx);
    thread::spawn(move || {
        let mut sender = Sender::default();
        // Never allow a wall-clock rollback before restart to defer retries arbitrarily far.
        if let Ok(db) = journal::open_db(&ctx.c.state_dir) {
            let _ = db.execute(
                "UPDATE outbox SET next_attempt=MIN(next_attempt,?1) WHERE submitted_at IS NULL",
                [Utc::now().timestamp() + ctx.c.retry_seconds as i64],
            );
        }
        while !stop.load(Ordering::Relaxed) {
            let result = deliver_mail(&ctx, &logger, &mut sender);
            if let Ok(mut h) = health.lock() {
                match result {
                    Ok(true) => *h = None,
                    Ok(false) => {}
                    Err(ref e) => *h = Some(format!("{e:#}")),
                }
            }
            for _ in 0..5 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    })
}
