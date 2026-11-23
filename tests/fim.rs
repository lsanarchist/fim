use chrono::{Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Europe::Bratislava;
use fim::{config::Config, daily, db, monitor, page, scan, App};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

const TOKEN: &str = "123456:fake_token_for_tests_only_xyz";

/// A temporary site with style.css, today's index.html and a fresh baseline.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    app: App,
}

fn write_config(root: &std::path::Path, extra: &str) {
    let config = format!(
        r#"{{"site_dir": "site", "database": "baseline.db", "log_file": "fim.log",
        "template": "template.html", "namedays": "namedays.json",
        "timezone": "Europe/Bratislava", "interval_seconds": 30,
        "telegram_bot_token": "{TOKEN}", "telegram_chat_id": "42"{extra}}}"#
    );
    fs::write(root.join("config.json"), config).unwrap();
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::create_dir(root.join("site")).unwrap();
    fs::write(root.join("site/style.css"), "body { color: black }").unwrap();
    fs::copy(repo.join("index.template.html"), root.join("template.html")).unwrap();
    fs::copy(repo.join("namedays.json"), root.join("namedays.json")).unwrap();
    write_config(&root, "");
    let app = App::load(&root.join("config.json")).unwrap();
    page::page(&app).unwrap();
    monitor::init(&app).unwrap();
    Fixture {
        _dir: dir,
        root,
        app,
    }
}

/// Runs one cycle with a fake Telegram that records messages.
fn cycle(f: &Fixture, sent: &mut Vec<String>) -> usize {
    let db = db::open(&f.app.cfg.database).unwrap();
    let mut send = |text: &str| -> anyhow::Result<()> {
        sent.push(text.to_string());
        Ok(())
    };
    monitor::cycle(&f.app, &db, &mut send).unwrap()
}

fn index_with_block(f: &Fixture, block: &str) {
    let template = fs::read_to_string(f.root.join("template.html")).unwrap();
    fs::write(
        f.root.join("site/index.html"),
        template.replace(daily::TOKEN, block),
    )
    .unwrap();
}

