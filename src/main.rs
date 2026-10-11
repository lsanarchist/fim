use anyhow::{bail,ensure,Result};
use std::path::Path;
use fim::{alerts,baseline,config::ContextData,journal,monitor,publisher};
fn start()->Result<i32> {
    let mut args:Vec<String>=std::env::args().skip(1).collect();
    let demo=args.first().is_some_and(|x|x=="--demo");
    if demo {args.remove(0);}
    ensure!(args.len()==2,"Usage: fim [--demo] config.json <page|init|check|watch|alert-test|deliver>");
    let ctx=ContextData::load(Path::new(&args[0]),demo)?;
    match args[1].as_str() {
        "page"=>publisher::page(&ctx)?,
        "init"=>{
            baseline::create(&ctx)?;
            println!("Baseline created. Monitoring never changes it automatically.");
        }
        "check"=>return monitor::run(ctx,true),
        "watch"=>return monitor::run(ctx,false),
        "alert-test"=>{
            journal::external(&ctx.c,"ALERT_TEST","Test notification: verify actual receipt in the inbox".into(),true)?;
            let mut sender=alerts::Sender::default();
            alerts::deliver_one(&ctx,&journal::Logger::new(&ctx.c),&mut sender)?;
            println!("Test queued. A busy worker or earlier due message may be processed first; verify the inbox.");
        }
        "deliver"=>{
            let mut sender=alerts::Sender::default();
            alerts::deliver_one(&ctx,&journal::Logger::new(&ctx.c),&mut sender)?;
            println!("Pending batches: {}",journal::queue_size(&ctx.c.state_dir)?);
        }
        _=>bail!("Unknown command"),
    }
    Ok(0)
}
fn main() {
    let code=match start() {Ok(c)=>c,Err(e)=>{eprintln!("FIM_FATAL: {e:#}");2}};
    std::process::exit(code);
}
