//! Notifications: ntfy (default, self-hostable), Discord webhook, generic JSON webhook.
//!
//! Which channels, what priority and which buttons each notification gets is
//! decided by the caller (escalation steps and notify.warning/camera/info config).
//! Sends run on a background thread so a slow endpoint can never delay a pause.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{CHANNELS, NotifyConfig};
use crate::escalation::button;
use crate::http::{Body, HttpRequest, Part, ReqwestTransport, Transport};

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// "incident" | "failure" | "warning" | "camera_down" | "camera_up" | "info"
    pub kind: String,
    pub title: String,
    pub message: String,
    pub printer: String,
    pub job_id: Option<i64>,
    pub job_name: Option<String>,
    pub score: Option<f64>,
    /// "paused" | "stopped" | None
    pub action_taken: Option<String>,
    #[serde(skip)]
    pub image_jpeg: Option<Arc<Vec<u8>>>,
    /// ntfy 1..5
    pub priority: i64,
    /// see escalation::button
    pub buttons: Vec<String>,
    pub incident_id: Option<String>,
    pub policy: Option<String>,
    pub next_action: Option<String>,
    pub next_action_ts: Option<f64>,
    pub ts: String,
}

impl Event {
    pub fn new(kind: &str, title: impl Into<String>, message: impl Into<String>, printer: &str) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            message: message.into(),
            printer: printer.into(),
            job_id: None,
            job_name: None,
            score: None,
            action_taken: None,
            image_jpeg: None,
            priority: 3,
            buttons: vec![],
            incident_id: None,
            policy: None,
            next_action: None,
            next_action_ts: None,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false),
        }
    }

    /// The attached image, if any (empty bytes count as none, like Python's truthiness).
    pub fn image(&self) -> Option<&Arc<Vec<u8>>> {
        self.image_jpeg.as_ref().filter(|b| !b.is_empty())
    }

    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap();
        v["has_image"] = Value::Bool(self.image_jpeg.is_some());
        v
    }
}

fn ascii(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii() { c } else { '?' }).collect()
}

/// ntfy action header values containing , or ; must be quoted
fn q(v: &str) -> String {
    if v.contains(',') || v.contains(';') {
        format!("\"{v}\"")
    } else {
        v.to_string()
    }
}

pub struct Notifier {
    pub cfg: NotifyConfig,
    pub public_url: String,
    transport: Arc<dyn Transport>,
    /// send on the caller's thread (tests)
    pub blocking: bool,
    pub sent: Arc<AtomicU64>,
    pub errors: Arc<AtomicU64>,
    workers: Mutex<std::collections::HashMap<String, Arc<crate::worker::Worker<Event>>>>,
    pub signer: crate::capability::Signer,
    stopping: Arc<AtomicBool>,
}

impl Notifier {
    pub fn new(cfg: NotifyConfig, public_url: &str, control_token: &str) -> Self {
        Self::with_transport(cfg, public_url, control_token, Arc::new(ReqwestTransport::new()))
    }

    pub fn with_transport(
        cfg: NotifyConfig,
        public_url: &str,
        _control_token: &str,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            cfg,
            public_url: public_url.trim_end_matches('/').to_string(),
            transport,
            blocking: false,
            sent: Arc::new(AtomicU64::new(0)),
            errors: Arc::new(AtomicU64::new(0)),
            workers: Mutex::new(std::collections::HashMap::new()),
            signer: crate::capability::Signer::default(),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn configured(&self) -> Vec<&'static str> {
        let c = &self.cfg;
        [
            ("ntfy", !c.ntfy.topic.is_empty()),
            ("discord", !c.discord.webhook_url.is_empty()),
            ("webhook", !c.webhook.url.is_empty()),
        ]
        .into_iter()
        .filter(|(_, ok)| *ok)
        .map(|(n, _)| n)
        .collect()
    }

    pub fn enabled(&self) -> bool {
        !self.configured().is_empty()
    }

