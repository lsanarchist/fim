use fim::{baseline, config::ContextData, publisher, scan, secure, Issue, Issues};
use std::{
    fs::{self, File, FileTimes},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

struct Fixture {
    _tmp: tempfile::TempDir,
    ctx: ContextData,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        for name in ["site", "state", "staging"] {
            fs::create_dir(p.join(name)).unwrap();
            fs::set_permissions(
                p.join(name),
                fs::Permissions::from_mode(if name == "site" { 0o755 } else { 0o700 }),
            )
            .unwrap();
        }
        fs::write(p.join("namedays.json"), include_bytes!("../namedays.json")).unwrap();
        fs::write(
            p.join("index.template.html"),
            include_bytes!("../index.template.html"),
        )
        .unwrap();
        fs::write(p.join("site/style.css"), b"body { margin: 0; }\n").unwrap();
        let mail = p.join("sendmail-test");
        fs::write(&mail, "#!/bin/sh\ncat >/dev/null\n").unwrap();
        fs::set_permissions(&mail, fs::Permissions::from_mode(0o700)).unwrap();
        let mut config: serde_json::Value =
            serde_json::from_str(include_str!("../config.json")).unwrap();
        config["sendmail"] = serde_json::json!(mail);
        fs::write(p.join("config.json"), serde_json::to_vec(&config).unwrap()).unwrap();
        let ctx = ContextData::load(&p.join("config.json"), true).unwrap();
        publisher::page(&ctx).unwrap();
        baseline::create(&ctx).unwrap();
        Self { _tmp: tmp, ctx }
    }
    fn issues(&self) -> Issues {
        scan::compare(
            &baseline::load(&self.ctx).unwrap().entries,
            &scan::run(&self.ctx),
            &Issues::new(),
        )
    }
    fn path(&self, path: &str) -> PathBuf {
        self.ctx.c.root.join(path)
    }
    fn edit_record(&self, path: &str, edit: impl FnOnce(&mut serde_json::Value)) {
        let db = rusqlite::Connection::open(&self.ctx.c.database).unwrap();
        let key = secure::path_key(Path::new(path));
        let record: String = db
            .query_row(
                "SELECT record FROM baseline WHERE path_hex=?1",
                [&key],
                |row| row.get(0),
            )
            .unwrap();
        let mut value = serde_json::from_str(&record).unwrap();
        edit(&mut value);
        db.execute(
            "UPDATE baseline SET record=?1 WHERE path_hex=?2",
            [&serde_json::to_string(&value).unwrap(), &key],
        )
        .unwrap();
    }
}
fn has(issues: &Issues, kind: &str, path: &str) -> bool {
    issues.values().any(|issue| {
        issue.kind == kind && issue.path_hex == Some(secure::path_key(Path::new(path)))
    })
}
fn shift_mtime(path: &Path) {
    let original = fs::metadata(path).unwrap().modified().unwrap();
    File::open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(original + Duration::from_secs(10)))
        .unwrap();
}

#[test]
fn baseline_v3_records_size_and_precise_mtime_for_regular_files() {
    let f = Fixture::new();
    let baseline = baseline::load(&f.ctx).unwrap();
    let metadata = fs::metadata(f.path("style.css")).unwrap();
    let fingerprint = &baseline.entries[Path::new("style.css")];
    assert_eq!(fingerprint.size, Some(metadata.len()));
    let modified = fingerprint.modified.as_ref().unwrap();
    assert_eq!(modified.seconds, metadata.mtime());
    assert_eq!(i64::from(modified.nanoseconds), metadata.mtime_nsec());
    assert_eq!(fingerprint.created.is_some(), metadata.created().is_ok());
    let db = rusqlite::Connection::open(&f.ctx.c.database).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn timestamp_only_change_is_an_incident_without_content_change() {
    let f = Fixture::new();
    shift_mtime(&f.path("style.css"));
    let issues = f.issues();
    assert!(has(&issues, "METADATA_CHANGED", "style.css"));
    assert!(!has(&issues, "CONTENT_CHANGED", "style.css"));
}

#[test]
fn restoring_bytes_alone_does_not_hide_a_rewrite() {
    let f = Fixture::new();
    let path = f.path("style.css");
    let bytes = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, b"unexpected contents").unwrap();
    fs::write(&path, bytes).unwrap();
    shift_mtime(&path);
    assert!(has(&f.issues(), "METADATA_CHANGED", "style.css"));
    File::open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(modified))
        .unwrap();
    assert!(f.issues().is_empty());
}

#[test]
fn resized_file_reports_content_and_metadata_changes() {
    let f = Fixture::new();
    fs::write(
        f.path("style.css"),
        b"a different and much longer stylesheet body",
    )
    .unwrap();
    let issues = f.issues();
    assert!(has(&issues, "CONTENT_CHANGED", "style.css"));
    assert!(has(&issues, "METADATA_CHANGED", "style.css"));
}

