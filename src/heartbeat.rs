//! Dead-man's switch: ping an external monitor (healthchecks.io, Uptime Kuma, ...) while
//! prusa-watch can protect a print.
//!
//! Every other alert comes from this process. If the host sleeps, the container dies or the
//! network drops, only something outside can notice the silence.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::HealthConfig;
use crate::http::{HttpRequest, Transport};
use crate::monitor::Monitor;

pub struct Heartbeat {
    url: String,
    fail_url: String,
    timeout: Duration,
    transport: Arc<dyn Transport>,
    /// Last (health ok, ping delivered), to log transitions rather than every beat.
    last: Mutex<Option<(bool, bool)>>,
}

/// What a single beat did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Beat {
    /// Healthy; `url` requested.
    Up,
    /// Unhealthy; `fail_url` requested.
    Down(String),
    /// Unhealthy and no `fail_url`: no request, so the external timer runs out.
    Skipped(String),
}

impl Heartbeat {
    /// None when no heartbeat URL is configured.
    pub fn new(cfg: &HealthConfig, timeout_s: f64, transport: Arc<dyn Transport>) -> Option<Self> {
        (!cfg.heartbeat_url.is_empty()).then(|| Self {
            url: cfg.heartbeat_url.clone(),
            fail_url: cfg.heartbeat_fail_url.clone(),
            timeout: Duration::from_secs_f64(timeout_s),
            transport,
            last: Mutex::new(None),
        })
    }

    /// Report `health` once. `Err` means the ping itself failed (network, bad status).
    pub fn beat(&self, health: Result<(), String>) -> Result<Beat, String> {
        let (template, beat) = match health {
            Ok(()) => (&self.url, Beat::Up),
            Err(reason) if self.fail_url.is_empty() => {
                self.log(false, true, &reason);
                return Ok(Beat::Skipped(reason));
            }
            Err(reason) => (&self.fail_url, Beat::Down(reason)),
        };
        let reason = match &beat {
            Beat::Up => "ok",
            Beat::Down(r) | Beat::Skipped(r) => r.as_str(),
        };
        let url = template.replace("{reason}", &encode(reason));
        let result = self
            .transport
            .send(&HttpRequest::new("GET", url), self.timeout)
            .and_then(|resp| {
                if (200..300).contains(&resp.status) {
                    Ok(())
                } else {
                    Err(format!("HTTP {}", resp.status))
                }
            });
        let healthy = beat == Beat::Up;
        match result {
            Ok(()) => {
                self.log(healthy, true, reason);
                Ok(beat)
            }
            Err(e) => {
                self.log(healthy, false, &e);
                Err(e)
            }
        }
    }

    fn log(&self, healthy: bool, delivered: bool, detail: &str) {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if *last == Some((healthy, delivered)) {
            return;
        }
        *last = Some((healthy, delivered));
        match (healthy, delivered) {
            (true, true) => tracing::info!("Heartbeat: reporting healthy"),
            (false, true) => tracing::warn!("Heartbeat: reporting unhealthy: {detail}"),
            (_, false) => tracing::warn!("Heartbeat: ping failed: {detail}"),
        }
    }

    /// Beat every `interval_s` from a separate thread until the monitor stops.
    pub fn spawn(self, monitor: Arc<Monitor>, interval_s: f64) -> JoinHandle<()> {
        let interval = Duration::from_secs_f64(interval_s);
        std::thread::Builder::new()
            .name("heartbeat".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(interval);
                    if monitor.stopping() {
                        return;
                    }
                    let _ = self.beat(monitor.protection_health());
                }
            })
            .expect("spawn heartbeat")
    }
}

/// Percent-encode everything but RFC 3986 unreserved characters.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