#[test]
fn config_rejects_unknown_fields_and_bad_values() {
    let f = fixture();
    write_config(&f.root, r#", "colour": "blue""#);
    let err = format!(
        "{:#}",
        Config::load(&f.root.join("config.json")).err().unwrap()
    );
    assert!(err.contains("unknown field"), "{err}");
    let bad = fs::read_to_string(f.root.join("config.json")).unwrap();
    let cases = [
        bad.replace(r#", "colour": "blue""#, "").replace("30", "0"),
        bad.replace(r#", "colour": "blue""#, "")
            .replace("\"baseline.db\"", "\"site/b.db\""),
        bad.replace(r#", "colour": "blue""#, "")
            .replace(TOKEN, "not-a-token-but-secret"),
    ];
    for case in cases {
        fs::write(f.root.join("config.json"), &case).unwrap();
        let err = format!(
            "{:#}",
            Config::load(&f.root.join("config.json")).err().unwrap()
        );
        assert!(
            !err.contains("fake_token") && !err.contains("but-secret"),
            "{err}"
        );
    }
}

#[test]
fn calendar_is_complete_and_uses_slovak_date_format() {
    let names = daily::load_names(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("namedays.json"))
        .unwrap();
    let day = |m, d| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
    assert_eq!(
        daily::text(&names, day(11, 22)).unwrap(),
        "Dnes je 22. 11. 2026. Meniny má Cecília."
    );
    assert_eq!(
        daily::text(&names, day(1, 1)).unwrap(),
        "Dnes je 1. 1. 2026. Dnes nemá meniny nikto."
    );
}

#[test]
fn daily_block_is_valid_only_for_the_right_day() {
    let f = fixture();
    let day = NaiveDate::from_ymd_opt(2026, 11, 22).unwrap();
    let template = fs::read_to_string(f.root.join("template.html")).unwrap();
    let html = daily::render(&template, &f.app.names, day).unwrap();
    assert_eq!(
        daily::normalize(&html, &f.app.names, &[day]).unwrap(),
        template
    );
    assert!(daily::normalize(&html, &f.app.names, &[day + Duration::days(1)]).is_err());
    let forged = html.replace("Cecília", "Hacker");
    assert!(daily::normalize(&forged, &f.app.names, &[day]).is_err());
}

#[test]
fn yesterday_is_accepted_only_shortly_after_midnight() {
    let at = |d, h, m| Bratislava.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap();
    let yesterday = at(21, 18, 0); // the page was written yesterday evening
    assert_eq!(daily::accepted_dates(at(22, 0, 1), yesterday).len(), 2);
    assert_eq!(daily::accepted_dates(at(22, 0, 3), yesterday).len(), 1);
    assert_eq!(daily::accepted_dates(at(22, 12, 0), yesterday).len(), 1);
    // A page written after midnight must carry today's date, even within the grace period.
    assert_eq!(daily::accepted_dates(at(22, 0, 1), at(22, 0, 0)).len(), 1);
}

#[test]
fn clean_site_has_no_incident() {
    let f = fixture();
    let mut sent = Vec::new();
    assert_eq!(cycle(&f, &mut sent), 0);
    assert!(sent.is_empty());
    // The streamed hash is the real SHA-256 of the file.
    let base = db::load_baseline(&db::open(&f.app.cfg.database).unwrap()).unwrap();
    assert_eq!(
        base["style.css"].sha256,
        scan::sha256(b"body { color: black }")
    );
}

#[test]
fn modification_alerts_once_and_recovery_alerts_once() {
    let f = fixture();
    let css = f.root.join("site/style.css");
    let original = fs::metadata(&css).unwrap().modified().unwrap();
    let mut sent = Vec::new();
    fs::write(&css, "body { color: red }").unwrap();
    assert_eq!(cycle(&f, &mut sent), 1);
    assert_eq!(cycle(&f, &mut sent), 1);
    assert_eq!(sent.len(), 1, "an open incident must not be repeated");
    assert!(sent[0].contains("MODIFIED") && sent[0].contains("style.css"));
    fs::write(&css, "body { color: black }").unwrap();
    fs::File::options()
        .write(true)
        .open(&css)
        .unwrap()
        .set_modified(original)
        .unwrap();
    assert_eq!(cycle(&f, &mut sent), 0);
    assert_eq!(sent.len(), 2);
    assert!(sent[1].contains("obnovené"));
}

#[test]
fn added_and_deleted_files_are_incidents() {
    let f = fixture();
    let mut sent = Vec::new();
    fs::write(f.root.join("site/shell.php"), "<?php system($_GET['c']);").unwrap();
    fs::remove_file(f.root.join("site/style.css")).unwrap();
    // A new file the monitor cannot read (e.g. created with umask 077) is still ADDED.
    let secret = f.root.join("site/secret.txt");
    fs::write(&secret, "x").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(cycle(&f, &mut sent), 3);
    let all = sent.join("\n");
    assert!(all.contains("ADDED") && all.contains("shell.php"));
    assert!(all.contains("DELETED") && all.contains("style.css"));
    assert!(
        all.contains("Súbor: secret.txt\nDetail: unreadable"),
        "{all}"
    );
}

#[test]
fn permission_change_is_a_metadata_incident() {
    let f = fixture();
    let mut sent = Vec::new();
    fs::set_permissions(
        f.root.join("site/style.css"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert_eq!(cycle(&f, &mut sent), 1);
    assert!(sent[0].contains("METADATA") && sent[0].contains("-> 0777"));
    // chown needs root, so the owner check is tested on the comparison itself.
    let base = db::load_baseline(&db::open(&f.app.cfg.database).unwrap()).unwrap();
    let mut now = base.clone();
    now.get_mut("style.css").unwrap().uid += 1;
    let issues = monitor::compare(&base, &now, Vec::new());
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].kind, "METADATA");
    assert!(
        issues[0].detail.starts_with("owner "),
        "{}",
        issues[0].detail
    );
}

#[test]
fn forged_daily_block_is_an_incident() {
    let f = fixture();
    let mut sent = Vec::new();
    index_with_block(
        &f,
        "Dnes je 1. 1. 2000. Meniny má <script>alert(1)</script>.",
    );
    assert_eq!(cycle(&f, &mut sent), 1);
    assert!(sent[0].contains("DAILY_BLOCK") && sent[0].contains("index.html"));
}

#[test]
fn failed_alert_is_kept_and_retried() {
    let f = fixture();
    let db = db::open(&f.app.cfg.database).unwrap();
    fs::write(f.root.join("site/new.txt"), "x").unwrap();
    let mut down = |_: &str| -> anyhow::Result<()> { anyhow::bail!("network down") };
    assert_eq!(monitor::cycle(&f.app, &db, &mut down).unwrap(), 1);
    assert_eq!(db::pending_alerts(&db).unwrap().len(), 1);
    let mut sent = Vec::new();
    cycle(&f, &mut sent);
    assert_eq!(
        sent.len(),
        1,
        "the waiting alert is delivered on the next cycle"
    );
    assert!(db::pending_alerts(&db).unwrap().is_empty());
    let log = fs::read_to_string(f.root.join("fim.log")).unwrap();
    assert!(log.contains("ALERT_FAILED") && !log.contains("fake_token"));
}

#[test]
fn page_replaces_an_old_block_without_incident() {
    let f = fixture();
    let old = Utc::now().with_timezone(&Bratislava).date_naive() - Duration::days(10);
    index_with_block(&f, &daily::text(&f.app.names, old).unwrap());
    let mut sent = Vec::new();
    assert_eq!(cycle(&f, &mut sent), 1, "a stale date is detected");
    page::page(&f.app).unwrap();
    assert_eq!(cycle(&f, &mut sent), 0);
    assert!(sent[1].contains("obnovené"));
}

#[test]
fn page_refuses_to_overwrite_a_tampered_page() {
    let f = fixture();
    let index = f.root.join("site/index.html");
    let tampered = fs::read_to_string(&index)
        .unwrap()
        .replace("Moja stránka</h1>", "Hacked</h1>");
    fs::write(&index, &tampered).unwrap();
    assert!(page::page(&f.app).is_err());
    assert_eq!(
        fs::read_to_string(&index).unwrap(),
        tampered,
        "evidence is kept"
    );
}

#[test]
fn init_never_overwrites_the_baseline() {
    let f = fixture();
    fs::write(f.root.join("site/style.css"), "changed").unwrap();
    assert!(monitor::init(&f.app).is_err());
    let mut sent = Vec::new();
    assert_eq!(cycle(&f, &mut sent), 1);
}