#[test]
fn daily_index_exemption_does_not_exempt_its_permissions_or_other_files() {
    let f = Fixture::new();
    let baseline = baseline::load(&f.ctx).unwrap();
    for path in ["", "index.html"] {
        let fingerprint = &baseline.entries[Path::new(path)];
        assert!(fingerprint.size.is_none());
        assert!(fingerprint.modified.is_none());
        assert!(fingerprint.created.is_none());
    }
    shift_mtime(&f.path("index.html"));
    assert!(f.issues().is_empty());
    fs::set_permissions(f.path("index.html"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(has(&f.issues(), "METADATA_CHANGED", "index.html"));
    shift_mtime(&f.path("style.css"));
    assert!(has(&f.issues(), "METADATA_CHANGED", "style.css"));
}

#[test]
fn older_baseline_requires_explicit_trusted_rebuild_without_mutation() {
    let f = Fixture::new();
    let db = rusqlite::Connection::open(&f.ctx.c.database).unwrap();
    db.pragma_update(None, "user_version", 2).unwrap();
    drop(db);
    let original = fs::read(&f.ctx.c.database).unwrap();
    let error = baseline::load(&f.ctx).err().unwrap().to_string();
    assert!(error.contains("version 2"));
    assert!(error.contains("verify the website"));
    assert!(error.contains("new database path and state_dir"));
    assert_eq!(fs::read(&f.ctx.c.database).unwrap(), original);
}

#[test]
fn incomplete_or_invalid_v3_metadata_is_not_silently_accepted() {
    let f = Fixture::new();
    f.edit_record("style.css", |record| {
        record["size"] = serde_json::Value::Null;
    });
    assert!(baseline::load(&f.ctx).is_err());
    let f = Fixture::new();
    f.edit_record("style.css", |record| {
        record["modified"]["nanoseconds"] = serde_json::json!(1_000_000_000u32);
    });
    assert!(baseline::load(&f.ctx).is_err());
}

#[test]
fn unavailable_birth_time_is_valid_and_is_not_fabricated_from_ctime() {
    let f = Fixture::new();
    f.edit_record("style.css", |record| {
        record["created"] = serde_json::Value::Null;
    });
    let baseline = baseline::load(&f.ctx).unwrap();
    assert!(baseline.entries[Path::new("style.css")].created.is_none());
}

#[test]
fn final_path_verification_rejects_a_same_inode_mutation() {
    let f = Fixture::new();
    let root = secure::directory(&f.ctx.c.root).unwrap();
    let path = Path::new("style.css");
    let observed = secure::beneath(&root, path, libc::O_RDONLY)
        .unwrap()
        .metadata()
        .unwrap();
    secure::verify_beneath(&root, path, &observed).unwrap();
    shift_mtime(&f.path("style.css"));
    assert!(secure::identity(
        &observed,
        &fs::metadata(f.path("style.css")).unwrap()
    ));
    assert!(secure::verify_beneath(&root, path, &observed).is_err());
}

#[test]
fn unknown_directory_does_not_overwrite_previous_incident_with_stale_entries() {
    let f = Fixture::new();
    let baseline = baseline::load(&f.ctx).unwrap();
    let mut scan = scan::run(&f.ctx);
    scan.unknown.insert(PathBuf::new());
    scan.entries.get_mut(Path::new("style.css")).unwrap().sha256 = "changed".into();
    let old = Issue::new(
        "CONTENT_CHANGED",
        Some(Path::new("style.css")),
        "last verified incident",
    );
    let mut previous = Issues::new();
    fim::add(&mut previous, old.clone());
    let issues = scan::compare(&baseline.entries, &scan, &previous);
    assert_eq!(issues.len(), 1);
    assert_eq!(issues.values().next().unwrap(), &old);
}

#[test]
fn baseline_path_aliases_and_nul_are_rejected() {
    for path in ["a//b", "a/./b", "a/", "a\0b"] {
        assert!(secure::key_path(&secure::path_key(Path::new(path))).is_err());
    }
    assert!(secure::key_path(&secure::path_key(Path::new("foo")).to_uppercase()).is_err());
    assert_eq!(secure::key_path("").unwrap(), Path::new(""));
}

#[test]
fn baseline_with_missing_parent_directory_is_rejected() {
    let f = Fixture::new();
    let db = rusqlite::Connection::open(&f.ctx.c.database).unwrap();
    db.execute("DELETE FROM baseline WHERE path_hex=''", [])
        .unwrap();
    drop(db);
    assert!(baseline::load(&f.ctx).is_err());
}

#[test]
fn init_refuses_policy_changed_since_context_was_loaded() {
    let f = Fixture::new();
    fs::remove_file(&f.ctx.c.database).unwrap();
    fs::write(&f.ctx.c.template, b"changed policy").unwrap();
    assert!(baseline::create(&f.ctx).is_err());
    assert!(!f.ctx.c.database.exists());
}
