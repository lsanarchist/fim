use fim::{
    add,
    config::Config,
    journal::{self, Audit, Journal, Logger},
    secure, Issue, Issues,
};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    _tmp: tempfile::TempDir,
    c: Config,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let mut c: Config = serde_json::from_str(include_str!("../config.json")).unwrap();
        c.state_dir = tmp.path().join("state");
        c.log_max_bytes = u64::MAX;
        fs::create_dir(&c.state_dir).unwrap();
        fs::set_permissions(&c.state_dir, fs::Permissions::from_mode(0o700)).unwrap();
        Self { _tmp: tmp, c }
    }
}

fn incidents(n: usize) -> Issues {
    let mut issues = Issues::new();
    for n in 0..n {
        add(
            &mut issues,
            Issue::new("TEST", Some(Path::new(&format!("file-{n}"))), "changed"),
        );
    }
    issues
}

fn stored_alerts(dir: &Path) -> usize {
    let db = journal::open_db(dir).unwrap();
    let mut stmt = db.prepare("SELECT body FROM outbox").unwrap();
    let bodies = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    bodies
        .iter()
        .map(|body| serde_json::from_str::<Vec<Audit>>(body).unwrap().len())
        .sum()
}

fn audit_count(dir: &Path) -> usize {
    journal::open_db(dir)
        .unwrap()
        .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn five_thousand_incidents_are_durable_without_queue_loss() {
    let f = Fixture::new();
    let next = incidents(5000);
    let mut j = Journal::new(&f.c);
    j.reconcile(next.clone());
    j.flush().unwrap();
    assert_eq!(j.dropped, 0);
    assert_eq!(j.pending(), 0);
    assert_eq!(audit_count(&f.c.state_dir), 5000);
    assert_eq!(stored_alerts(&f.c.state_dir), 5000);
    drop(j);
    let restarted = Journal::new(&f.c);
    assert_eq!(restarted.active, next);
    assert!(restarted.storage_error.is_none());
}

#[test]
fn completed_full_batches_checkpoint_on_the_final_empty_flush() {
    let f = Fixture::new();
    let next = incidents(64);
    let mut j = Journal::new(&f.c);
    j.reconcile(next.clone());
    assert_eq!(j.pending(), 0);
    assert_eq!(audit_count(&f.c.state_dir), 64);
    assert!(Journal::new(&f.c).active.is_empty());
    j.flush().unwrap();
    assert_eq!(Journal::new(&f.c).active, next);
}

#[test]
fn partial_reconcile_cannot_checkpoint_future_incidents() {
    let f = Fixture::new();
    let previous = incidents(1);
    let mut j = Journal::new(&f.c);
    j.reconcile(previous.clone());
    j.flush().unwrap();
    let db = journal::open_db(&f.c.state_dir).unwrap();
    db.execute_batch(
        "CREATE TRIGGER stop_audit BEFORE INSERT ON audit
        WHEN (SELECT count(*) FROM audit)>=33
        BEGIN SELECT RAISE(ABORT, 'injected persistent failure'); END;",
    )
    .unwrap();
    let next = incidents(65);
    j.reconcile(next.clone());
    assert!(j.flush().is_err());
    assert_eq!(audit_count(&f.c.state_dir), 33);
    assert_eq!(Journal::new(&f.c).active, previous);
    db.execute_batch("DROP TRIGGER stop_audit").unwrap();
    while j.pending() > 0 {
        j.flush().unwrap();
    }
    assert_eq!(Journal::new(&f.c).active, next);
    assert_eq!(stored_alerts(&f.c.state_dir), 65);
}

#[test]
fn unavailable_storage_bounds_memory_without_checkpointing_lost_audit() {
    let f = Fixture::new();
    let state_db = f.c.state_dir.join("state.db");
    fs::create_dir(&state_db).unwrap();
    let mut j = Journal::new(&f.c);
    j.reconcile(incidents(5000));
    assert!(j.storage_error.is_some());
    assert_eq!(j.pending(), 4096);
    assert_eq!(j.dropped, 904);
    fs::remove_dir(&state_db).unwrap();
    while j.pending() > 0 {
        j.flush().unwrap();
    }
    assert_eq!(audit_count(&f.c.state_dir), 4096);
    assert_eq!(stored_alerts(&f.c.state_dir), 4096);
    assert!(
        Journal::new(&f.c).active.is_empty(),
        "lost transitions must be re-observed after restart"
    );
}

fn event(id: String) -> Audit {
    Audit {
        id,
        time: "2026-10-11T00:00:00Z".into(),
        kind: "TEST".into(),
        detail: "line one\nline two \"quoted\"".repeat(64),
        alert: false,
    }
}

fn log_records(dir: &Path) -> Vec<Audit> {
    let mut records = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name == "fim.log"
            || name
                .strip_prefix("fim.log.")
                .is_some_and(|n| n.parse::<usize>().is_ok())
        {
            let bytes = fs::read(path).unwrap();
            assert!(bytes.ends_with(b"\n"));
            for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
                records.push(serde_json::from_slice(line).unwrap());
            }
        }
    }
    records
}

