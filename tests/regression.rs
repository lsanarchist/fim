use fim::{add,alerts,baseline,config::ContextData,daily,journal,monitor,publisher,scan,secure,Issue,Issues};
use chrono::{DateTime,NaiveDate,Utc};
use chrono_tz::Europe::Bratislava;
use notify::{Event,EventKind,event::{ModifyKind,DataChange,RenameMode}};
use std::{collections::BTreeSet,fs,io::Read,os::unix::{fs::{symlink,PermissionsExt},ffi::OsStringExt},
    ffi::OsString,path::{Path,PathBuf},sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}},thread,time::{Duration,Instant}};

struct Fixture { _tmp:tempfile::TempDir, ctx:ContextData }
impl Fixture {
    fn new()->Self {
        let tmp=tempfile::tempdir().unwrap(); let p=tmp.path();
        for d in ["site","state","staging"] {
            fs::create_dir(p.join(d)).unwrap();
            fs::set_permissions(p.join(d),fs::Permissions::from_mode(if d=="site" {0o755}else{0o700})).unwrap();
        }
        fs::write(p.join("namedays.json"),include_bytes!("../namedays.json")).unwrap();
        fs::write(p.join("index.template.html"),include_bytes!("../index.template.html")).unwrap();
        fs::write(p.join("site/style.css"),b"body { margin: 0; }\n").unwrap();
        let mail=p.join("sendmail-test");
        fs::write(&mail,format!("#!/bin/sh\ncat > '{}'\n",p.join("mail.eml").display())).unwrap();
        fs::set_permissions(&mail,fs::Permissions::from_mode(0o700)).unwrap();
        let mut config:serde_json::Value=serde_json::from_str(include_str!("../config.json")).unwrap();
        config["sendmail"]=serde_json::json!(mail);
        config["alert_timeout_seconds"]=serde_json::json!(1);
        config["retry_seconds"]=serde_json::json!(1);
        fs::write(p.join("config.json"),serde_json::to_vec(&config).unwrap()).unwrap();
        let ctx=ContextData::load(&p.join("config.json"),true).unwrap();
        publisher::page(&ctx).unwrap(); baseline::create(&ctx).unwrap();
        Self {_tmp:tmp,ctx}
    }
    fn issues(&self)->Issues {
        scan::compare(&baseline::load(&self.ctx).unwrap().entries,&scan::run(&self.ctx),&Issues::new())
    }
    fn mta(&self,body:&str) {fs::write(&self.ctx.c.sendmail,format!("#!/bin/sh\n{body}\n")).unwrap();}
}
fn names()->daily::Names {serde_json::from_str(include_str!("../namedays.json")).unwrap()}
fn date()->NaiveDate {NaiveDate::from_ymd_opt(2026,9,28).unwrap()}
fn html(d:NaiveDate)->String {include_str!("../index.template.html").replace(daily::TOKEN,&daily::value(&names(),d).unwrap())}
fn instant(s:&str)->DateTime<chrono_tz::Tz> {DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Bratislava)}
fn has(i:&Issues,kind:&str,path:&str)->bool {i.values().any(|x|x.kind==kind && x.path_hex==Some(secure::path_key(Path::new(path))))}