    /// channels=None -> every configured channel; Some([]) -> nothing.
    pub fn send(self: &Arc<Self>, event: Event, channels: Option<&[String]>) {
        let configured = self.configured();
        let targets: Vec<&'static str> = match channels {
            None => configured.clone(),
            Some(list) => configured
                .iter()
                .filter(|c| list.iter().any(|l| l == *c))
                .copied()
                .collect(),
        };
        if targets.is_empty() {
            tracing::info!("Notify (no channel): {} - {}", event.title, event.message);
            return;
        }
        if self.blocking {
            self.send_all(&event, &targets);
        } else {
            let mut workers = self.workers.lock().unwrap();
            for name in targets {
                let worker = workers.entry(name.into()).or_insert_with(|| {
                    // A worker owns its sender dependencies, never an Arc back to its owner.
                    let mut sender =
                        Notifier::with_transport(self.cfg.clone(), &self.public_url, "", self.transport.clone());
                    sender.signer = self.signer.clone();
                    sender.stopping = self.stopping.clone();
                    let sent = self.sent.clone();
                    let errors = self.errors.clone();
                    Arc::new(crate::worker::Worker::new(
                        &format!("notify-{name}"),
                        16,
                        move |event: Event| {
                            let req = sender.request(name, &event);
                            for attempt in 0..3 {
                                if sender.stopping.load(Ordering::Acquire) {
                                    return Ok(());
                                }
                                let response = sender
                                    .transport
                                    .send(&req, Duration::from_secs_f64(sender.cfg.timeout_s));
                                match response {
                                    Ok(response) if (200..300).contains(&response.status) => {
                                        sent.fetch_add(1, Ordering::Relaxed);
                                        return Ok(());
                                    }
                                    Ok(response) if response.status != 429 && response.status < 500 => break,
                                    _ if attempt < 2 => std::thread::sleep(Duration::from_millis(250 << attempt)),
                                    _ => break,
                                }
                            }
                            errors.fetch_add(1, Ordering::Relaxed);
                            Err(format!("notification via {name} failed after bounded retries"))
                        },
                    ))
                });
                if !worker.submit(event.clone()) {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let workers: Vec<_> = self.workers.lock().unwrap().values().cloned().collect();
        for worker in workers {
            worker.stop();
        }
    }

    pub fn dropped(&self) -> u64 {
        self.workers
            .lock()
            .unwrap()
            .values()
            .map(|worker| worker.stats.dropped.load(Ordering::Relaxed))
            .sum()
    }

    pub fn delivery_health(&self) -> Value {
        let workers = self.workers.lock().unwrap();
        Value::Object(
            workers
                .iter()
                .map(|(name, worker)| {
                    (
                        name.clone(),
                        json!({
                            "sent": worker.stats.completed.load(Ordering::Relaxed),
                            "errors": worker.stats.errors.load(Ordering::Relaxed),
                            "dropped": worker.stats.dropped.load(Ordering::Relaxed),
                            "last_error": worker.stats.last_error.lock().unwrap().clone(),
                        }),
                    )
                })
                .collect(),
        )
    }

    fn request(&self, name: &str, event: &Event) -> HttpRequest {
        match name {
            "ntfy" => self.ntfy_request(event),
            "discord" => self.discord_request(event),
            _ => self.webhook_request(event),
        }
    }

    fn send_all(&self, event: &Event, targets: &[&str]) {
        for name in CHANNELS {
            if !targets.contains(&name) {
                continue;
            }
            let req = self.request(name, event);
            let res = self
                .transport
                .send(&req, Duration::from_secs_f64(self.cfg.timeout_s.max(0.1)))
                .and_then(|r| {
                    if !(200..300).contains(&r.status) {
                        Err(format!(
                            "HTTP {} {}",
                            r.status,
                            r.text().chars().take(200).collect::<String>()
                        ))
                    } else {
                        Ok(())
                    }
                });
            match res {
                Ok(()) => {
                    self.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!("Notify via {name} failed: {e}");
                }
            }
        }
    }

    // -- buttons -----------------------------------------------------------
    pub fn dashboard_url(&self, cmd: &str, incident_id: &str) -> String {
        let mut url = reqwest::Url::parse(&format!("{}/api/incident/{cmd}", self.public_url))
            .expect("configured dashboard URL must be valid");
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("id", incident_id);
            let expires = crate::now_ts() as i64 + 3600;
            query.append_pair("expires", &expires.to_string());
            query.append_pair("cap", &self.signer.sign(cmd, incident_id, expires));
        }
        url.into()
    }

    /// Translate button names into ntfy action definitions.
    ///
    /// Replies go to ntfy.reply_topic when set (works from anywhere: prusa-watch
    /// subscribes outbound); otherwise straight to the dashboard (LAN/VPN only).
    pub fn ntfy_actions(&self, e: &Event) -> Vec<String> {
        let c = &self.cfg.ntfy;
        let mut out = Vec::new();
        for b in &e.buttons {
            let Some((cmd, label)) = button(b) else { continue };
            let label = if b == "act" {
                if e.next_action.as_deref() == Some("stop") {
                    "Stop now"
                } else {
                    "Pause now"
                }
            } else {
                label.unwrap_or("")
            };
            let Some(cmd) = cmd else {
                if !self.public_url.is_empty() {
                    out.push(format!("view, {label}, {}", self.public_url));
                }
                continue;
            };
            let Some(iid) = e.incident_id.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            if !c.reply_topic.is_empty() {
                let url = format!("{}/{}", c.url.trim_end_matches('/'), c.reply_topic);
                let auth = if c.token.is_empty() {
                    String::new()
                } else {
                    format!(", headers.Authorization=Bearer {}", c.token)
                };
                out.push(format!(
                    "http, {}, {url}, method=POST{auth}, body={cmd} {iid}, clear=true",
                    q(label)
                ));
            } else if !self.public_url.is_empty() {
                out.push(format!(
                    "http, {}, {}, method=POST, clear=true",
                    q(label),
                    self.dashboard_url(cmd, iid)
                ));
            }
        }
        out.truncate(3);
        out
    }

    // -- channels ---------------------------------------------------------
    pub fn ntfy_request(&self, e: &Event) -> HttpRequest {
        let c = &self.cfg.ntfy;
        let url = format!("{}/{}", c.url.trim_end_matches('/'), c.topic);
        let tags = match e.kind.as_str() {
            "failure" => "rotating_light,printer",
            "incident" => "hourglass_flowing_sand,printer",
            "warning" => "warning,printer",
            "camera_down" => "no_entry_sign,camera",
            "camera_up" => "white_check_mark,camera",
            _ => "printer",
        };
        let method = if e.image().is_some() { "PUT" } else { "POST" };
        let mut req = HttpRequest::new(method, url)
            .header("Title", ascii(&e.title))
            .header("Priority", e.priority.clamp(1, 5).to_string())
            .header("Tags", tags);
        if !c.token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", c.token));
        }
        let actions = self.ntfy_actions(e);
        if !actions.is_empty() {
            req = req.header("Actions", actions.join("; "));
        }
        if !self.public_url.is_empty() {
            req = req.header("Click", self.public_url.clone());
        }
        match e.image() {
            Some(img) => req
                .header("Filename", "frame.jpg")
                .header("Message", ascii(&e.message))
                .body(Body::Bytes(img.to_vec())),
            None => req.body(Body::Bytes(e.message.clone().into_bytes())),
        }
    }