fn write_during_contention(logger: &Logger, audit: &Audit, deadline: Instant) {
    loop {
        if logger.write(audit) {
            return;
        }
        // Production intentionally gives up a busy log lock before writing so
        // the durable journal can retry later. These direct-logger concurrency
        // tests exercise the same retry, without accepting any other I/O error
        // or retrying a record that may already have reached the file.
        assert_eq!(
            logger.error().as_deref(),
            Some("Timed out acquiring log lock"),
            "unexpected log failure for {}",
            audit.id
        );
        assert!(
            Instant::now() < deadline,
            "log writer exceeded its contention budget for {}",
            audit.id
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn independently_created_loggers_write_complete_lines() {
    let f = Fixture::new();
    let barrier = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|worker| {
            let logger = Logger::new(&f.c);
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                let deadline = Instant::now() + Duration::from_secs(30);
                for n in 0..500 {
                    write_during_contention(&logger, &event(format!("{worker}-{n}")), deadline);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let records = log_records(&f.c.state_dir);
    assert_eq!(records.len(), 2000);
    assert_eq!(
        records.iter().map(|r| &r.id).collect::<BTreeSet<_>>().len(),
        2000
    );
}

#[test]
fn log_lock_timeout_is_bounded_and_event_has_sqlite_fallback() {
    let f = Fixture::new();
    let _lock = secure::lock(&f.c.state_dir.join("fim.log.lock")).unwrap();
    let mut j = Journal::new(&f.c);
    let began = Instant::now();
    j.emit("TEST", "locked logger", true);
    assert!(began.elapsed() < Duration::from_secs(2));
    assert!(j.logger.error().is_some());
    let burst = Instant::now();
    for _ in 0..20 {
        j.emit("TEST", "still locked", true);
    }
    assert!(
        burst.elapsed() < Duration::from_secs(1),
        "lock cooldown must protect event bursts"
    );
    j.flush().unwrap();
    let db = journal::open_db(&f.c.state_dir).unwrap();
    let unlogged: i64 = db
        .query_row("SELECT count(*) FROM audit WHERE logged=0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(unlogged, 21);
    drop(_lock);
    journal::replay_logs(&f.c.state_dir, &j.logger).unwrap();
    assert_eq!(log_records(&f.c.state_dir).len(), 21);
    assert!(j.logger.error().is_none());
}

#[test]
fn subprocess_log_worker() {
    let Some(state) = std::env::var_os("FIM_TEST_LOG_STATE") else {
        return;
    };
    let worker = std::env::var("FIM_TEST_LOG_WORKER").unwrap();
    let max = std::env::var("FIM_TEST_LOG_MAX").unwrap().parse().unwrap();
    let mut c: Config = serde_json::from_str(include_str!("../config.json")).unwrap();
    c.state_dir = PathBuf::from(state);
    c.log_max_bytes = max;
    let logger = Logger::new(&c);
    let deadline = Instant::now() + Duration::from_secs(30);
    for n in 0..100 {
        write_during_contention(&logger, &event(format!("process-{worker}-{n}")), deadline);
    }
}

#[test]
fn subprocess_partial_write_worker() {
    let Some(state) = std::env::var_os("FIM_TEST_PARTIAL_LOG_STATE") else {
        return;
    };
    let mut c: Config = serde_json::from_str(include_str!("../config.json")).unwrap();
    c.state_dir = PathBuf::from(state);
    c.log_max_bytes = u64::MAX;
    let logger = Logger::new(&c);
    assert!(logger.write(&event("before-failure".into())));
    let path = c.state_dir.join("fim.log");
    let original = fs::read(&path).unwrap();
    // SAFETY: this dedicated child process runs no other test; the signal handler
    // and soft file-size limit are restored before normal logging resumes.
    let (old_limit, old_handler) = unsafe {
        let mut old_limit = std::mem::zeroed::<libc::rlimit>();
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut old_limit), 0);
        let old_handler = libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        assert_ne!(old_handler, libc::SIG_ERR);
        let limited = libc::rlimit {
            rlim_cur: original.len() as libc::rlim_t + 100,
            rlim_max: old_limit.rlim_max,
        };
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limited), 0);
        (old_limit, old_handler)
    };
    assert!(!logger.write(&event("partial-failure".into())));
    let error = logger.error().unwrap();
    assert!(error.contains("incomplete record removed"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), original);
    // SAFETY: restore the values captured above in this isolated child process.
    unsafe {
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &old_limit), 0);
        assert_ne!(libc::signal(libc::SIGXFSZ, old_handler), libc::SIG_ERR);
    }
    assert!(logger.write(&event("after-failure".into())));
    assert!(logger.error().is_none());
    let records = log_records(&c.state_dir);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].id, "before-failure");
    assert_eq!(records[1].id, "after-failure");
}