#[test] fn calendar_366(){daily::validate_calendar(&names()).unwrap();}
#[test] fn sha256_vector(){assert_eq!(secure::hash(b"abc"),"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");}
#[test] fn daily_rollover_same_fingerprint(){
    let a=daily::normalize_dates(&html(date()),&names(),&[date()]).unwrap();
    let b=daily::normalize_dates(&html(date().succ_opt().unwrap()),&names(),&[date().succ_opt().unwrap()]).unwrap();
    assert_eq!(a,b);
}
#[test] fn read_crossing_midnight_accepts_new_and_old(){
    let a=instant("2026-09-28T23:59:59+02:00");let b=instant("2026-09-29T00:00:01+02:00");
    assert!(daily::normalize(&html(date()),&names(),a,b,0).is_ok());
    assert!(daily::normalize(&html(date().succ_opt().unwrap()),&names(),a,b,0).is_ok());
}
#[test] fn grace_is_exact_and_expires(){
    let a=instant("2026-09-29T00:00:59+02:00");let b=instant("2026-09-29T00:01:00+02:00");
    assert!(daily::normalize(&html(date()),&names(),a,a,60).is_ok());
    assert!(daily::normalize(&html(date()),&names(),b,b,60).is_err());
}
#[test] fn future_date_not_accepted_before_midnight(){
    let a=instant("2026-09-28T23:59:50+02:00");
    assert!(daily::normalize(&html(date().succ_opt().unwrap()),&names(),a,a,60).is_err());
}
#[test] fn wrong_name_and_injected_html_rejected(){
    assert!(daily::normalize_dates(&html(date()).replace("Václav","Wrong"),&names(),&[date()]).is_err());
    assert!(daily::normalize_dates(&html(date()).replace(daily::END,&format!("<script>x</script>{}",daily::END)),&names(),&[date()]).is_err());
}
#[test] fn duplicate_missing_reversed_markers_rejected(){
    for h in [html(date()).replace(daily::START,""),format!("{}{}{}",html(date()),daily::START,daily::END),
        format!("{}{}",daily::END,daily::START)] {assert!(daily::normalize_dates(&h,&names(),&[date()]).is_err());}
}
#[test] fn unchanged_tree_clean(){assert!(Fixture::new().issues().is_empty());}
#[test] fn damaged_index_does_not_hide_other_changes(){
    let f=Fixture::new();
    fs::write(f.ctx.c.root.join("index.html"),"invalid").unwrap();
    fs::write(f.ctx.c.root.join("new.php"),"new").unwrap();
    fs::write(f.ctx.c.root.join("style.css"),"changed").unwrap();
    let i=f.issues();
    assert!(has(&i,"DAILY_INVALID","index.html"));
    assert!(has(&i,"ADDED","new.php"));assert!(has(&i,"CONTENT_CHANGED","style.css"));
}
#[test] fn another_change_in_invalid_index_changes_report(){
    let f=Fixture::new();fs::write(f.ctx.c.root.join("index.html"),"bad one").unwrap();let a=f.issues();
    fs::write(f.ctx.c.root.join("index.html"),"bad two").unwrap();assert_ne!(a,f.issues());
}
#[test] fn added_modified_deleted(){
    let f=Fixture::new();fs::remove_file(f.ctx.c.root.join("style.css")).unwrap();
    fs::write(f.ctx.c.root.join("new.txt"),"x").unwrap();let i=f.issues();
    assert!(has(&i,"DELETED","style.css"));assert!(has(&i,"ADDED","new.txt"));
}
#[test] fn permission_changes_and_empty_directories(){
    let f=Fixture::new();fs::set_permissions(f.ctx.c.root.join("style.css"),fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir(f.ctx.c.root.join("empty")).unwrap();let i=f.issues();
    assert!(has(&i,"METADATA_CHANGED","style.css"));assert!(has(&i,"ADDED","empty"));
}
#[test] fn unknown_prefix_not_deleted_or_recovered(){
    let f=Fixture::new();let base=baseline::load(&f.ctx).unwrap();
    let mut before=Issues::new();add(&mut before,Issue::new("CONTENT_CHANGED",Some(Path::new("style.css")),"old incident"));
    let mut s=scan::Scan::default();s.unknown.insert(PathBuf::new());
    let i=scan::compare(&base.entries,&s,&before);
    assert!(has(&i,"CONTENT_CHANGED","style.css"));assert!(!has(&i,"DELETED","style.css"));
}
#[test] fn symlink_does_not_stop_other_files(){
    let f=Fixture::new();symlink("/etc/passwd",f.ctx.c.root.join("escape")).unwrap();
    fs::write(f.ctx.c.root.join("ok.txt"),"x").unwrap();let i=f.issues();
    assert!(has(&i,"UNREADABLE","escape"));assert!(has(&i,"ADDED","ok.txt"));
}
#[test] fn rejects_intermediate_symlink_and_parent_escape(){
    let f=Fixture::new();let root=secure::directory(&f.ctx.c.root).unwrap();
    symlink("/etc",f.ctx.c.root.join("escape")).unwrap();
    assert!(secure::beneath(&root,Path::new("escape/passwd"),libc::O_RDONLY).is_err());
    assert!(secure::beneath(&root,Path::new("../namedays.json"),libc::O_RDONLY).is_err());
}
#[test] fn non_utf8_paths_are_lossless(){
    let f=Fixture::new();let p=PathBuf::from(OsString::from_vec(vec![b'x',0xff]));
    assert_eq!(secure::key_path(&secure::path_key(&p)).unwrap(),p);
    fs::write(f.ctx.c.root.join(&p),"x").unwrap();
    assert!(scan::run(&f.ctx).entries.contains_key(&p));
}
#[test] fn oversized_file_does_not_hide_smaller_file(){
    let mut f=Fixture::new();f.ctx.c.max_file_bytes=32;
    fs::write(f.ctx.c.root.join("huge"),vec![b'x';1024]).unwrap();
    fs::write(f.ctx.c.root.join("tiny"),"x").unwrap();let s=scan::run(&f.ctx);
    assert!(has(&s.errors,"UNREADABLE","huge"));assert!(s.entries.contains_key(Path::new("tiny")));
}
#[test] fn stream_hash_matches_known_bytes(){
    let f=Fixture::new();let bytes=vec![123u8;1024*1024+17];
    fs::write(f.ctx.c.root.join("large.bin"),&bytes).unwrap();let s=scan::run(&f.ctx);
    assert_eq!(s.entries[Path::new("large.bin")].sha256,secure::hash(&bytes));
}
#[test] fn deadline_expires_without_hanging(){
    let f=Fixture::new();let root=secure::directory(&f.ctx.c.root).unwrap();
    assert!(scan::inspect(&f.ctx,&root,Path::new("style.css"),Instant::now()).is_err());
}
#[test] fn baseline_overwrite_refused(){let f=Fixture::new();assert!(baseline::create(&f.ctx).is_err());}
#[test] fn tampered_baseline_not_used_by_loaded_monitor(){
    let f=Fixture::new();let b=baseline::load(&f.ctx).unwrap();let original=b.entries.clone();
    let db=rusqlite::Connection::open(&f.ctx.c.database).unwrap();db.execute("DELETE FROM baseline",[]).unwrap();drop(db);
    assert!(b.guard.verify(true).is_err());assert_eq!(b.entries,original);
}
#[test] fn calendar_changes_break_guard_and_baseline_policy(){
    let f=Fixture::new();fs::write(&f.ctx.c.calendar,b"{}").unwrap();
    assert!(f.ctx.guards.iter().any(|g|g.verify(true).is_err()));assert!(baseline::load(&f.ctx).is_err());
}
#[test] fn recovery_recorded_even_when_mail_failed(){
    let f=Fixture::new();f.mta("exit 1");let mut j=journal::Journal::new(&f.ctx.c);
    let mut i=Issues::new();add(&mut i,Issue::new("TEST",None,"bad"));j.reconcile(i);j.flush().unwrap();
    assert!(alerts::deliver_one(&f.ctx,&j.logger,&mut alerts::Sender::default()).is_err());
    j.reconcile(Issues::new());j.flush().unwrap();
    let db=journal::open_db(&f.ctx.c.state_dir).unwrap();let found:i64=db.query_row(
        "SELECT count(*) FROM audit WHERE json LIKE '%RECOVERED%'",[],|r|r.get(0)).unwrap();assert!(found>0);
}
#[test] fn outbox_and_active_survive_restart(){
    let f=Fixture::new();let mut j=journal::Journal::new(&f.ctx.c);let mut i=Issues::new();
    add(&mut i,Issue::new("TEST",None,"bad"));j.reconcile(i.clone());j.flush().unwrap();drop(j);
    let j=journal::Journal::new(&f.ctx.c);assert_eq!(j.active,i);assert_eq!(journal::queue_size(&f.ctx.c.state_dir).unwrap(),1);
}
#[test] fn repeated_state_is_not_requeued(){
    let f=Fixture::new();let mut j=journal::Journal::new(&f.ctx.c);let mut i=Issues::new();
    add(&mut i,Issue::new("TEST",None,"bad"));j.reconcile(i.clone());j.flush().unwrap();
    j.reconcile(i);j.flush().unwrap();assert_eq!(journal::queue_size(&f.ctx.c.state_dir).unwrap(),1);
}
#[test] fn mail_submission_is_recorded_without_claiming_delivery(){
    let f=Fixture::new();journal::external(&f.ctx.c,"TEST","test".into(),true).unwrap();
    assert!(alerts::deliver_one(&f.ctx,&journal::Logger::new(&f.ctx.c),&mut alerts::Sender::default()).unwrap());
    assert_eq!(journal::queue_size(&f.ctx.c.state_dir).unwrap(),0);
    let eml=fs::read_to_string(f._tmp.path().join("mail.eml")).unwrap();assert!(eml.contains("admin@example.com"));
}
#[test] fn mail_timeout_kills_group_and_preserves_queue(){
    let f=Fixture::new();f.mta("sleep 30");journal::external(&f.ctx.c,"TEST","test".into(),true).unwrap();
    let start=Instant::now();assert!(alerts::deliver_one(&f.ctx,&journal::Logger::new(&f.ctx.c),&mut alerts::Sender::default()).is_err());
    assert!(start.elapsed()<Duration::from_secs(5));assert_eq!(journal::queue_size(&f.ctx.c.state_dir).unwrap(),1);
}
#[test] fn blocked_mail_worker_does_not_block_scan(){
    let mut f=Fixture::new();f.ctx.c.alert_timeout_seconds=3;f.mta("sleep 30");
    journal::external(&f.ctx.c,"TEST","test".into(),true).unwrap();
    let stop=Arc::new(AtomicBool::new(false));let h=Arc::new(Mutex::new(None));
    let worker=alerts::worker(f.ctx.clone(),journal::Logger::new(&f.ctx.c),stop.clone(),h);
    thread::sleep(Duration::from_millis(100));fs::write(f.ctx.c.root.join("new"),"x").unwrap();
    let began=Instant::now();let i=f.issues();assert!(has(&i,"ADDED","new"));
    assert!(began.elapsed()<Duration::from_secs(2));stop.store(true,Ordering::Relaxed);worker.join().unwrap();
}
#[test] fn log_failure_has_durable_audit_fallback_and_replay(){
    let f=Fixture::new();let p=f.ctx.c.state_dir.join("fim.log");fs::remove_file(&p).unwrap();fs::create_dir(&p).unwrap();
    let mut j=journal::Journal::new(&f.ctx.c);j.emit("TEST","log unavailable",true);j.flush().unwrap();
    assert!(j.logger.error().is_some());assert!(f.issues().is_empty());
    fs::remove_dir(&p).unwrap();journal::replay_logs(&f.ctx.c.state_dir,&j.logger).unwrap();
    assert!(fs::read_to_string(&p).unwrap().contains("log unavailable"));
}
#[test] fn log_rotation_is_bounded(){
    let mut f=Fixture::new();f.ctx.c.log_max_bytes=128;let log=journal::Logger::new(&f.ctx.c);
    for _ in 0..20 {log.status("TEST","a".repeat(200));}
    assert!(f.ctx.c.state_dir.join("fim.log.5").exists());assert!(!f.ctx.c.state_dir.join("fim.log.6").exists());
}
#[test] fn cannot_run_two_monitors_with_same_state(){
    let f=Fixture::new();let p=f.ctx.c.state_dir.join("monitor.lock");let _first=secure::lock(&p).unwrap();
    assert!(secure::lock(&p).is_err());
}
#[test] fn publisher_refuses_damaged_page(){
    let f=Fixture::new();let p=f.ctx.c.root.join("index.html");fs::write(&p,"tampered").unwrap();
    assert!(publisher::page(&f.ctx).is_err());assert_eq!(fs::read_to_string(p).unwrap(),"tampered");
}
#[test] fn publisher_keeps_displaced_page_and_paired_receipt(){
    let f=Fixture::new();let today=Utc::now().with_timezone(&f.ctx.tz).date_naive();
    let yesterday=f.ctx.template.replace(daily::TOKEN,&daily::value(&f.ctx.names,today.pred_opt().unwrap()).unwrap());
    fs::write(f.ctx.c.root.join("index.html"),&yesterday).unwrap();publisher::page(&f.ctx).unwrap();
    assert!(f.issues().is_empty());
    assert!(fs::read_dir(&f.ctx.c.staging_dir).unwrap().filter_map(|e|e.ok())
        .any(|e|e.file_name().to_string_lossy().starts_with("verified-") && fs::read_to_string(e.path()).unwrap()==yesterday));
    let r:serde_json::Value=serde_json::from_slice(&fs::read(f.ctx.c.state_dir.join("publication.json")).unwrap()).unwrap();
    let e=Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both))).add_path(PathBuf::from(r["stage"].as_str().unwrap()))
        .add_path(f.ctx.c.root.join("index.html")).set_tracker(123);
    let authorized=publisher::authorized_trackers(&f.ctx,&[e.clone()]);assert!(authorized.contains(&123));
    let data=Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(f.ctx.c.root.join("index.html"));
    let (ordinary,expected)=monitor::classify(&[e,data],&authorized);assert_eq!(expected.len(),1);assert_eq!(ordinary.len(),1);
}
#[test] fn restored_mutation_remains_alertable(){
    let f=Fixture::new();assert!(f.issues().is_empty());
    let e=Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(f.ctx.c.root.join("style.css"));
    let (ordinary,expected)=monitor::classify(&[e],&BTreeSet::new());assert_eq!(ordinary.len(),1);assert!(expected.is_empty());
}
#[test] fn exchange_preserves_actual_old_inode(){
    let f=Fixture::new();let s=secure::directory(&f.ctx.c.staging_dir).unwrap();let r=secure::directory(&f.ctx.c.root).unwrap();
    fs::write(f.ctx.c.staging_dir.join("candidate"),"new").unwrap();fs::write(f.ctx.c.root.join("slot"),"actual old").unwrap();
    secure::rename(&s,Path::new("candidate"),&r,Path::new("slot"),libc::RENAME_EXCHANGE).unwrap();
    assert_eq!(fs::read_to_string(f.ctx.c.staging_dir.join("candidate")).unwrap(),"actual old");
    assert_eq!(fs::read_to_string(f.ctx.c.root.join("slot")).unwrap(),"new");
}
#[test] fn concurrent_symlink_swap_never_reads_outside_root(){
    let f=Fixture::new();let slot=f.ctx.c.root.join("slot");let stash=f.ctx.c.root.join("stash");
    let outside=f._tmp.path().join("secret");fs::write(&outside,"OUTSIDE").unwrap();fs::write(&slot,"inside").unwrap();
    let root=secure::directory(&f.ctx.c.root).unwrap();
    let worker=thread::spawn(move||{for _ in 0..300 {
        if fs::rename(&slot,&stash).is_ok() {let _=symlink(&outside,&slot);let _=fs::remove_file(&slot);let _=fs::rename(&stash,&slot);}
    }});
    for _ in 0..300 {if let Ok(mut fd)=secure::beneath(&root,Path::new("slot"),libc::O_RDONLY|libc::O_NONBLOCK) {
        let mut text=String::new();fd.read_to_string(&mut text).unwrap();assert_eq!(text,"inside");
    }} worker.join().unwrap();
}
#[test] fn unsafe_trust_file_is_rejected(){
    let f=Fixture::new();fs::set_permissions(&f.ctx.c.calendar,fs::Permissions::from_mode(0o666)).unwrap();
    assert!(secure::trusted(&f.ctx.c.calendar,false).is_err());
}
#[test] fn dst_fall_back_does_not_change_calendar_date(){
    let a=instant("2026-10-25T02:59:59+02:00");let b=instant("2026-10-25T02:00:01+01:00");
    let d=NaiveDate::from_ymd_opt(2026,10,25).unwrap();
    assert!(daily::normalize(&html(d),&names(),a,b,60).is_ok());
}
#[test] fn dst_spring_forward_does_not_change_calendar_date(){
    let a=instant("2026-03-29T01:59:59+01:00");let b=instant("2026-03-29T03:00:01+02:00");
    let d=NaiveDate::from_ymd_opt(2026,3,29).unwrap();
    assert!(daily::normalize(&html(d),&names(),a,b,60).is_ok());
}
