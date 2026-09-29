//! Listen for button replies on an ntfy topic.
//!
//! Alert buttons POST a tiny text body ("<command> <incident-id>", e.g. "veto x1Y2z3") to
//! `notify.ntfy.reply_topic`. We hold an outbound streaming subscription to that
//! topic (`GET /<topic>/json`), so replies arrive whether your phone is on the
//! home Wi-Fi or on LTE, with nothing exposed to the internet.
//!
//! Every command must carry the id of the *current* incident; anything else
//! (old alerts, replays, junk someone posted to the topic) is ignored.

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::config::NtfyConfig;

pub const COMMANDS: [&str; 5] = ["veto", "act", "stop", "resume", "mute"];

pub fn parse_command(text: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() != 2 {
        return None;
    }
    let cmd = parts[0].to_lowercase();
    COMMANDS.contains(&cmd.as_str()).then(|| (cmd, parts[1].to_string()))
}

/// Opens the streaming GET (real HTTP, or canned lines in tests).
pub trait StreamOpener: Send + Sync {
    fn open(&self, url: &str, since: &str, headers: &[(String, String)]) -> Result<Box<dyn BufRead + Send>, String>;
}

pub struct HttpStreamOpener {
    client: reqwest::blocking::Client,
}

impl Default for HttpStreamOpener {
    fn default() -> Self {
        // The blocking client's timeout applies per read, not to the whole (long-lived)
        // stream; ntfy sends keepalives every ~45 s, so 120 s detects a dead connection.
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("http client");
        Self { client }
    }
}

impl StreamOpener for HttpStreamOpener {
    fn open(&self, url: &str, since: &str, headers: &[(String, String)]) -> Result<Box<dyn BufRead + Send>, String> {
        let sep = if url.contains('?') { '&' } else { '?' };
        let since: String = since.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let mut rb = self.client.get(format!("{url}{sep}since={since}"));
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().map_err(|e| crate::http::describe(&e))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        Ok(Box::new(BufReader::new(resp)))
    }
}

pub type Handler = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

pub struct NtfyReplyListener {
    pub cfg: NtfyConfig,
    handler: Handler,
    opener: Arc<dyn StreamOpener>,
    pub backoff_s: f64,
    since: Mutex<String>,
    stop: AtomicBool,
    wake: (Mutex<()>, Condvar),
    pub connected: AtomicBool,
    pub received: AtomicU64,
}

impl NtfyReplyListener {
    pub fn new(cfg: NtfyConfig, handler: Handler) -> Arc<Self> {
        Self::with_opener(cfg, handler, Arc::new(HttpStreamOpener::default()))
    }

    pub fn with_opener(cfg: NtfyConfig, handler: Handler, opener: Arc<dyn StreamOpener>) -> Arc<Self> {
        let backoff_s = cfg.reply_reconnect_s;
        Arc::new(Self {
            cfg,
            handler,
            opener,
            backoff_s,
            // never act on replies older than our start
            since: Mutex::new((crate::now_ts() as i64).to_string()),
            stop: AtomicBool::new(false),
            wake: (Mutex::new(()), Condvar::new()),
            connected: AtomicBool::new(false),
            received: AtomicU64::new(0),
        })
    }

    pub fn url(&self) -> String {
        format!("{}/{}/json", self.cfg.url.trim_end_matches('/'), self.cfg.reply_topic)
    }

    pub fn since(&self) -> String {
        self.since.lock().unwrap().clone()
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::Builder::new().name("ntfy-replies".into()).spawn(move || me.run()).ok();
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _g = self.wake.0.lock().unwrap(); // no lost wakeup for a thread about to wait
        self.wake.1.notify_all();
    }

    fn run(&self) {
        while !self.stop.load(Ordering::Relaxed) {
            if let Err(e) = self.stream_once() {
                tracing::warn!("Replies: ntfy stream error ({e}); reconnecting in {:.0}s", self.backoff_s);
            }
            self.connected.store(false, Ordering::Relaxed);
            let (lock, cv) = &self.wake;
            let g = lock.lock().unwrap();
            let _ = cv
                .wait_timeout_while(g, Duration::from_secs_f64(self.backoff_s.max(0.1)), |_| !self.stop.load(Ordering::Relaxed));
        }
    }

    pub fn stream_once(&self) -> Result<(), String> {
        let headers: Vec<(String, String)> =
            if self.cfg.token.is_empty() { vec![] } else { vec![("Authorization".into(), format!("Bearer {}", self.cfg.token))] };
        let since = self.since();
        let reader = self.opener.open(&self.url(), &since, &headers)?;
        self.connected.store(true, Ordering::Relaxed);
        tracing::info!("Replies: listening on ntfy topic {}", self.cfg.reply_topic);
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            self.handle_line(&line);
        }
        Ok(())
    }

    pub fn handle_line(&self, line: &str) -> Option<String> {
        if line.trim().is_empty() {
            return None;
        }
        let msg: Value = serde_json::from_str(line).ok()?;
        if msg.get("event").and_then(Value::as_str) != Some("message") {
            return None; // open / keepalive
        }
        if let Some(id) = msg.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            *self.since.lock().unwrap() = id.to_string(); // resume after this message on reconnect
        }
        let Some((cmd, iid)) = parse_command(msg.get("message").and_then(Value::as_str).unwrap_or("")) else {
            tracing::info!("Replies: ignoring unrecognized message on reply topic");
            return None;
        };
        self.received.fetch_add(1, Ordering::Relaxed);
        // A panic in the handler must not kill the listener thread (buttons would silently stop).
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.handler)(&cmd, &iid))) {
            Ok(r) => Some(r),
            Err(_) => {
                tracing::error!("Replies: handler failed for '{cmd}'");
                None
            }
        }
    }
}