#[test]
fn failed_log_write_removes_the_partial_record_before_next_append() {
    let f = Fixture::new();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "subprocess_partial_write_worker", "--nocapture"])
        .env("FIM_TEST_PARTIAL_LOG_STATE", &f.c.state_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "partial-write subprocess failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(log_records(&f.c.state_dir).len(), 2);
}

fn process_writers(c: &Config) {
    let children: Vec<_> = (0..4)
        .map(|worker| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "subprocess_log_worker", "--nocapture"])
                .env("FIM_TEST_LOG_STATE", &c.state_dir)
                .env("FIM_TEST_LOG_WORKER", worker.to_string())
                .env("FIM_TEST_LOG_MAX", c.log_max_bytes.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "logger subprocess failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn independent_processes_share_the_log_lock() {
    let f = Fixture::new();
    process_writers(&f.c);
    let records = log_records(&f.c.state_dir);
    assert_eq!(records.len(), 400);
    assert_eq!(
        records.iter().map(|r| &r.id).collect::<BTreeSet<_>>().len(),
        400
    );
}

#[test]
fn concurrent_process_rotation_preserves_complete_bounded_logs() {
    let mut f = Fixture::new();
    f.c.log_max_bytes = 32 * 1024;
    process_writers(&f.c);
    let records = log_records(&f.c.state_dir);
    assert!(!records.is_empty());
    assert_eq!(
        records.iter().map(|r| &r.id).collect::<BTreeSet<_>>().len(),
        records.len()
    );
    assert!(f.c.state_dir.join("fim.log.5").exists());
    assert!(!f.c.state_dir.join("fim.log.6").exists());
    assert!(f.c.state_dir.join("fim.log.lock").exists());
}
