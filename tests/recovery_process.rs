//! The real binary: `run` marks itself running, pings the heartbeat, announces a restart
//! after an unclean exit, and clears its marker on SIGTERM.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Minimal HTTP server: answers 200 to everything and reports "METHOD /path".
fn server() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let mut length = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                    if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = std::io::Read::read_exact(&mut reader, &mut body);
                let mut out = stream;
                let _ = out.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{}");
                let request = line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
                let _ = tx.send(request);
            });
        }
    });
    (port, rx)
}

fn write_config(dir: &Path, port: u16) {
    let model = format!("{}/tests/fixtures/fake-model.onnx", env!("CARGO_MANIFEST_DIR"));
    let cfg = format!(
        r#"
printer: {{host: "127.0.0.1:1", password: x, auth: apikey, timeout_s: 1, poll_interval_s: 1}}
camera: {{url: "{missing}"}}
detector: {{model_path: "{model}", expected_sha256: ""}}
notify:
  ntfy: {{url: "http://127.0.0.1:{port}", topic: alerts}}
web: {{enabled: false}}
health:
  heartbeat_url: "http://127.0.0.1:{port}/hb"
  heartbeat_interval_s: 10
state_dir: "{state}"
escalation: {{default_policy: watch_only, schedules: []}}
"#,
        missing = dir.join("no-such-camera.avi").display(),
        state = dir.join("data").display(),
    );
    std::fs::write(dir.join("config.yaml"), cfg).unwrap();
}

fn start(dir: &Path) -> Child {
    let mut c = Command::new(env!("CARGO_BIN_EXE_prusa-watch"));
    c.current_dir(dir)
        .arg("run")
        .arg("--config")
        .arg(dir.join("config.yaml"));
    for (k, _) in std::env::vars() {
        if k.starts_with("PRUSA_WATCH") || k.to_ascii_lowercase().ends_with("_proxy") {
            c.env_remove(k);
        }
    }
    c.env("NO_PROXY", "127.0.0.1,localhost")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c.spawn().unwrap()
}

fn wait_for(rx: &mpsc::Receiver<String>, want: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(req) if req == want => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

fn sigterm(child: &mut Child) -> std::process::ExitStatus {
    Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "no exit after SIGTERM");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn run_heartbeats_announces_unclean_restarts_and_shuts_down_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = server();
    write_config(dir.path(), port);
    let running = dir.path().join("data").join("running.json");

    // First start: nothing to announce; an idle, unreachable printer is still "healthy".
    let mut child = start(dir.path());
    assert!(wait_for(&rx, "GET /hb", Duration::from_secs(30)), "no heartbeat");
    assert!(running.exists());
    let status = sigterm(&mut child);
    assert!(status.success(), "{status:?}");
    assert!(!running.exists(), "clean shutdown clears the marker");

    // Simulate a crash: the marker survives, so the next start announces the restart.
    std::fs::write(&running, "{}").unwrap();
    while rx.try_recv().is_ok() {}
    let mut child = start(dir.path());
    assert!(
        wait_for(&rx, "POST /alerts", Duration::from_secs(15)),
        "no restart notice"
    );
    let _ = sigterm(&mut child);
}
