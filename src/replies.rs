//! Listen for button replies on an ntfy topic.
//!
//! Alert buttons POST a tiny text body to `notify.ntfy.reply_topic`:
//! `"<command> <incident-id> <expires> <signature>"`. We hold an outbound streaming
//! subscription to that topic (`GET /<topic>/json`), so replies arrive whether your
//! phone is on the home Wi-Fi or on LTE, with nothing exposed to the internet.
//!
//! Anyone who learns the topic name can post to it, so a message is only a command if it
//! carries a signature this installation issued for exactly that command and incident and
//! it has not expired ([`crate::capability`]). Everything else is dropped unread: junk,
//! unsigned or forged commands, a signature moved to another command, and old replays.
//! The monitor then still requires the incident to be the open one.

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::capability::{Grant, Signer};
use crate::config::NtfyConfig;

pub const COMMANDS: [&str; 5] = ["veto", "act", "stop", "resume", "mute"];

/// A syntactically valid reply. Not yet authorized: see [`Signer::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub command: String,
    pub incident: String,
    pub expires: i64,
    pub signature: String,
}

pub fn parse_command(text: &str) -> Option<Reply> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [command, incident, expires, signature] = parts.as_slice() else {
        return None;
    };
    let command = command.to_lowercase();
    if !COMMANDS.contains(&command.as_str()) {
        return None;
    }
    Some(Reply {
        command,
        incident: incident.to_string(),
        expires: expires.parse().ok()?,
        signature: signature.to_string(),
    })
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
        // A total request deadline bounds shutdown, including an idle stream. Reconnect
        // from the last processed message; keepalives do not extend the deadline.
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(5))
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
    signer: Signer,
    handler: Handler,
    /// Told about genuine replies that arrived after their expiry (command, incident).
    expired: Mutex<Option<Handler>>,
    opener: Arc<dyn StreamOpener>,
    pub backoff_s: f64,
    since: Mutex<String>,
    stop: AtomicBool,
    wake: (Mutex<()>, Condvar),
    pub connected: AtomicBool,
    /// authorized commands handed to the handler
    pub received: AtomicU64,
    /// well-formed commands dropped: unsigned, forged, wrong scope or expired
    pub rejected: AtomicU64,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl NtfyReplyListener {
    /// `signer` must be the one that signs the notification buttons.
    pub fn new(cfg: NtfyConfig, signer: Signer, handler: Handler) -> Arc<Self> {
        Self::with_opener(cfg, signer, handler, Arc::new(HttpStreamOpener::default()))
    }

    pub fn with_opener(cfg: NtfyConfig, signer: Signer, handler: Handler, opener: Arc<dyn StreamOpener>) -> Arc<Self> {
        let backoff_s = cfg.reply_reconnect_s;
        Arc::new(Self {
            cfg,
            signer,
            handler,
            expired: Mutex::new(None),
            opener,
            backoff_s,
            // never act on replies older than our start
            since: Mutex::new((crate::now_ts() as i64).to_string()),
            stop: AtomicBool::new(false),
            wake: (Mutex::new(()), Condvar::new()),
            connected: AtomicBool::new(false),
            received: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            thread: Mutex::new(None),
        })
    }

    /// Call `handler(command, incident)` for a genuine reply that arrived too late, so the
    /// person who tapped the button can be told it did nothing.
    pub fn on_expired(&self, handler: Handler) {
        *self.expired.lock().unwrap() = Some(handler);
    }

    pub fn url(&self) -> String {
        format!("{}/{}/json", self.cfg.url.trim_end_matches('/'), self.cfg.reply_topic)
    }

    pub fn since(&self) -> String {
        self.since.lock().unwrap().clone()
    }

    pub fn start(self: &Arc<Self>) {
        let mut thread = self.thread.lock().unwrap();
        if thread.is_some() {
            return;
        }
        let me = self.clone();
        *thread = Some(
            std::thread::Builder::new()
                .name("ntfy-replies".into())
                .spawn(move || me.run())
                .expect("spawn reply listener"),
        );
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _g = self.wake.0.lock().unwrap(); // no lost wakeup for a thread about to wait
        self.wake.1.notify_all();
        drop(_g);
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
        self.connected.store(false, Ordering::Relaxed);
    }

    fn run(&self) {
        while !self.stop.load(Ordering::Relaxed) {
            if let Err(e) = self.stream_once() {
                tracing::warn!(
                    "Replies: ntfy stream error ({e}); reconnecting in {:.0}s",
                    self.backoff_s
                );
            }
            self.connected.store(false, Ordering::Relaxed);
            let (lock, cv) = &self.wake;
            let g = lock.lock().unwrap();
            let _ = cv.wait_timeout_while(g, Duration::from_secs_f64(self.backoff_s.max(0.1)), |_| {
                !self.stop.load(Ordering::Relaxed)
            });
        }
    }

    pub fn stream_once(&self) -> Result<(), String> {
        let headers: Vec<(String, String)> = if self.cfg.token.is_empty() {
            vec![]
        } else {
            vec![("Authorization".into(), format!("Bearer {}", self.cfg.token))]
        };
        let since = self.since();
        let reader = self.opener.open(&self.url(), &since, &headers)?;
        self.connected.store(true, Ordering::Relaxed);
        tracing::info!("Replies: listening for button replies");
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            self.handle_line(&line);
        }
        Ok(())
    }

    /// Remember how far the stream has been read, so a reconnect never replays it.
    fn advance(&self, id: Option<&str>) {
        if let Some(id) = id {
            *self.since.lock().unwrap() = id.to_string();
        }
    }

    pub fn handle_line(&self, line: &str) -> Option<String> {
        if line.trim().is_empty() {
            return None;
        }
        let msg: Value = serde_json::from_str(line).ok()?;
        if msg.get("event").and_then(Value::as_str) != Some("message") {
            return None; // open / keepalive
        }
        let id = msg.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
        // Message text is attacker-controlled and may hold a signature: it is never logged.
        let Some(reply) = parse_command(msg.get("message").and_then(Value::as_str).unwrap_or("")) else {
            self.advance(id);
            tracing::info!("Replies: ignoring unrecognized message on reply topic");
            return None;
        };
        let (cmd, iid) = (reply.command.as_str(), reply.incident.as_str());
        match self
            .signer
            .check(cmd, iid, reply.expires, &reply.signature, crate::now_ts())
        {
            Grant::Valid => {}
            Grant::Invalid => {
                self.advance(id);
                self.rejected.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("Replies: ignoring '{cmd}' without a valid signature");
                return None;
            }
            Grant::Expired => {
                self.advance(id);
                self.rejected.fetch_add(1, Ordering::Relaxed);
                tracing::info!("Replies: ignoring '{cmd}' from an expired notification");
                let expired = self.expired.lock().unwrap().clone();
                if let Some(expired) = expired {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| expired(cmd, iid)));
                }
                return None;
            }
        }
        self.received.fetch_add(1, Ordering::Relaxed);
        // A panic in the handler must not kill the listener thread (buttons would silently stop).
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.handler)(cmd, iid))) {
            Ok(r) => {
                self.advance(id);
                Some(r)
            }
            Err(_) => {
                tracing::error!("Replies: handler failed for '{cmd}'");
                None
            }
        }
    }
}
