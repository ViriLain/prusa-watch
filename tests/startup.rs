//! Port of tests/test_startup.py: .env loading, model download, CLI wiring.
//!
//! Adaptations:
//!   - httpx.MockTransport -> a fake `Fetch` for canned 200 bodies; the HTTP-status
//!     case runs the real `HttpFetch` against a one-shot local HTTP server.
//!   - `cli.main([...])` -> the built binary via std::process::Command (isolated
//!     cwd, PRUSA_WATCH_* removed).
//!   - Python monkeypatched `model.download_model` for the fetch-model / _ensure_model
//!     tests. A binary can't be monkeypatched, so those point all proxies at a dead
//!     local port: any download attempt fails fast (and never reaches the network),
//!     which shows whether/where the CLI tried to download.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use prusa_watch::dotenv::{load_dotenv, parse_dotenv};
use prusa_watch::model::{DEFAULT_URL, Fetch, HttpFetch, ModelDownloadError, download_model};

// ---------------------------------------------------------------- .env parsing

#[test]
fn parse_dotenv_subset() {
    let text = r#"
# comment
PRUSALINK_PASSWORD=abc123
export NTFY_TOPIC = my-topic   # trailing comment
QUOTED="has # hash and spaces"
SINGLE='x=y'
URL=http://192.168.1.10:8484
EMPTY=
not a line
"#;
    let expected: BTreeMap<String, String> = [
        ("PRUSALINK_PASSWORD", "abc123"),
        ("NTFY_TOPIC", "my-topic"),
        ("QUOTED", "has # hash and spaces"),
        ("SINGLE", "x=y"),
        ("URL", "http://192.168.1.10:8484"),
        ("EMPTY", ""),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(parse_dotenv(text), expected);
}

#[test]
fn load_dotenv_shell_wins_empty_skipped_first_file_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.env");
    let b = tmp.path().join("b.env");
    std::fs::write(&a, "ONE=from-a\nTWO=from-a\nEMPTY=\n").unwrap();
    std::fs::write(&b, "TWO=from-b\nTHREE=from-b\n").unwrap();
    let mut env: BTreeMap<String, String> = [("ONE".to_string(), "from-shell".to_string())].into();
    let loaded = load_dotenv(&[a.clone(), b.clone(), tmp.path().join("missing.env"), a.clone()], &mut env);
    assert_eq!(loaded, vec![a.canonicalize().unwrap(), b.canonicalize().unwrap()]);
    let expected: BTreeMap<String, String> = [("ONE", "from-shell"), ("TWO", "from-a"), ("THREE", "from-b")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(env, expected);
}

// ---------------------------------------------------------------- model download

/// Canned 200 response (the Rust analogue of the Python `_transport(body)`).
struct Canned(Vec<u8>);

impl Fetch for Canned {
    fn get(&self, _url: &str) -> Result<(Box<dyn Read>, Option<u64>), String> {
        Ok((Box::new(Cursor::new(self.0.clone())), Some(self.0.len() as u64)))
    }
}

struct Boom;

impl Fetch for Boom {
    fn get(&self, _url: &str) -> Result<(Box<dyn Read>, Option<u64>), String> {
        panic!("should not download");
    }
}

fn part_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "part")).collect())
        .unwrap_or_default()
}

/// Serve exactly one HTTP response on 127.0.0.1; returns the URL.
fn serve_once(status: u16, reason: &'static str, body: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let mut req = Vec::new();
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body);
        }
    });
    format!("http://{addr}/model.onnx")
}

#[test]
fn download_model_writes_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("models").join("m.onnx");
    download_model(&out, DEFAULT_URL, false, &Canned(vec![b'x'; 2_000_000]), false, false).unwrap();
    assert_eq!(std::fs::metadata(&out).unwrap().len(), 2_000_000);
    assert!(part_files(out.parent().unwrap()).is_empty());
}

#[test]
fn download_model_noop_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("m.onnx");
    std::fs::write(&out, b"existing").unwrap();
    download_model(&out, DEFAULT_URL, false, &Boom, false, true).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"existing");
}

#[test]
fn download_model_rejects_bad_responses() {
    let cases: [(&'static [u8], u16, &'static str); 2] = [(b"<html>nope</html>", 200, "OK"), (b"denied", 403, "Forbidden")];
    for (body, status, reason) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("m.onnx");
        let url = serve_once(status, reason, body);
        let res: Result<PathBuf, ModelDownloadError> = download_model(&out, &url, false, &HttpFetch, false, false);
        let e = res.expect_err("expected ModelDownloadError").to_string();
        // 200 + tiny body -> size check; 403 -> HTTP status error (not a proxy/connect error)
        let why = if status == 200 { "looks wrong".to_string() } else { format!("model download failed: {status}") };
        assert!(e.contains(&why), "status {status}: {e}");
        assert!(!out.exists(), "status {status}");
        assert!(part_files(tmp.path()).is_empty(), "status {status}");
    }
}

