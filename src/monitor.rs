use anyhow::{Context, Result};
use chrono::Utc;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind,AccessMode,ModifyKind}};
use std::{collections::BTreeSet, os::{unix::{net::UnixDatagram,fs::MetadataExt}, linux::net::SocketAddrExt},
    sync::{Arc,Mutex,mpsc::{self,Receiver},atomic::{AtomicBool,Ordering}},
    thread, time::{Duration,Instant}};
use crate::{add, alerts, baseline, config::ContextData, journal::{self,Journal},
    publisher, scan, secure, Issue, Issues};

pub static TERMINATE:AtomicBool=AtomicBool::new(false);
extern "C" fn terminate(_:libc::c_int) { TERMINATE.store(true,Ordering::Relaxed); }
pub fn install_signals() {
    // SAFETY: handler only performs a lock-free atomic store; no allocations/locks/I/O.
    unsafe { libc::signal(libc::SIGTERM,terminate as libc::sighandler_t);
        libc::signal(libc::SIGINT,terminate as libc::sighandler_t); }
}
fn sd_notify(message:&str) {
    let Ok(path)=std::env::var("NOTIFY_SOCKET") else {return;};
    let Ok(socket)=UnixDatagram::unbound() else {return;};
    if let Some(name)=path.strip_prefix('@') {
        if let Ok(addr)=std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()) {
            let _=socket.send_to_addr(message.as_bytes(),&addr);
        }
    } else { let _=socket.send_to(message.as_bytes(),path); }
}
pub struct EventWatch { _watcher:RecommendedWatcher, pub rx:Receiver<notify::Result<Event>>, pub lost:Arc<AtomicBool> }
pub fn mutation(event:&Event)->bool {
    !matches!(event.kind,EventKind::Access(_))
        || matches!(event.kind,EventKind::Access(AccessKind::Close(AccessMode::Write)))
}
impl EventWatch {
    pub fn new(ctx:&ContextData)->Result<Self> {
        let root=ctx.c.root.clone();
        let (tx,rx)=mpsc::sync_channel(4096);
        let lost=Arc::new(AtomicBool::new(false)); let flag=lost.clone();
        let mut watcher=RecommendedWatcher::new(move |result:notify::Result<Event>| {
            if let Ok(e)=&result {
                if e.need_rescan() { flag.store(true,Ordering::Relaxed); }
                if !mutation(e) || (!e.need_rescan() && !e.paths.iter().any(|p|p.starts_with(&root))) {return;}
            }
            if tx.try_send(result).is_err() { flag.store(true,Ordering::Relaxed); }
        },notify::Config::default().with_follow_symlinks(false))?;
        watcher.watch(&ctx.c.root,RecursiveMode::Recursive)?;
        watcher.watch(ctx.c.root.parent().context("No root parent")?,RecursiveMode::NonRecursive)?;
        // A protected staging directory allows correlating authorized publication rename cookies.
        watcher.watch(&ctx.c.staging_dir,RecursiveMode::NonRecursive)?;
        Ok(Self {_watcher:watcher,rx,lost})
    }
}
pub fn classify(events:&[Event],authorized:&BTreeSet<usize>)->(Vec<String>,Vec<String>) {
    let mut ordinary=Vec::new(); let mut expected=Vec::new();
    for e in events {
        let detail=format!("{:?} paths={:?} cookie={:?}",e.kind,e.paths,e.tracker());
        if matches!(e.kind,EventKind::Modify(ModifyKind::Name(_)))
            && e.tracker().is_some_and(|t|authorized.contains(&t)) { expected.push(detail); }
        else { ordinary.push(detail); }
    }
    (ordinary,expected)
}
pub fn run(ctx:ContextData,once:bool)->Result<i32> {
    let _lock=secure::lock(&ctx.c.state_dir.join("monitor.lock"))?;
    install_signals();
    let mut j=Journal::new(&ctx.c);
    j.emit("START",if once {"check"} else {"watch"},false);
    if ctx.demo { j.emit("DEMO_MODE","Ownership isolation disabled; not for server deployment",false); }
    let stop=Arc::new(AtomicBool::new(false));
    let mail_health:alerts::MailHealth=Arc::new(Mutex::new(j.active.values()
        .find(|i|i.kind=="MAIL_UNAVAILABLE").map(|i|i.detail.clone())));
    let worker=if once {None} else {Some(alerts::worker(ctx.clone(),j.logger.clone(),stop.clone(),mail_health.clone()))};
    let loaded=baseline::load(&ctx);
    let (base,base_error)=match loaded {Ok(b)=>(Some(b),None),Err(e)=>(None,Some(format!("{e:#}")))};
    let root_pin=secure::directory(&ctx.c.root).and_then(|f|Ok(f.metadata()?)).ok();
    let mut armed_id=root_pin.as_ref().map(|m|(m.dev(),m.ino()));
    let mut armed=if once {None} else {Some(EventWatch::new(&ctx))};
    let mut periodic=Instant::now();
    let mut debounce:Option<Instant>=None;
    let mut last_scan=Instant::now()-Duration::from_secs(1);
    let mut reminder=Instant::now()+Duration::from_secs(ctx.c.reminder_seconds);
    let mut watch_retry=Instant::now()+Duration::from_secs(ctx.c.interval_seconds);
    let mut events=Vec::new(); let mut event_loss=false;
    let mut watch_error:Option<String>=None;
    let mut ever_incident=false;
    sd_notify("READY=1\nSTATUS=FIM started");
    loop {
        if TERMINATE.load(Ordering::Relaxed) {break;}
        if let Some(result)=&armed {
            match result {
                Ok(w)=>{
                    if w.lost.swap(false,Ordering::Relaxed) {event_loss=true;}
                    for result in w.rx.try_iter().take(4096) {
                        match result {
                            Ok(e)=>{
                                if events.len()<4096 {events.push(e);} else {event_loss=true;}
                                debounce.get_or_insert(Instant::now()+Duration::from_millis(250));
                            }
                            Err(e)=>{watch_error=Some(e.to_string());event_loss=true;}
                        }
                    }
                }
                Err(e)=>watch_error=Some(format!("{e:#}")),
            }
        }
        let due=Instant::now()>=periodic || event_loss
            || debounce.is_some_and(|d|Instant::now()>=d);
        if due && last_scan.elapsed()>=Duration::from_millis(250) {
            let began=Instant::now();
            // Fixed start-to-start deadline; no full interval sleep AFTER the work.
            if began>=periodic {periodic=began+Duration::from_secs(ctx.c.interval_seconds);}
            last_scan=began; debounce=None;
            let snapshot=scan::run(&ctx);
            let mut issues=if let Some(b)=&base {scan::compare(&b.entries,&snapshot,&j.active)} else {
                let mut i=j.active.clone(); i.extend(snapshot.errors.clone()); i
            };
            if let Some(e)=&base_error {add(&mut issues,Issue::new("BASELINE_UNAVAILABLE",None,e.clone()));}
            for guard in ctx.guards.iter().chain(base.iter().map(|b|&b.guard)) {
                if let Err(e)=guard.verify(ctx.demo) {
                    add(&mut issues,Issue::new("TRUST_CHANGED",Some(&guard.path),format!("{e:#}")));
                }
            }
            let current_root=secure::directory(&ctx.c.root).and_then(|f|Ok(f.metadata()?)).ok();
            let current_id=current_root.as_ref().map(|m|(m.dev(),m.ino()));
            if root_pin.as_ref().map(|m|(m.dev(),m.ino())) != current_id {
                add(&mut issues,Issue::new("ROOT_IDENTITY_CHANGED",None,"Pinned website directory was replaced"));
            }
            if current_id != armed_id && !once {
                watch_error=Some("Root changed; rearming event coverage".into());
                watch_retry=Instant::now(); armed_id=current_id;
            }
            j.logger.status("HEARTBEAT",format!("{} entries; {} errors",snapshot.entries.len(),snapshot.errors.len()));
            if let Some(e)=j.logger.error() {add(&mut issues,Issue::new("LOG_IO",None,e));}
            if let Some(e)=&j.storage_error {add(&mut issues,Issue::new("STATE_IO",None,e.clone()));}
            if j.dropped>0 {add(&mut issues,Issue::new("AUDIT_LOSS",None,format!("{} events lost after persistence/RAM exhaustion",j.dropped)));}
            if let Ok(health)=mail_health.lock() {
                if let Some(e)=&*health {add(&mut issues,Issue::new("MAIL_UNAVAILABLE",None,e.clone()));}
            }
            if worker.as_ref().is_some_and(|w|w.is_finished()) {add(&mut issues,Issue::new("MAIL_WORKER_DOWN",None,"Mail worker stopped"));}
            if let Some(e)=&watch_error {add(&mut issues,Issue::new("EVENT_WATCH_DEGRADED",None,e.clone()));}
            if event_loss {j.emit("EVENT_LOSS","Event queue lost information; full rescan performed; transient history is unrecoverable",true);}
            let trackers=if event_loss {BTreeSet::new()} else {publisher::authorized_trackers(&ctx,&events)};
            let (ordinary,expected)=classify(&events,&trackers);
            // Every non-authorized mutation is auditable/alertable, including change-then-restore.
            for batch in ordinary.chunks(32) {j.emit("FS_ACTIVITY",batch.join("\n"),true);}
            for batch in expected.chunks(32) {j.emit("EXPECTED_PUBLICATION",batch.join("\n"),false);}
            events.clear(); event_loss=false;
            if began.elapsed()>Duration::from_secs(ctx.c.interval_seconds) {
                add(&mut issues,Issue::new("SCAN_OVERRUN",None,"Scan exceeded configured start-to-start interval"));
            }
            ever_incident|=!issues.is_empty() || !ordinary.is_empty();
            j.reconcile(issues);
            if Instant::now()>=reminder && !j.active.is_empty() {
                let example=j.active.values().take(32).collect::<Vec<_>>();
                j.emit("REMINDER",format!("{} active incidents (first 32): {}",j.active.len(),serde_json::to_string(&example)?),true);
                reminder=Instant::now()+Duration::from_secs(ctx.c.reminder_seconds);
            }
            let _=j.flush();
            sd_notify("WATCHDOG=1\nSTATUS=FIM scan completed");
            if once {break;}
        }
        if !once && watch_error.is_some() && Instant::now()>=watch_retry {
            armed=Some(EventWatch::new(&ctx));
            if armed.as_ref().is_some_and(|x|x.is_ok()) {watch_error=None;event_loss=true;}
            watch_retry=Instant::now()+Duration::from_secs(ctx.c.interval_seconds);
        }
        // Log/DB/mail failure never returns out of the watch loop.
        if j.pending()>0 || j.storage_error.is_some() {let _=j.flush();}
        let _=journal::replay_logs(&ctx.c.state_dir,&j.logger);
        thread::sleep(Duration::from_millis(100));
    }
    stop.store(true,Ordering::Relaxed);
    j.emit("STOP","Monitoring stopped",false);
    // Drain RAM to SQLite (not email). Abort persistence loop on failure, but remain explicit on stderr.
    while j.pending()>0 {
        if let Err(e)=j.flush() {eprintln!("FIM: unpersisted events at shutdown: {e:#}");ever_incident=true;break;}
    }
    if once {
        let mut sender=alerts::Sender::default();
        // One bounded delivery attempt. Remaining batches belong to watch/deliver, never silently discarded.
        if let Err(e)=alerts::deliver_one(&ctx,&j.logger,&mut sender) {eprintln!("FIM email: {e:#}");ever_incident=true;}
    }
    if let Some(worker)=worker {let _=worker.join();}
    Ok(if ever_incident {1} else {0})
}
