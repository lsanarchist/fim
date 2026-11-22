use anyhow::Result;
use fim::{alert, db, monitor, page, App};
use std::path::Path;

const USAGE: &str = "\
Usage: fim <config.json> <command>

Commands:
  init        create the baseline from a known-good site (never overwrites it)
  check       compare once; exit code 0 = clean, 1 = incident
  watch       compare every interval_seconds until stopped
  page        write today's date and name day into index.html
  alert-test  send a test message to Telegram";

fn run(config: &str, command: &str) -> Result<i32> {
    let app = App::load(Path::new(config))?;
    match command {
        "init" => monitor::init(&app)?,
        "check" => return monitor::check(&app),
        "watch" => return monitor::watch(&app),
        "page" => page::page(&app)?,
        "alert-test" => {
            let db = db::open(&app.cfg.database)?;
            let text = format!("FIM: testovacia správa\nČas: {}", app.log.now());
            db::queue_alert(&db, &text)?;
            alert::flush(&db, &app.log, &mut alert::telegram(&app.cfg))?;
            if !db::pending_alerts(&db)?.is_empty() {
                return Ok(1); // the reason is in the log line ALERT_FAILED
            }
        }
        _ => unreachable!(),
    }
    Ok(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let commands = ["init", "check", "watch", "page", "alert-test"];
    if args.len() != 2 || !commands.contains(&args[1].as_str()) {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let code = run(&args[0], &args[1]).unwrap_or_else(|e| {
        eprintln!("fim: error: {e:#}");
        2
    });
    std::process::exit(code);
}
