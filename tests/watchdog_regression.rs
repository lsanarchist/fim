use fim::{baseline, config::ContextData, publisher};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct MonitorChild(Child);
impl Drop for MonitorChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn long_scan_interval_keeps_systemd_watchdog_alive_while_idle() {
    let temp = tempfile::tempdir().unwrap();
    let p = temp.path();
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
    fs::write(&mta, "#!/bin/sh\ncat >/dev/null\n").unwrap();
    fs::set_permissions(&mta, fs::Permissions::from_mode(0o700)).unwrap();
    let mut config: serde_json::Value =
        serde_json::from_str(include_str!("../config.json")).unwrap();
    config["sendmail"] = serde_json::json!(mta);
    config["interval_seconds"] = serde_json::json!(300);
    fs::write(p.join("config.json"), serde_json::to_vec(&config).unwrap()).unwrap();
    let ctx = ContextData::load(&p.join("config.json"), true).unwrap();
    publisher::page(&ctx).unwrap();
    baseline::create(&ctx).unwrap();

    let socket_path = p.join("notify.sock");
    let socket = UnixDatagram::bind(&socket_path).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut child = MonitorChild(
        Command::new(env!("CARGO_BIN_EXE_fim"))
            .args(["--demo", p.join("config.json").to_str().unwrap(), "watch"])
            .env("NOTIFY_SOCKET", &socket_path)
            .env("WATCHDOG_USEC", "1000000")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );

    let mut buffer = [0u8; 4096];
    let startup_deadline = Instant::now() + Duration::from_secs(5);
    let initial_scan = loop {
        assert!(
            Instant::now() < startup_deadline,
            "monitor never completed its first scan"
        );
        let size = socket.recv(&mut buffer).expect("initial scan notification");
        let message = std::str::from_utf8(&buffer[..size]).unwrap();
        if message.contains("WATCHDOG=1") && message.contains("scan completed") {
            break Instant::now();
        }
    };
    let size = socket
        .recv(&mut buffer)
        .expect("watchdog ping during the 300-second idle interval");
    assert_eq!(std::str::from_utf8(&buffer[..size]).unwrap(), "WATCHDOG=1");
    assert!(initial_scan.elapsed() < Duration::from_secs(2));
    assert!(child.0.try_wait().unwrap().is_none());
    let log = fs::read_to_string(p.join("state/fim.log")).unwrap();
    let scan_count = log
        .lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"] == "HEARTBEAT"
        })
        .count();
    assert_eq!(
        scan_count, 1,
        "the second ping must occur while idle, without another scan"
    );

    // SAFETY: this is the live child owned by the test; SIGTERM exercises graceful shutdown.
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "monitor did not stop after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
