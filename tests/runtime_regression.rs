use fim::{alerts, baseline, config::ContextData, daily, journal, monitor, publisher};
use std::{
    fs,
    os::fd::AsRawFd,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    _temp: tempfile::TempDir,
    ctx: ContextData,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let p = temp.path();
        for (name, mode) in [("site", 0o755), ("state", 0o700), ("staging", 0o700)] {
            fs::create_dir(p.join(name)).unwrap();
            fs::set_permissions(p.join(name), fs::Permissions::from_mode(mode)).unwrap();
        }
        fs::write(p.join("namedays.json"), include_bytes!("../namedays.json")).unwrap();
        fs::write(
            p.join("index.template.html"),
            include_bytes!("../index.template.html"),
        )
        .unwrap();
        let mta = p.join("fake-mta");
        fs::write(&mta, "#!/bin/sh\ncat >/dev/null\n").unwrap();
        fs::set_permissions(&mta, fs::Permissions::from_mode(0o700)).unwrap();
        let mut c: serde_json::Value =
            serde_json::from_str(include_str!("../config.json")).unwrap();
        c["sendmail"] = serde_json::json!(mta);
        fs::write(p.join("config.json"), serde_json::to_vec(&c).unwrap()).unwrap();
        let ctx = ContextData::load(&p.join("config.json"), true).unwrap();
        publisher::page(&ctx).unwrap();
        baseline::create(&ctx).unwrap();
        Self { _temp: temp, ctx }
    }

    fn command(&self, action: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fim"));
        command
            .arg("--demo")
            .arg(self._temp.path().join("config.json"))
            .arg(action);
        command
    }
}

struct MailWorker {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl MailWorker {
    fn start(ctx: &ContextData, health: &alerts::MailHealth) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let join = alerts::worker(
            ctx.clone(),
            journal::Logger::new(&ctx.c),
            stop.clone(),
            health.clone(),
        );
        Self {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for MailWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            join.join().unwrap();
        }
    }
}

