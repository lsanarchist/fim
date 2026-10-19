use anyhow::{bail, ensure, Result};
use fim::{
    alerts, baseline,
    config::{ContextData, MailContext},
    journal, monitor, publisher,
};
use std::path::Path;

const USAGE: &str = "Usage: fim [--demo] <config.json> <page|init|check|watch|alert-test|deliver>";

fn print_help() {
    println!(
        "FIM {} — file integrity monitoring for a static website\n\n\
         {USAGE}\n\n\
         Commands:\n\
           page        Publish the verified date and name-day page\n\
           init        Create a baseline from a known-good website (never overwrites)\n\
           check       Compare once and attempt one due email batch\n\
           watch       Monitor filesystem events and run periodic integrity scans\n\
           alert-test  Queue a test notification and attempt one due email batch\n\
           deliver     Attempt one due email batch without loading the baseline\n\n\
         Options:\n\
           --demo      Relax root ownership checks for a local classroom demo\n\
           -h, --help  Show this help without loading configuration\n\
           -V, --version  Show the application version\n\n\
         Relative configuration paths are resolved from the configuration directory.\n\
         First run: page, then init, then check or watch.\n\
         Check/watch exit codes: 0 clean, 1 incident or runtime failure, 2 startup error.\n\
         Use README.md for configuration, deployment and baseline migration details.",
        env!("CARGO_PKG_VERSION")
    );
}

fn start() -> Result<i32> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let demo = args.first().is_some_and(|x| x == "--demo");
    if demo {
        args.remove(0);
    }
    if args.len() == 1 {
        match args[0].as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(0);
            }
            "-V" | "--version" => {
                println!("fim {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            _ => {}
        }
    }
    ensure!(args.len() == 2, "{USAGE}\nRun fim --help for details.");
    ensure!(
        matches!(
            args[1].as_str(),
            "page" | "init" | "check" | "watch" | "alert-test" | "deliver"
        ),
        "Unknown command: {}\n{USAGE}",
        args[1]
    );
    if matches!(args[1].as_str(), "alert-test" | "deliver") {
        let ctx = MailContext::load(Path::new(&args[0]), demo)?;
        if args[1] == "alert-test" {
            journal::external(
                &ctx.c,
                "ALERT_TEST",
                "Test notification: verify actual receipt in the inbox".into(),
                true,
            )?;
        }
        alerts::deliver_mail(
            &ctx,
            &journal::Logger::new(&ctx.c),
            &mut alerts::Sender::default(),
        )?;
        if args[1] == "alert-test" {
            println!("Test queued. A busy worker or earlier due message may be processed first; verify the inbox.");
        } else {
            println!(
                "Pending batches: {}",
                journal::queue_size(&ctx.c.state_dir)?
            );
        }
        return Ok(0);
    }
    let ctx = ContextData::load(Path::new(&args[0]), demo)?;
    match args[1].as_str() {
        "page" => publisher::page(&ctx)?,
        "init" => {
            baseline::create(&ctx)?;
            println!("Baseline created. Monitoring never changes it automatically.");
        }
        "check" => return monitor::run(ctx, true),
        "watch" => return monitor::run(ctx, false),
        _ => bail!("Unknown command"),
    }
    Ok(0)
}
fn main() {
    let code = match start() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("FIM_FATAL: {e:#}");
            2
        }
    };
    std::process::exit(code);
}
