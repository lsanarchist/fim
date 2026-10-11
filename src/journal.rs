//! File logging is best-effort, with a durable SQLite audit/outbox fallback.
//! Incident state is independent of whether email submission succeeded.
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, fs, io::Write, path::{Path, PathBuf},
    sync::{Arc, Mutex}, time::Duration};
use crate::{config::Config, secure, Issues};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Audit {
    pub id: String, pub time: String, pub kind: String, pub detail: String, pub alert: bool,
}
#[derive(Clone)]
pub struct Logger { inner: Arc<Mutex<LogInner>> }
struct LogInner { dir: PathBuf, max: u64, error: Option<String> }
impl Logger {
    pub fn new(c: &Config) -> Self {
        Self { inner: Arc::new(Mutex::new(LogInner { dir:c.state_dir.clone(), max:c.log_max_bytes, error:None })) }
    }
    pub fn error(&self) -> Option<String> {
        match self.inner.lock() { Ok(x) => x.error.clone(), Err(_) => Some("Logger lock poisoned".into()) }
    }
    pub fn write(&self, audit: &Audit) -> bool {
        let Ok(mut inner) = self.inner.lock() else { eprintln!("FIM: logger lock poisoned"); return false; };
        let result = (|| -> Result<()> {
            secure::private_dir(&inner.dir)?;
            let path = inner.dir.join("fim.log");
            let mut file = secure::runtime_file(&path)?;
            if file.metadata()?.len() >= inner.max {
                drop(file);
                for n in (1..=4).rev() {
                    let src = inner.dir.join(format!("fim.log.{n}"));
                    if src.try_exists()? { fs::rename(src, inner.dir.join(format!("fim.log.{}", n+1)))?; }
                }
                fs::rename(&path, inner.dir.join("fim.log.1"))?;
                file = secure::runtime_file(&path)?;
            }
            writeln!(file, "{}", serde_json::to_string(audit)?)?;
            file.sync_data()?;
            Ok(())
        })();
        match result {
            Ok(()) => { inner.error=None; true }
            Err(e) => {
                inner.error=Some(format!("{e:#}"));
                eprintln!("FIM_LOG_FALLBACK {}", serde_json::to_string(audit).unwrap_or_default());
                eprintln!("FIM_LOG_ERROR {e:#}"); false
            }
        }
    }
    pub fn status(&self, kind:&str, detail:impl Into<String>) {
        self.write(&Audit { id:format!("status-{}",Utc::now().timestamp_nanos_opt().unwrap_or_default()),
            time:Utc::now().to_rfc3339(),kind:kind.into(),detail:detail.into(),alert:false });
    }
}
pub fn open_db(dir: &Path) -> Result<Connection> {
    secure::private_dir(dir)?;
    let path = dir.join("state.db");
    // Private directory prevents an untrusted user racing SQLite's path open.
    let _reservation = secure::runtime_file(&path)?;
    let db = Connection::open(&path)?;
    db.busy_timeout(Duration::from_millis(200))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS active(id INTEGER PRIMARY KEY CHECK(id=1), json TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS audit(id TEXT PRIMARY KEY, json TEXT NOT NULL, logged INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS outbox(id TEXT PRIMARY KEY, body TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0, next_attempt INTEGER NOT NULL DEFAULT 0,
            last_error TEXT NOT NULL DEFAULT '', submitted_at INTEGER);
        CREATE INDEX IF NOT EXISTS outbox_due ON outbox(submitted_at,next_attempt);")?;
    Ok(db)
}
pub struct Journal {
    pub active: Issues, pub logger: Logger, pub storage_error: Option<String>,
    pub dropped: u64, c: Config, pending: VecDeque<(Audit,bool)>, seq:u64, boot:String,
}
impl Journal {
    pub fn new(c:&Config) -> Self {
        let logger=Logger::new(c);
        let loaded=(|| -> Result<Issues> {
            let db=open_db(&c.state_dir)?;
            let json:Option<String>=db.query_row("SELECT json FROM active WHERE id=1",[],|r|r.get(0)).optional()?;
            Ok(match json { Some(s)=>serde_json::from_str(&s)?,None=>Issues::new() })
        })();
        let (active,storage_error)=match loaded { Ok(a)=>(a,None),Err(e)=>(Issues::new(),Some(format!("{e:#}"))) };
        Self { active,logger,storage_error,dropped:0,c:c.clone(),pending:VecDeque::new(),seq:0,
            boot:format!("{}-{}",Utc::now().timestamp_nanos_opt().unwrap_or_default(),std::process::id()) }
    }
    pub fn emit(&mut self, kind:&str, detail:impl Into<String>, alert:bool) {
        self.seq+=1;
        let event=Audit { id:format!("{}-{}",self.boot,self.seq),time:Utc::now().to_rfc3339(),
            kind:kind.into(),detail:detail.into(),alert };
        let logged=self.logger.write(&event);
        if self.pending.len()>=4096 {
            self.pending.pop_front(); self.dropped+=1;
            eprintln!("FIM_CRITICAL: persistence unavailable and RAM journal full; {} events dropped",self.dropped);
        }
        self.pending.push_back((event,logged));
    }
    pub fn reconcile(&mut self, next:Issues) {
        let mut changes=Vec::new();
        for (key,issue) in &next {
            if self.active.get(key)!=Some(issue) {
                changes.push(("INCIDENT",serde_json::to_string(issue).unwrap_or_else(|_|format!("{issue:?}"))));
            }
        }
        for (key,issue) in &self.active {
            if !next.contains_key(key) {
                changes.push(("RECOVERED",serde_json::to_string(issue).unwrap_or_else(|_|format!("{issue:?}"))));
            }
        }
        // Set active state regardless of mail success; no last_sent/incident conflation.
        self.active=next;
        for (kind,detail) in changes { self.emit(kind,detail,true); }
    }
    pub fn flush(&mut self) -> Result<()> {
        let result=(|| -> Result<()> {
            let mut db=open_db(&self.c.state_dir)?;
            // Bound a transaction and email body. Additional batches stay queued for later ticks.
            let n=self.pending.len().min(32);
            let batch:Vec<_>=self.pending.iter().take(n).cloned().collect();
            let tx=db.transaction()?;
            let mut alerts=Vec::new();
            for (event,logged) in &batch {
                tx.execute("INSERT OR IGNORE INTO audit VALUES(?1,?2,?3)",
                    params![event.id,serde_json::to_string(event)?,if *logged {1} else {0}])?;
                if event.alert { alerts.push(event); }
            }
            if !alerts.is_empty() {
                let id=&alerts[0].id;
                tx.execute("INSERT OR IGNORE INTO outbox(id,body) VALUES(?1,?2)",
                    params![id,serde_json::to_string_pretty(&alerts)?])?;
            }
            if n == self.pending.len() {
                tx.execute("INSERT INTO active VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
                    [serde_json::to_string(&self.active)?])?;
            }
            // Retain pending mail and unlogged audit records, prune only delivered/duplicated history.
            tx.execute("DELETE FROM outbox WHERE submitted_at IS NOT NULL AND submitted_at<?1",
                [Utc::now().timestamp()-7*86400])?;
            tx.execute("DELETE FROM audit WHERE logged=1 AND rowid < (SELECT MAX(rowid)-10000 FROM audit)",[])?;
            tx.commit()?;
            for _ in 0..n { self.pending.pop_front(); }
            Ok(())
        })();
        match &result { Ok(())=>self.storage_error=None,Err(e)=>self.storage_error=Some(format!("{e:#}")) }
        result
    }
    pub fn pending(&self)->usize { self.pending.len() }
}
pub fn replay_logs(dir:&Path, logger:&Logger)->Result<()> {
    let db=open_db(dir)?;
    let mut stmt=db.prepare("SELECT id,json FROM audit WHERE logged=0 ORDER BY rowid LIMIT 32")?;
    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (id,json) in rows {
        let event:Audit=serde_json::from_str(&json).context("Corrupt audit record")?;
        if !logger.write(&event) { break; }
        db.execute("UPDATE audit SET logged=1 WHERE id=?1",[id])?;
    }
    Ok(())
}
pub fn queue_size(dir:&Path)->Result<i64> {
    Ok(open_db(dir)?.query_row("SELECT count(*) FROM outbox WHERE submitted_at IS NULL",[],|r|r.get(0))?)
}

/// Publisher/alert-test must not overwrite the monitor's active-state snapshot.
pub fn external(c:&Config, kind:&str, detail:String, alert:bool)->Result<()> {
    let event=Audit { id:format!("external-{}-{}",Utc::now().timestamp_nanos_opt().unwrap_or_default(),std::process::id()),
        time:Utc::now().to_rfc3339(),kind:kind.into(),detail,alert };
    let logged=Logger::new(c).write(&event);
    let mut db=open_db(&c.state_dir)?;
    let tx=db.transaction()?;
    let json=serde_json::to_string(&event)?;
    tx.execute("INSERT INTO audit VALUES(?1,?2,?3)",params![event.id,json,if logged {1} else {0}])?;
    if alert { tx.execute("INSERT INTO outbox(id,body) VALUES(?1,?2)",params![event.id,json])?; }
    tx.commit()?;
    Ok(())
}