#[test]
fn download_model_rejects_non_onnx() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("m.onnx");
    let e = download_model(&out, DEFAULT_URL, false, &Canned(vec![0u8; 2_000_000]), false, true).unwrap_err();
    assert!(e.to_string().contains("not a loadable ONNX"), "{e}");
    assert!(!out.exists());
    assert!(part_files(tmp.path()).is_empty());
}

// ---------------------------------------------------------------- CLI

/// The CLI binary run from `cwd`, isolated from the host's PRUSA_WATCH_* and PW_TEST_* vars.
fn cli(cwd: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_prusa-watch"));
    c.current_dir(cwd);
    for (k, _) in std::env::vars() {
        if k.starts_with("PRUSA_WATCH") || k.starts_with("PW_TEST_") {
            c.env_remove(k);
        }
    }
    c
}

/// Route every HTTP(S) request of the child to a closed local port, so a download
/// attempt fails immediately instead of fetching ~200 MB.
fn no_network(c: &mut Command) -> &mut Command {
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    }; // listener dropped: connections are refused
    for k in ["NO_PROXY", "no_proxy"] {
        c.env_remove(k);
    }
    for k in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        c.env(k, &dead);
    }
    c
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn run(c: &mut Command) -> Output {
    c.output().expect("run prusa-watch")
}

#[test]
fn env_file_next_to_config_feeds_config() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap(); // cwd without a .env
    std::fs::write(
        tmp.path().join("config.yaml"),
        "printer: {host: 192.168.1.50, password: '${PW_TEST_PASSWORD}'}\ncamera: {url: 'rtsp://192.168.1.51/live'}\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join(".env"), "PW_TEST_PASSWORD=s3cret\n").unwrap();
    let out = run(cli(work.path()).arg("config").arg("-c").arg(tmp.path().join("config.yaml")).arg("--show-secrets"));
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("password: s3cret"), "{stdout}");
    assert!(stderr.contains("loaded") && stderr.contains(".env"), "{stderr}");
}

#[test]
fn shell_env_overrides_env_file() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("config.yaml"),
        "printer: {host: h, password: '${PW_TEST_PASSWORD}'}\ncamera: {url: 'rtsp://c/live'}\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join(".env"), "PW_TEST_PASSWORD=from-file\n").unwrap();
    let out = run(cli(work.path())
        .env("PW_TEST_PASSWORD", "from-shell")
        .arg("config")
        .arg("-c")
        .arg(tmp.path().join("config.yaml"))
        .arg("--show-secrets"));
    assert!(text(&out.stdout).contains("password: from-shell"), "{}", text(&out.stdout));
}

#[test]
fn missing_config_gives_a_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(cli(tmp.path()).arg("check").arg("-c").arg(tmp.path().join("nope.yaml")));
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("cp config.example.yaml"), "{}", text(&out.stderr));
}

#[test]
fn fetch_model_uses_configured_path() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("weights").join("w.onnx");
    let config = tmp.path().join("config.yaml");
    std::fs::write(&config, format!("detector: {{model_path: '{}'}}\n", target.display())).unwrap();
    let fetch = |force: bool| {
        let mut c = cli(tmp.path());
        no_network(&mut c).arg("fetch-model").arg("-c").arg(&config);
        if force {
            c.arg("--force");
        }
        run(&mut c)
    };

    // missing -> downloads to the configured path (the attempt fails here: no network)
    let out = fetch(false);
    let err = text(&out.stderr);
    assert!(err.contains("Downloading model from"), "{err}");
    assert!(target.parent().unwrap().is_dir(), "download should target {}", target.display());
    assert!(!target.exists() && part_files(target.parent().unwrap()).is_empty());

    // present -> no download unless --force
    std::fs::write(&target, b"m").unwrap();
    let out = fetch(false);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    assert!(!text(&out.stderr).contains("Downloading model"), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains(&target.display().to_string()), "{}", text(&out.stdout));

    // --force -> re-download attempted even though the file exists (and a failed one keeps it)
    let out = fetch(true);
    assert!(text(&out.stderr).contains("Downloading model from"), "{}", text(&out.stderr));
    assert_eq!(std::fs::read(&target).unwrap(), b"m");
}

#[test]
fn ensure_model_reports_failure() {
    // `_ensure_model` is private to the binary; `fetch-model` is its thinnest caller.
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.yaml");
    std::fs::write(&config, format!("detector: {{model_path: '{}'}}\n", tmp.path().join("m.onnx").display())).unwrap();
    let mut c = cli(tmp.path());
    let out = run(no_network(&mut c).arg("fetch-model").arg("-c").arg(&config));
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("prusa-watch fetch-model"), "{}", text(&out.stderr));
}
