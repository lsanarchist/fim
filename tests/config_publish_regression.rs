use chrono::Utc;
use fim::{
    alerts, baseline,
    config::{ContextData, MailContext},
    daily, journal, publisher,
};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

struct Fixture {
    tmp: tempfile::TempDir,
    config: serde_json::Value,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
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
        fs::write(
            &mta,
            format!("#!/bin/sh\ncat >> '{}'\n", p.join("mail.eml").display()),
        )
        .unwrap();
        fs::set_permissions(&mta, fs::Permissions::from_mode(0o700)).unwrap();
        let mut config: serde_json::Value =
            serde_json::from_str(include_str!("../config.json")).unwrap();
        config["sendmail"] = serde_json::json!(mta);
        config["alert_timeout_seconds"] = serde_json::json!(1);
        config["retry_seconds"] = serde_json::json!(1);
        let fixture = Self { tmp, config };
        fixture.save();
        fixture
    }
    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }
    fn save(&self) {
        fs::write(
            self.path("config.json"),
            serde_json::to_vec(&self.config).unwrap(),
        )
        .unwrap();
    }
    fn context(&self) -> anyhow::Result<ContextData> {
        ContextData::load(&self.path("config.json"), true)
    }
    fn cli(&self, command: &str) {
        let result = Command::new(env!("CARGO_BIN_EXE_fim"))
            .args([
                "--demo",
                self.path("config.json").to_str().unwrap(),
                command,
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn delivery_and_alert_test_do_not_depend_on_website_inputs() {
    for damaged in [
        "calendar-missing",
        "calendar-invalid",
        "template-missing",
        "template-invalid",
        "root-missing",
        "staging-missing",
        "baseline-invalid",
    ] {
        let f = Fixture::new();
        let mail = MailContext::load(&f.path("config.json"), true).unwrap();
        journal::external(
            &mail.c,
            "TEST",
            "policy-independent notification".into(),
            true,
        )
        .unwrap();
        match damaged {
            "calendar-missing" => fs::remove_file(f.path("namedays.json")).unwrap(),
            "calendar-invalid" => fs::write(f.path("namedays.json"), b"invalid JSON").unwrap(),
            "template-missing" => fs::remove_file(f.path("index.template.html")).unwrap(),
            "template-invalid" => fs::write(f.path("index.template.html"), [0xff]).unwrap(),
            "root-missing" => fs::remove_dir(f.path("site")).unwrap(),
            "staging-missing" => fs::remove_dir(f.path("staging")).unwrap(),
            "baseline-invalid" => fs::write(f.path("baseline.db"), b"invalid database").unwrap(),
            _ => unreachable!(),
        }
        f.cli("deliver");
        assert_eq!(
            journal::queue_size(&mail.c.state_dir).unwrap(),
            0,
            "{damaged}"
        );
        assert!(fs::read_to_string(f.path("mail.eml"))
            .unwrap()
            .contains("policy-independent notification"));
        f.cli("alert-test");
        assert_eq!(
            journal::queue_size(&mail.c.state_dir).unwrap(),
            0,
            "{damaged}"
        );
        assert!(fs::read_to_string(f.path("mail.eml"))
            .unwrap()
            .contains("Test notification"));
    }
}

#[test]
fn narrow_delivery_retains_config_and_private_state_validation() {
    let f = Fixture::new();
    fs::set_permissions(f.path("state"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(MailContext::load(&f.path("config.json"), true).is_err());
    fs::set_permissions(f.path("state"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(f.path("config.json"), b"invalid JSON").unwrap();
    assert!(MailContext::load(&f.path("config.json"), true).is_err());
}

#[test]
fn narrow_delivery_refuses_runtime_storage_inside_the_website() {
    let mut f = Fixture::new();
    fs::create_dir(f.path("site/state")).unwrap();
    fs::set_permissions(f.path("site/state"), fs::Permissions::from_mode(0o700)).unwrap();
    f.config["state_dir"] = serde_json::json!("site/state");
    f.save();
    assert!(MailContext::load(&f.path("config.json"), true).is_err());
}

#[test]
fn invalid_mail_timeout_is_rejected_before_launching_the_mta() {
    let f = Fixture::new();
    let mut mail = MailContext::load(&f.path("config.json"), true).unwrap();
    let mut sender = alerts::Sender::default();
    for timeout in [0, 61, u64::MAX] {
        mail.c.alert_timeout_seconds = timeout;
        assert!(sender.send_mail(&mail, "must not be sent").is_err());
    }
    assert!(!f.path("mail.eml").exists());
}

#[test]
fn publisher_rejects_oversized_render_without_creating_index_or_candidate() {
    let f = Fixture::new();
    let mut ctx = f.context().unwrap();
    let today = Utc::now().with_timezone(&ctx.tz).date_naive();
    ctx.names
        .insert(today.format("%m-%d").to_string(), "&".repeat(1024));
    ctx.c.max_index_bytes = 1024;
    let error = publisher::page(&ctx).unwrap_err();
    assert!(error
        .to_string()
        .contains("Rendered index exceeds size limit"));
    assert!(!f.path("site/index.html").exists());
    assert_eq!(fs::read_dir(f.path("staging")).unwrap().count(), 0);
    assert!(!f.path("state/publication.json").exists());
}

#[test]
fn publisher_rejects_oversized_render_without_replacing_previous_index_or_receipt() {
    let f = Fixture::new();
    let mut ctx = f.context().unwrap();
    publisher::page(&ctx).unwrap();
    baseline::create(&ctx).unwrap();
    let index = fs::read(f.path("site/index.html")).unwrap();
    let receipt = fs::read(f.path("state/publication.json")).unwrap();
    let today = Utc::now().with_timezone(&ctx.tz).date_naive();
    ctx.names
        .insert(today.format("%m-%d").to_string(), "&".repeat(1024));
    ctx.c.max_index_bytes = 1024;
    let error = publisher::page(&ctx).unwrap_err();
    assert!(error
        .to_string()
        .contains("Rendered index exceeds size limit"));
    assert_eq!(fs::read(f.path("site/index.html")).unwrap(), index);
    assert_eq!(fs::read(f.path("state/publication.json")).unwrap(), receipt);
    assert_eq!(fs::read_dir(f.path("staging")).unwrap().count(), 0);
}

#[test]
fn config_validates_escaped_size_of_future_calendar_entries() {
    let mut f = Fixture::new();
    let ctx = f.context().unwrap();
    let tomorrow = Utc::now()
        .with_timezone(&ctx.tz)
        .date_naive()
        .succ_opt()
        .unwrap();
    let mut names = ctx.names.clone();
    names.insert(tomorrow.format("%m-%d").to_string(), "&".repeat(1024));
    // The raw name is valid; HTML escaping makes the future page exceed its limit.
    daily::validate_calendar(&names).unwrap();
    fs::write(f.path("namedays.json"), serde_json::to_vec(&names).unwrap()).unwrap();
    f.config["max_index_bytes"] = serde_json::json!(1024);
    f.save();
    let error = f.context().err().unwrap();
    assert!(error
        .to_string()
        .contains("Rendered index exceeds size limit"));
    assert!(!f.path("site/index.html").exists());
}

#[test]
fn extreme_publication_receipt_timestamps_fail_closed() {
    let f = Fixture::new();
    let ctx = f.context().unwrap();
    publisher::page(&ctx).unwrap();
    let path = f.path("state/publication.json");
    let mut receipt: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for timestamp in [i64::MIN, i64::MAX] {
        receipt["at"] = serde_json::json!(timestamp);
        fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(publisher::authorized_trackers(&ctx, &[]).is_empty());
    }
}

#[test]
fn scan_and_reminder_intervals_are_bounded_before_monitor_start() {
    let mut f = Fixture::new();
    for (field, invalid) in [
        ("interval_seconds", 0),
        ("interval_seconds", 86401),
        ("reminder_seconds", 59),
        ("reminder_seconds", 31_536_001),
        ("reminder_seconds", u64::MAX),
    ] {
        let original = f.config[field].clone();
        f.config[field] = serde_json::json!(invalid);
        f.save();
        assert!(f.context().is_err(), "{field}={invalid}");
        f.config[field] = original;
    }
    for reminder in [60, 31_536_000] {
        f.config["reminder_seconds"] = serde_json::json!(reminder);
        f.save();
        assert!(f.context().is_ok(), "reminder_seconds={reminder}");
    }
}
