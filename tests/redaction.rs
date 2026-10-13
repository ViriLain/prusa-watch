//! Diagnostic output never contains credentials. Every secret below is synthetic and
//! distinctive, so a single occurrence anywhere in an export fails the test.
mod common;

use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use prusa_watch::config::{Config, effective_config, load_config};
use prusa_watch::redact::Redactor;

/// Fragments of the synthetic secrets. None may appear in anything exported.
const SECRETS: [&str; 14] = [
    "SYNTH-PRUSALINK-PASS",
    "SYNTH-PRINTER-URL-PASS",
    "printeruser-synth",
    "SYNTH-CAMERA-PASS",
    "camuser-synth",
    "SYNTH-CAMERA-KEY",
    "SYNTH-NTFY-URL-PASS",
    "synth-alert-topic-1a2b3c",
    "tk_SYNTH_ntfy_token",
    "synth-reply-topic-4d5e6f",
    "SYNTH-DISCORD-TOKEN",
    "SYNTH-HOOK-ID",
    "SYNTH-HOOK-QUERY",
    "SYNTH-WEB-TOKEN",
];

fn assert_clean(what: &str, text: &str) {
    for secret in SECRETS {
        assert!(
            !text.contains(secret),
            "{what} exposes {secret}:\n{}",
            text.lines()
                .filter(|line| line.contains(secret))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// A port nothing listens on: connections are refused at once.
fn dead_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The model sits in a directory named like the dashboard token. Nothing treats a model
/// path as secret, so it reaches the log unredacted by any call site: only the log writer
/// (and `check`'s own output path) stands between it and the terminal.
fn model_under_a_secret_name(dir: &Path) -> String {
    let folder = dir.join("SYNTH-WEB-TOKEN");
    std::fs::create_dir_all(&folder).unwrap();
    let model = folder.join("model.onnx");
    std::fs::copy(fake_model_path(), &model).unwrap();
    model.to_string_lossy().into_owned()
}

fn config_yaml(dir: &Path, printer_port: u16, camera_port: u16, web_port: u16) -> String {
    format!(
        r#"
printer:
  host: "http://printeruser-synth:SYNTH-PRINTER-URL-PASS@127.0.0.1:{printer_port}"
  password: SYNTH-PRUSALINK-PASS
  timeout_s: 1
  poll_interval_s: 0.2
camera:
  url: "rtsp://camuser-synth:SYNTH-CAMERA-PASS@127.0.0.1:{camera_port}/live?key=SYNTH-CAMERA-KEY"
  open_timeout_s: 0.5
  read_timeout_s: 0.5
  reconnect_backoff_s: 0.1
detector:
  model_path: "{model}"
  expected_sha256: ""
notify:
  ntfy:
    url: "http://ntfyuser:SYNTH-NTFY-URL-PASS@127.0.0.1:{printer_port}"
    topic: synth-alert-topic-1a2b3c
    token: tk_SYNTH_ntfy_token
    reply_topic: synth-reply-topic-4d5e6f
    reply_reconnect_s: 0.1
  discord:
    webhook_url: "https://discord.invalid/api/webhooks/123456789012/SYNTH-DISCORD-TOKEN"
  webhook:
    url: "https://ha.invalid/api/webhook/SYNTH-HOOK-ID?secret=SYNTH-HOOK-QUERY"
  timeout_s: 1
web:
  port: {web_port}
  token: SYNTH-WEB-TOKEN
  public_url: "http://192.168.1.10:8484"
escalation:
  default_policy: watch_only
  schedules: []
  policies:
    watch_only:
      steps:
        - {{at: 0, message: "see https://ntfy.sh/synth-alert-topic-1a2b3c"}}
state_dir: "{state}"
log_level: DEBUG
"#,
        model = model_under_a_secret_name(dir),
        state = dir.join("data").display(),
    )
}

fn synthetic_config(dir: &Path) -> Config {
    let path = dir.join("config.yaml");
    std::fs::write(&path, config_yaml(dir, 9, 9, 8484)).unwrap();
    let cfg = load_config(Some(&path), &Default::default()).unwrap();
    cfg.validate().unwrap();
    cfg
}

#[test]
fn the_test_would_notice_a_leak() {
    // The unredacted configuration contains every secret, so the checks below are not vacuous.
    let dir = tempfile::tempdir().unwrap();
    let cfg = synthetic_config(dir.path());
    let raw = serde_yaml::to_string(&effective_config(&cfg, false)).unwrap();
    for secret in SECRETS {
        assert!(raw.contains(secret), "{secret} is not in the synthetic config");
    }
}

#[test]
fn effective_config_masks_every_secret_and_keeps_what_helps() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = synthetic_config(dir.path());
    let shown = effective_config(&cfg, true);
    assert_clean("effective config", &serde_yaml::to_string(&shown).unwrap());
    // still a useful diagnostic: hosts, paths and non-secret settings are readable
    assert_eq!(shown["camera"]["url"], "rtsp://***@127.0.0.1:9/live?***");
    assert_eq!(shown["printer"]["host"], "http://***@127.0.0.1:9");
    assert_eq!(shown["notify"]["ntfy"]["url"], "http://***@127.0.0.1:9");
    assert_eq!(shown["notify"]["ntfy"]["topic"], "***");
    assert_eq!(shown["notify"]["discord"]["webhook_url"], "***");
    assert_eq!(shown["web"]["public_url"], "http://192.168.1.10:8484");
    assert_eq!(shown["printer"]["name"], "core-one");
    // a secret copied into a free-text setting is masked there too
    let step = &shown["escalation"]["policies"]["watch_only"]["steps"][0]["message"];
    assert_eq!(step, "see https://ntfy.sh/***");
}

#[test]
fn free_text_from_transports_and_ffmpeg_is_scrubbed() {
    let dir = tempfile::tempdir().unwrap();
    let redactor = Redactor::from_config(&synthetic_config(dir.path()));
    for line in [
        "error sending request for url (https://ntfy.sh/synth-reply-topic-4d5e6f/json?since=1): connection refused",
        "POST https://discord.invalid/api/webhooks/123456789012/SYNTH-DISCORD-TOKEN: HTTP 404",
        "[rtsp @ 0x5581] rtsp://camuser-synth:SYNTH-CAMERA-PASS@10.0.0.9:554/live?key=SYNTH-CAMERA-KEY: Connection refused",
        "ffmpeg -i rtsp://camuser-synth:SYNTH-CAMERA-PASS@10.0.0.9/live -an -",
        "401 from server, sent Authorization: Bearer tk_SYNTH_ntfy_token",
        "X-Api-Key: SYNTH-PRUSALINK-PASS rejected",
        "HTTP 403 {\"error\":\"forbidden\",\"topic\":\"synth-alert-topic-1a2b3c\"}",
        "GET http://192.168.1.10:8484/api/state?token=SYNTH-WEB-TOKEN",
        "hook https://ha.invalid/api/webhook/SYNTH-HOOK-ID?secret=SYNTH-HOOK-QUERY failed",
        "password SYNTH-PRUSALINK-PASS in the middle of a sentence",
        "topicsynth-alert-topic-1a2b3cglued",
    ] {
        let clean = redactor.text(line);
        assert_clean(line, &clean);
        assert!(clean.contains("***"), "{line} -> {clean}");
    }
    // Text without secrets is left alone.
    let plain = "GET /api/v1/status: HTTP 503 Service Unavailable (attempt 2 of 3)";
    assert_eq!(redactor.text(plain), plain);
}

// ------------------------------------------------------------------ what the monitor exports

#[test]
fn state_notifications_and_api_errors_carry_no_secret() {
    let rig = rig_with(|cfg| {
        cfg.escalation = serde_yaml::from_str("{default_policy: ask_first, schedules: []}").unwrap();
        cfg.printer.host = "http://printeruser-synth:SYNTH-PRINTER-URL-PASS@printer.lan".into();
        cfg.printer.password = "SYNTH-PRUSALINK-PASS".into();
        cfg.camera.url = "rtsp://camuser-synth:SYNTH-CAMERA-PASS@cam.lan/live?key=SYNTH-CAMERA-KEY".into();
        cfg.notify.ntfy.token = "tk_SYNTH_ntfy_token".into();
    });
    rig.printer.set("PRINTING", Some(7));
    rig.advance(60);
    rig.grabber.set_image(Some(solid(255)));
    let id = loop {
        rig.advance(1);
        if let Some(id) = rig.mon.incident_id() {
            break id;
        }
    };

    // A transport error that quotes everything a client library might quote.
    let leaky = "GET /api/v1/status: error sending request for url \
                 (http://printeruser-synth:SYNTH-PRINTER-URL-PASS@printer.lan/api/v1/status): \
                 X-Api-Key: SYNTH-PRUSALINK-PASS, camera rtsp://camuser-synth:SYNTH-CAMERA-PASS@cam.lan/live?key=SYNTH-CAMERA-KEY";
    rig.printer.s().status_error = Some(leaky.into());
    rig.transport.clear();

    // the button reply, the watch loop and the alert all see that error (the dashboard's
    // HTTP error body is covered in tests/web.rs)
    let reply = rig.mon.handle_reply("act", &id);
    assert!(reply.contains("failed"), "{reply}");
    assert_clean("reply result", &reply);
    rig.advance(3);

    let snapshot = rig.mon.snapshot();
    let state = serde_json::to_string(&snapshot).unwrap();
    assert!(snapshot.printer_error.is_some() && snapshot.action_error.is_some());
    assert_clean("/api/state snapshot", &state);
    assert!(state.contains("http://***@printer.lan/***"), "{state}");
    assert_clean("readiness", &rig.mon.readiness().unwrap_err());

    let sent = rig.sent();
    assert!(
        sent.iter().any(|request| header(request, "Title").contains("FAILED")),
        "expected the failed-action alert: {:?}",
        rig.titles()
    );
    for request in &sent {
        for (name, value) in &request.headers {
            if name.eq_ignore_ascii_case("authorization") {
                continue; // prusa-watch's own credential for the publish
            }
            assert_clean(&format!("notification header {name}"), value);
        }
        assert_clean("notification body", &String::from_utf8_lossy(&body_bytes(request)));
    }
}

// ------------------------------------------------------------------ the shipped binary

fn cli(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prusa-watch"));
    command.current_dir(cwd);
    for (name, _) in std::env::vars() {
        if name.starts_with("PRUSA_WATCH") || name.to_lowercase().ends_with("_proxy") {
            command.env_remove(name);
        }
    }
    command.env("NO_PROXY", "*");
    command
}

/// Stands in for ffmpeg: fails the way ffmpeg does, quoting its whole command line
/// (and with it the camera URL) on stderr.
#[cfg(unix)]
fn leaky_ffmpeg(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("ffmpeg-stub");
    std::fs::write(
        &path,
        "#!/bin/sh\necho \"[rtsp @ 0x1] $*: Connection refused\" >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn config_and_check_output_contain_no_secret() {
    let dir = tempfile::tempdir().unwrap();
    let dead = dead_port();
    std::fs::write(
        dir.path().join("config.yaml"),
        config_yaml(dir.path(), dead, dead, 8484),
    )
    .unwrap();

    let config = cli(dir.path()).arg("config").output().unwrap();
    assert!(config.status.success(), "{config:?}");
    let shown = String::from_utf8_lossy(&config.stdout).to_string() + &String::from_utf8_lossy(&config.stderr);
    assert_clean("prusa-watch config", &shown);
    assert!(shown.contains("rtsp://***@127.0.0.1"), "{shown}");

    let check = cli(dir.path())
        .arg("check")
        .env("PRUSA_WATCH_FFMPEG", leaky_ffmpeg(dir.path()))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&check.stdout);
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert_eq!(check.status.code(), Some(1), "{stdout}\n{stderr}"); // nothing is reachable
    assert_clean("prusa-watch check stdout", &stdout);
    assert_clean("prusa-watch check stderr", &stderr);
    // the output is still the diagnostic it is meant to be
    assert!(stdout.contains("[printer] http://***@127.0.0.1"), "{stdout}");
    assert!(stdout.contains("[camera] rtsp://***@127.0.0.1"), "{stdout}");
    assert!(stdout.contains("FAIL"), "{stdout}");
    // and the leak path was really exercised: ffmpeg's line, URL and all, reached the log
    assert!(
        stderr.contains("Connection refused") && stderr.contains("rtsp://***@127.0.0.1"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn a_running_monitor_logs_and_serves_no_secret() {
    let dir = tempfile::tempdir().unwrap();
    let dead = dead_port();
    let web_port = dead_port();
    std::fs::write(
        dir.path().join("config.yaml"),
        config_yaml(dir.path(), dead, dead, web_port),
    )
    .unwrap();
    let mut child = cli(dir.path())
        .arg("run")
        .env("PRUSA_WATCH_FFMPEG", leaky_ffmpeg(dir.path()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Wait until the loop has failed to reach the printer, the camera and the reply topic.
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let get = |path: &str| {
        http.get(format!("http://127.0.0.1:{web_port}{path}"))
            .header("X-Token", "SYNTH-WEB-TOKEN")
            .send()
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let state = loop {
        let state = get("/api/state")
            .ok()
            .and_then(|response| response.text().ok())
            .unwrap_or_default();
        if state.contains("\"printer_error\":\"") {
            break state;
        }
        assert!(Instant::now() < deadline, "monitor never reported the printer error");
        std::thread::sleep(Duration::from_millis(100));
    };
    std::thread::sleep(Duration::from_millis(1500)); // several camera and reply-stream retries
    let metrics = get("/metrics").unwrap().text().unwrap();
    let health = get("/healthz").unwrap().text().unwrap();
    child.kill().unwrap();
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    let mut stdout = String::new();
    child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    child.wait().unwrap();

    assert_clean("/api/state", &state);
    assert_clean("/metrics", &metrics);
    assert_clean("/healthz", &health);
    assert_clean("log output", &stderr);
    assert_clean("stdout", &stdout);
    // each leak path produced a (clean) line; the last one is redacted by the log writer alone
    for expected in [
        "printer unreachable",
        "Camera: could not open",
        "Replies:",
        "Detector: loaded",
        "/***/model.onnx",
    ] {
        assert!(stderr.contains(expected), "no {expected:?} line in:\n{stderr}");
    }
}

/// Credentials written the ways people actually write them: a bare `user:pass@host`, a
/// password with URL-special characters, vendor-style `key=value` paths, and a secret
/// pasted into the wrong setting so that validation quotes it back.
#[cfg(unix)]
#[test]
fn awkward_credential_placements_stay_hidden() {
    const AWKWARD: [&str; 6] = [
        "HostPass-Q7",
        "hostuser-synth",
        "Sharp-Secret-77",
        "abc#Sharp",
        "PathPass-55",
        "misplaced-topic-9f8e7d",
    ];
    let hidden = |what: &str, text: &str| {
        for secret in AWKWARD {
            assert!(!text.contains(secret), "{what} exposes {secret}:\n{text}");
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let dead = dead_port();
    let config = |camera: &str, channels: &str| {
        format!(
            r#"
printer: {{host: "hostuser-synth:HostPass-Q7@127.0.0.1:{dead}", password: x, timeout_s: 1}}
camera: {{url: "{camera}", open_timeout_s: 0.5, read_timeout_s: 0.5, reconnect_backoff_s: 0.1}}
detector: {{model_path: "{model}", expected_sha256: ""}}
notify: {{ntfy: {{topic: misplaced-topic-9f8e7d}}, warning: {{channels: {channels}}}}}
state_dir: "{state}"
"#,
            model = fake_model_path(),
            state = dir.path().join("data").display(),
        )
    };
    let run = |arguments: &[&str], yaml: String| {
        std::fs::write(dir.path().join("config.yaml"), yaml).unwrap();
        let output = cli(dir.path())
            .args(arguments)
            .env("PRUSA_WATCH_FFMPEG", leaky_ffmpeg(dir.path()))
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).to_string() + &String::from_utf8_lossy(&output.stderr)
    };

    for camera in [
        format!("rtsp://camadmin:abc#Sharp-Secret-77@127.0.0.1:{dead}/live"),
        format!("rtsp://127.0.0.1:{dead}/user=camadmin&password=PathPass-55&channel=1"),
    ] {
        let shown = run(&["config"], config(&camera, "[ntfy]"));
        hidden("config", &shown);
        assert!(
            shown.contains("host: '***@127.0.0.1:") || shown.contains("host: \"***@127.0.0.1:"),
            "{shown}"
        );
        let checked = run(&["check"], config(&camera, "[ntfy]"));
        hidden("check", &checked);
        assert!(checked.contains("[printer] ***@127.0.0.1:"), "{checked}");
        assert!(checked.contains("Connection refused"), "{checked}");
    }

    // The topic also pasted where a channel name belongs: validation quotes it back.
    let camera = format!("rtsp://127.0.0.1:{dead}/live");
    for command in ["config", "check", "run"] {
        let refused = run(&[command], config(&camera, "[misplaced-topic-9f8e7d]"));
        assert!(refused.contains("notify.warning.channels"), "{command}: {refused}");
        hidden(command, &refused);
    }
}