#[test]
fn mail_health_recovers_when_another_sender_already_drained_the_queue() {
    let f = Fixture::new();
    journal::external(&f.ctx.c, "TEST", "pending notification".into(), true).unwrap();
    assert!(alerts::deliver_one(
        &f.ctx,
        &journal::Logger::new(&f.ctx.c),
        &mut alerts::Sender::default(),
    )
    .unwrap());
    assert_eq!(journal::queue_size(&f.ctx.c.state_dir).unwrap(), 0);
    let health: alerts::MailHealth = Arc::new(Mutex::new(Some("previous MTA failure".into())));
    let _worker = MailWorker::start(&f.ctx, &health);
    let deadline = Instant::now() + Duration::from_secs(3);
    while health.lock().unwrap().is_some() {
        assert!(
            Instant::now() < deadline,
            "restored mail failure never recovered"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn idle_mail_worker_preserves_failure_while_a_batch_is_in_backoff() {
    let f = Fixture::new();
    journal::external(&f.ctx.c, "TEST", "pending notification".into(), true).unwrap();
    let db = journal::open_db(&f.ctx.c.state_dir).unwrap();
    let initial_retry = chrono::Utc::now().timestamp() + 3600;
    db.execute("UPDATE outbox SET next_attempt=?1", [initial_retry])
        .unwrap();
    let health: alerts::MailHealth = Arc::new(Mutex::new(Some("previous MTA failure".into())));
    let worker = MailWorker::start(&f.ctx, &health);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let retry: i64 = db
            .query_row("SELECT next_attempt FROM outbox", [], |r| r.get(0))
            .unwrap();
        if retry < initial_retry {
            break; // The worker has opened the database and applied its retry clamp.
        }
        assert!(Instant::now() < deadline, "mail worker never started");
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(150));
    drop(worker);
    assert_eq!(
        health.lock().unwrap().as_deref(),
        Some("previous MTA failure")
    );
    assert_eq!(journal::queue_size(&f.ctx.c.state_dir).unwrap(), 1);
}

#[test]
fn actual_inotify_exchange_is_authorized_without_muting_unrelated_writes() {
    let f = Fixture::new();
    let today = chrono::Utc::now().with_timezone(&f.ctx.tz).date_naive();
    let yesterday = f.ctx.template.replace(
        daily::TOKEN,
        &daily::value(&f.ctx.names, today.pred_opt().unwrap()).unwrap(),
    );
    fs::write(f.ctx.c.root.join("index.html"), yesterday).unwrap();
    let watch = monitor::EventWatch::new(&f.ctx).unwrap();
    // Let events from fixture setup settle before starting the measured
    // publication; no production mutation is discarded during this phase.
    let startup_deadline = Instant::now() + Duration::from_secs(1);
    while watch.rx.recv_timeout(Duration::from_millis(100)).is_ok() {
        assert!(
            Instant::now() < startup_deadline,
            "setup event stream did not settle"
        );
    }
    publisher::page(&f.ctx).unwrap();
    fs::write(f.ctx.c.root.join("unrelated.txt"), "changed").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        match watch.rx.recv_timeout(Duration::from_millis(250)) {
            Ok(event) => events.push(event.unwrap()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !events.is_empty() => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("watcher disconnected: {e}"),
        }
    }
    assert!(!watch.lost.load(Ordering::Relaxed));
    let authorized = publisher::authorized_trackers(&f.ctx, &events);
    assert_eq!(authorized.len(), 2, "exchange cookies missing: {events:?}");
    let (ordinary, expected) = monitor::classify(&events, &authorized);
    assert!(!expected.is_empty(), "publication was not recognized");
    assert!(
        ordinary
            .iter()
            .all(|detail| detail.contains("unrelated.txt")),
        "legitimate publication generated an unexpected alarm: {ordinary:?}"
    );
    assert!(!ordinary.is_empty(), "unrelated mutation was suppressed");
}

fn log_has_issue(f: &Fixture, event_kind: &str, issue_kind: &str) -> bool {
    fs::read_to_string(f.ctx.c.state_dir.join("fim.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<journal::Audit>(line).ok())
        .any(|event| {
            event.kind == event_kind
                && serde_json::from_str::<fim::Issue>(&event.detail)
                    .is_ok_and(|issue| issue.kind == issue_kind)
        })
}

fn corrupt_audit(f: &Fixture) -> rusqlite::Connection {
    let db = journal::open_db(&f.ctx.c.state_dir).unwrap();
    db.execute(
        "INSERT INTO audit(id,json,logged) VALUES('damaged-record','invalid JSON',0)",
        [],
    )
    .unwrap();
    db
}

#[test]
fn check_reports_corrupt_replay_without_modifying_the_original_record() {
    let f = Fixture::new();
    let db = corrupt_audit(&f);
    let output = f.command("check").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(log_has_issue(&f, "INCIDENT", "AUDIT_REPLAY_FAILED"));
    let row: (String, i64) = db
        .query_row(
            "SELECT json,logged FROM audit WHERE id='damaged-record'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, ("invalid JSON".into(), 0));
}

#[test]
fn check_recovers_stale_mail_incident_after_standalone_delivery() {
    let f = Fixture::new();
    let mut previous = journal::Journal::new(&f.ctx.c);
    let mut issues = fim::Issues::new();
    fim::add(
        &mut issues,
        fim::Issue::new("MAIL_UNAVAILABLE", None, "previous MTA failure"),
    );
    previous.reconcile(issues);
    previous.flush().unwrap();
    assert!(
        alerts::deliver_one(&f.ctx, &previous.logger, &mut alerts::Sender::default(),).unwrap()
    );
    assert_eq!(journal::queue_size(&f.ctx.c.state_dir).unwrap(), 0);
    let output = f.command("check").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(log_has_issue(&f, "RECOVERED", "MAIL_UNAVAILABLE"));
    assert!(journal::Journal::new(&f.ctx.c).active.is_empty());
}

struct MonitorProcess(Child);

impl MonitorProcess {
    fn stop(&mut self) -> ExitStatus {
        // SAFETY: the live child belongs to this test; exercise graceful shutdown.
        assert_eq!(unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "monitor ignored SIGTERM");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for MonitorProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn watch_reports_replay_failure_and_recovers_after_record_is_restored() {
    let mut f = Fixture::new();
    f.ctx.c.interval_seconds = 1;
    fs::write(
        f._temp.path().join("config.json"),
        serde_json::to_vec(&f.ctx.c).unwrap(),
    )
    .unwrap();
    let db = corrupt_audit(&f);
    let mut child = MonitorProcess(
        f.command("watch")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !log_has_issue(&f, "INCIDENT", "AUDIT_REPLAY_FAILED") {
        assert!(child.0.try_wait().unwrap().is_none(), "monitor stopped");
        assert!(Instant::now() < deadline, "replay failure was hidden");
        thread::sleep(Duration::from_millis(20));
    }
    // Simulate an operator restoring known-good audit bytes from a backup.
    let restored = journal::Audit {
        id: "damaged-record".into(),
        time: chrono::Utc::now().to_rfc3339(),
        kind: "TEST_EVIDENCE".into(),
        detail: "restored record".into(),
        alert: false,
    };
    let updated = db
        .execute(
            "UPDATE audit SET json=?1 WHERE id='damaged-record'",
            [serde_json::to_string(&restored).unwrap()],
        )
        .unwrap();
    assert_eq!(updated, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !log_has_issue(&f, "RECOVERED", "AUDIT_REPLAY_FAILED") {
        assert!(child.0.try_wait().unwrap().is_none(), "monitor stopped");
        assert!(
            Instant::now() < deadline,
            "replay recovery was not reported; row: {:?}; log: {}",
            db.query_row(
                "SELECT json,logged FROM audit WHERE id='damaged-record'",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            )
            .unwrap(),
            fs::read_to_string(f.ctx.c.state_dir.join("fim.log")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(20));
    }
    let logged: i64 = db
        .query_row(
            "SELECT logged FROM audit WHERE id='damaged-record'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(logged, 1);
    assert_eq!(
        child.stop().code(),
        Some(1),
        "operational incident was forgotten on shutdown"
    );
}

#[test]
fn subprocess_sqlite_lock_probe() {
    let Some(path) = std::env::var_os("FIM_TEST_SQLITE_LOCK_PATH") else {
        return;
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let lock = libc::flock {
        l_type: libc::F_WRLCK as _,
        l_whence: libc::SEEK_SET as _,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    // SAFETY: valid descriptor and lock structure; this separate process must
    // be refused an exclusive lock while the parent's SQLite connection lives.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
    assert_eq!(
        result, -1,
        "another process acquired the active database lock"
    );
    assert!(matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EAGAIN | libc::EACCES)
    ));
}

#[test]
fn opening_another_connection_preserves_sqlite_process_locks() {
    let f = Fixture::new();
    let db = journal::open_db(&f.ctx.c.state_dir).unwrap();
    let sibling = journal::open_db(&f.ctx.c.state_dir).unwrap();
    drop(sibling);
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "subprocess_sqlite_lock_probe", "--nocapture"])
        .env(
            "FIM_TEST_SQLITE_LOCK_PATH",
            f.ctx.c.state_dir.join("state.db"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lock probe failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(db.is_autocommit(), "the original connection remains live");
}

#[test]
fn recovered_operational_incidents_clear_even_when_baseline_is_unavailable() {
    let f = Fixture::new();
    let mut previous = journal::Journal::new(&f.ctx.c);
    let mut issues = fim::Issues::new();
    for kind in ["MAIL_UNAVAILABLE", "AUDIT_REPLAY_FAILED"] {
        fim::add(&mut issues, fim::Issue::new(kind, None, "previous failure"));
    }
    fim::add(
        &mut issues,
        fim::Issue::new(
            "CONTENT_CHANGED",
            Some(std::path::Path::new("index.html")),
            "last verified incident",
        ),
    );
    previous.reconcile(issues);
    previous.flush().unwrap();
    let mut sender = alerts::Sender::default();
    while alerts::deliver_one(&f.ctx, &previous.logger, &mut sender).unwrap() {}
    fs::rename(&f.ctx.c.database, f._temp.path().join("baseline.offline")).unwrap();
    let output = f.command("check").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let active = journal::Journal::new(&f.ctx.c).active;
    assert!(active.values().any(|i| i.kind == "BASELINE_UNAVAILABLE"));
    assert!(active.values().any(|i| i.kind == "CONTENT_CHANGED"));
    for kind in ["MAIL_UNAVAILABLE", "AUDIT_REPLAY_FAILED"] {
        assert!(
            !active.values().any(|i| i.kind == kind),
            "{kind} stayed stale"
        );
        assert!(log_has_issue(&f, "RECOVERED", kind));
    }
}

#[test]
fn replay_cannot_report_success_when_log_failure_hides_later_corruption() {
    let f = Fixture::new();
    let db = journal::open_db(&f.ctx.c.state_dir).unwrap();
    let valid = journal::Audit {
        id: "first-valid-row".into(),
        time: chrono::Utc::now().to_rfc3339(),
        kind: "TEST_EVIDENCE".into(),
        detail: "first record".into(),
        alert: false,
    };
    db.execute(
        "INSERT INTO audit(id,json,logged) VALUES(?1,?2,0)",
        rusqlite::params![valid.id, serde_json::to_string(&valid).unwrap()],
    )
    .unwrap();
    let _second = corrupt_audit(&f);
    let log = f.ctx.c.state_dir.join("fim.log");
    let saved_log = f.ctx.c.state_dir.join("log.saved");
    fs::rename(&log, &saved_log).unwrap();
    fs::create_dir(&log).unwrap();
    let logger = journal::Logger::new(&f.ctx.c);
    let error = journal::replay_logs(&f.ctx.c.state_dir, &logger).unwrap_err();
    assert!(error.to_string().contains("log write failure"));
    let remaining: i64 = db
        .query_row("SELECT count(*) FROM audit WHERE logged=0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(remaining, 2);
    fs::remove_dir(&log).unwrap();
    fs::rename(&saved_log, &log).unwrap();
    let error = journal::replay_logs(&f.ctx.c.state_dir, &logger).unwrap_err();
    assert!(error.to_string().contains("Corrupt audit record"));
    let remaining: i64 = db
        .query_row("SELECT count(*) FROM audit WHERE logged=0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(remaining, 1);
}