    pub fn discord_request(&self, e: &Event) -> HttpRequest {
        let color: i64 = match e.kind.as_str() {
            "failure" | "incident" => 0xE5484D,
            "warning" => 0xF5A524,
            "camera_down" => 0x8B8D98,
            _ => 0x3E63DD,
        };
        let mut embed = json!({"title": e.title, "description": e.message, "color": color, "timestamp": e.ts});
        if e.image().is_some() {
            embed["image"] = json!({"url": "attachment://frame.jpg"});
        }
        let payload = json!({"embeds": [embed]});
        let req = HttpRequest::new("POST", self.cfg.discord.webhook_url.clone());
        match e.image() {
            Some(img) => req.body(Body::Multipart(vec![
                Part {
                    name: "payload_json".into(),
                    filename: None,
                    content_type: None,
                    data: payload.to_string().into_bytes(),
                },
                Part {
                    name: "files[0]".into(),
                    filename: Some("frame.jpg".into()),
                    content_type: Some("image/jpeg".into()),
                    data: img.to_vec(),
                },
            ])),
            None => req.body(Body::Json(payload)),
        }
    }

    pub fn webhook_request(&self, e: &Event) -> HttpRequest {
        let mut body = e.to_json();
        if !self.public_url.is_empty() && e.image().is_some() {
            body["image_url"] = json!(format!("{}/frame.jpg", self.public_url));
        }
        if let Some(iid) = e.incident_id.as_deref().filter(|s| !s.is_empty())
            && !self.public_url.is_empty()
        {
            let mut urls = serde_json::Map::new();
            for b in ["keep", "act", "stop", "resume", "mute"] {
                let cmd = button(b).unwrap().0.unwrap();
                urls.insert(cmd.into(), json!(self.dashboard_url(cmd, iid)));
            }
            body["command_urls"] = Value::Object(urls);
        }
        HttpRequest::new("POST", self.cfg.webhook.url.clone()).body(Body::Json(body))
    }
}
