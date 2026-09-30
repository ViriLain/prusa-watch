//! Configuration loading.
//!
//! Layers, lowest to highest precedence:
//!   1. Built-in defaults (the `Default` impls below; `escalation::BUILTIN`)
//!   2. Your config.yaml: only the keys you want to change. `${ENV_VAR}` and
//!      `${ENV_VAR:-default}` are expanded inside it.
//!   3. Environment overrides: `PRUSA_WATCH__<SECTION>__<KEY>=value`, e.g.
//!      `PRUSA_WATCH__DECISION__SENSITIVITY=1.25`,
//!      `PRUSA_WATCH__ESCALATION__DEFAULT_POLICY=watch_only`.
//!      Values are parsed as YAML (numbers, true/false, `[lists]`).
//!
//! `prusa-watch config` prints the effective result; config.reference.yaml lists
//! every setting with its default.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrinterConfig {
    pub name: String,
    /// IP/hostname of the printer (PrusaLink)
    pub host: String,
    pub scheme: String,
    /// Buddy firmware PrusaLink user is always "maker"
    pub username: String,
    /// Settings > Network > PrusaLink on the printer
    pub password: String,
    /// "digest" (user/password) or "apikey" (X-Api-Key header)
    pub auth: String,
    pub poll_interval_s: f64,
    pub timeout_s: f64,
    /// Deadline to observe a requested state change at the printer.
    pub action_confirmation_s: f64,
    pub action_retry_s: f64,
    pub action_max_attempts: u32,
}

impl Default for PrinterConfig {
    fn default() -> Self {
        Self {
            name: "core-one".into(),
            host: String::new(),
            scheme: "http".into(),
            username: "maker".into(),
            password: String::new(),
            auth: "digest".into(),
            poll_interval_s: 5.0,
            timeout_s: 5.0,
            action_confirmation_s: 60.0,
            action_retry_s: 15.0,
            action_max_attempts: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraConfig {
    /// `rtsp://<camera-ip>/live` (or a file path / http URL for testing)
    pub url: String,
    /// RTSP transport; TCP avoids smeared frames on lossy Wi-Fi
    pub transport: String,
    /// Optional crop in normalized coords [x1, y1, x2, y2] (0..1).
    pub roi: Option<Vec<f64>>,
    /// frame older than this = camera considered down
    pub stale_after_s: f64,
    pub reconnect_backoff_s: f64,
    /// RTSP connect/handshake timeout
    pub open_timeout_s: f64,
    /// no frame for this long = reconnect
    pub read_timeout_s: f64,
    /// Regions to ignore, each normalized [x1, y1, x2, y2] in *full-frame* coordinates
    /// (same space as `roi`). Detections whose centre falls inside one are dropped before
    /// scoring: for a spot that glints or collects debris and keeps fooling the model.
    pub ignore: Vec<Vec<f64>>,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            transport: "tcp".into(),
            roi: None,
            stale_after_s: 30.0,
            reconnect_backoff_s: 5.0,
            open_timeout_s: 10.0,
            read_timeout_s: 10.0,
            ignore: vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectorConfig {
    pub model_path: String,
    /// Empty explicitly allows a custom unverified model; otherwise pin its SHA-256.
    pub expected_sha256: String,
    /// per-box confidence floor (Obico default)
    pub threshold: f64,
    pub nms: f64,
    /// Obico's hyperparameters are tuned for 10 s
    pub interval_s: f64,
    pub use_gpu: bool,
    pub visualization_threshold: f64,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            model_path: "models/model-weights.onnx".into(),
            expected_sha256: crate::model::DEFAULT_SHA256.into(),
            threshold: 0.08,
            nms: 0.45,
            interval_s: 10.0,
            use_gpu: false,
            visualization_threshold: 0.2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionConfig {
    /// >1 = more trigger-happy, <1 = more conservative
    pub sensitivity: f64,
    /// After you resume a print that prusa-watch paused, actions stay disarmed this long.
    pub resume_grace_s: f64,
    // Obico 1st-gen hyperparameters (backend/config/settings.py: FD_1ST_GEN_PARAMS)
    pub ewm_span: i64,
    pub rolling_win_short: i64,
    pub rolling_win_long: i64,
    pub threshold_low: f64,
    pub threshold_high: f64,
    /// 30 frames * 10 s = 5 min grace at print start
    pub init_safe_frame_num: i64,
    pub rolling_mean_short_multiple: f64,
    pub escalating_factor: f64,
    /// Fresh-install prior: seed the long-run baseline as if we'd already watched
    /// this many clean (p=0) frames (360 = 1 h). Set 0 for exact Obico behavior.
    pub baseline_prior_frames: i64,
    /// An incident only opens when the *current* frame's summed confidence is at least
    /// this. Obico's verdict rides a moving average, which keeps "remembering" a short
    /// burst (a glint, dust catching the light) after the frame is clean again; without
    /// this gate that tail can pause a print on a frame with nothing in it. 0 = pure Obico.
    pub min_frame_p: f64,
}

impl Default for DecisionConfig {
    fn default() -> Self {
        Self {
            sensitivity: 1.0,
            resume_grace_s: 120.0,
            ewm_span: 12,
            rolling_win_short: 310,
            rolling_win_long: 7200,
            threshold_low: 0.38,
            threshold_high: 0.78,
            init_safe_frame_num: 30,
            rolling_mean_short_multiple: 3.8,
            escalating_factor: 1.75,
            baseline_prior_frames: 360,
            min_frame_p: 0.3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NtfyConfig {
    pub url: String,
    pub topic: String,
    /// optional bearer token for protected/self-hosted topics
    pub token: String,
    /// Topic prusa-watch SUBSCRIBES to for button replies.
    pub reply_topic: String,
    /// backoff when the reply stream drops
    pub reply_reconnect_s: f64,
}

impl Default for NtfyConfig {
    fn default() -> Self {
        Self {
            url: "https://ntfy.sh".into(),
            topic: String::new(),
            token: String::new(),
            reply_topic: String::new(),
            reply_reconnect_s: 5.0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    pub webhook_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookConfig {
    /// receives JSON POST (e.g. Home Assistant webhook trigger)
    pub url: String,
}

pub const CHANNELS: [&str; 3] = ["ntfy", "discord", "webhook"];

/// Routing for non-escalation notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventNotifyConfig {
    pub enabled: bool,
    /// subset of ntfy/discord/webhook; null = every configured channel
    pub channels: Option<Vec<String>>,
    /// ntfy priority 1 (min) .. 5 (max)
    pub priority: i64,
    /// min seconds between repeats within one print
    pub cooldown_s: f64,
}

impl Default for EventNotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            channels: None,
            priority: 3,
            cooldown_s: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub ntfy: NtfyConfig,
    pub discord: DiscordConfig,
    pub webhook: WebhookConfig,
    /// "Possible failure" heads-up (warning band, below the failure threshold)
    pub warning: EventNotifyConfig,
    /// Camera went stale / came back while printing
    pub camera: EventNotifyConfig,
    /// Confirmations: "keeping the print running", "resumed", ...
    pub info: EventNotifyConfig,
    /// per-request timeout for notification HTTP calls
    pub timeout_s: f64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            ntfy: NtfyConfig::default(),
            discord: DiscordConfig::default(),
            webhook: WebhookConfig::default(),
            warning: EventNotifyConfig {
                priority: 4,
                cooldown_s: 300.0,
                ..Default::default()
            },
            camera: EventNotifyConfig {
                priority: 3,
                ..Default::default()
            },
            info: EventNotifyConfig {
                priority: 3,
                ..Default::default()
            },
            timeout_s: 15.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// used for links in notifications, e.g. http://192.168.1.10:8484
    pub public_url: String,
    /// Optional shared secret for control endpoints (pause/resume/stop/mute).
    pub token: String,
    /// Explicit opt-in for network-accessible controls without authentication.
    pub allow_unauthenticated: bool,
    /// score history kept for the dashboard chart
    pub history_s: f64,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 8484,
            public_url: String::new(),
            token: String::new(),
            allow_unauthenticated: false,
            history_s: 7200.0,
        }
    }
}

/// What the detector saw, kept on disk for tuning (state_dir/history, state_dir/frames).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    /// one CSV row per analyzed frame: `history/job-<id>.csv`
    pub history: bool,
    /// annotated frames when the model sees something: `frames/job-<id>/`
    pub frames: bool,
    /// save a frame once its summed confidence reaches this (warning/failure frames always)
    pub frame_min_p: f64,
    /// cap on saved frames per print (~100 KB each)
    pub max_frames_per_job: i64,
    /// history/frames of older prints beyond this many are deleted (0 = keep all)
    pub keep_jobs: i64,
    /// Combined history, frames and failures budget; 0 explicitly disables the byte limit.
    pub max_storage_mb: u64,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            history: true,
            frames: true,
            frame_min_p: 0.3,
            max_frames_per_job: 120,
            keep_jobs: 50,
            max_storage_mb: 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub printer: PrinterConfig,
    pub camera: CameraConfig,
    pub detector: DetectorConfig,
    pub decision: DecisionConfig,
    pub notify: NotifyConfig,
    pub web: WebConfig,
    pub recording: RecordingConfig,
    /// What to do once a failure is detected: parsed and validated by `escalation`.
    pub escalation: Mapping,
    pub state_dir: String,
    /// IANA name for schedules, e.g. America/New_York; empty = system local time
    pub timezone: String,
    pub save_failure_frames: bool,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            printer: Default::default(),
            camera: Default::default(),
            detector: Default::default(),
            decision: Default::default(),
            notify: Default::default(),
            web: Default::default(),
            recording: Default::default(),
            escalation: Mapping::new(),
            state_dir: "data".into(),
            timezone: String::new(),
            save_failure_frames: true,
            log_level: "INFO".into(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors: Vec<String> = Vec::new();
        if self.printer.host.is_empty() {
            errors.push("printer.host is required".into());
        }
        if self.printer.password.is_empty() {
            errors.push("printer.password is required (Settings > Network > PrusaLink on the printer)".into());
        }
        if self.printer.auth != "digest" && self.printer.auth != "apikey" {
            errors.push("printer.auth must be 'digest' or 'apikey'".into());
        }
        if self.camera.url.is_empty() {
            errors.push("camera.url is required (rtsp://<camera-ip>/live)".into());
        }
        if let Err(e) = crate::escalation::parse_escalation(Some(&self.escalation), true) {
            errors.push(e.to_string());
        }
        if let Err(e) = crate::policy::resolve_tz(&self.timezone) {
            errors.push(e.to_string());
        }
        for (name, ev) in [
            ("warning", &self.notify.warning),
            ("camera", &self.notify.camera),
            ("info", &self.notify.info),
        ] {
            let mut bad: Vec<&String> = ev
                .channels
                .iter()
                .flatten()
                .filter(|c| !CHANNELS.contains(&c.as_str()))
                .collect();
            if !bad.is_empty() {
                bad.sort();
                bad.dedup();
                errors.push(format!(
                    "notify.{name}.channels: unknown {} (use {})",
                    crate::policy::py_list(&bad),
                    crate::policy::py_list(&CHANNELS)
                ));
            }
            if !(1..=5).contains(&ev.priority) {
                errors.push(format!("notify.{name}.priority must be 1..5"));
            }
        }
        if self.recording.max_storage_mb > 1_000_000 {
            errors.push("recording.max_storage_mb must be <= 1000000".into());
        }
        if self.recording.max_frames_per_job < 0 || self.recording.keep_jobs < 0 {
            errors.push("recording.max_frames_per_job and recording.keep_jobs must be >= 0".into());
        }
        if let Some(r) = &self.camera.roi
            && (r.len() != 4
                || !(0.0 <= r[0] && r[0] < r[2] && r[2] <= 1.0 && 0.0 <= r[1] && r[1] < r[3] && r[3] <= 1.0))
        {
            errors.push("camera.roi must be [x1, y1, x2, y2] with 0 <= x1 < x2 <= 1 and 0 <= y1 < y2 <= 1".into());
        }
        for (i, r) in self.camera.ignore.iter().enumerate() {
            if r.len() != 4 || !(0.0 <= r[0] && r[0] < r[2] && r[2] <= 1.0 && 0.0 <= r[1] && r[1] < r[3] && r[3] <= 1.0)
            {
                errors.push(format!(
                    "camera.ignore[{i}] must be [x1, y1, x2, y2] with 0 <= x1 < x2 <= 1 and 0 <= y1 < y2 <= 1 (full-frame coordinates)"
                ));
            }
        }
        let mut range = |name: &str, value: f64, min: f64, max: f64| {
            if !value.is_finite() || !(min..=max).contains(&value) {
                errors.push(format!("{name} must be finite and in {min}..={max}"));
            }
        };
        for (name, value) in [
            ("printer.poll_interval_s", self.printer.poll_interval_s),
            ("printer.timeout_s", self.printer.timeout_s),
            ("printer.action_confirmation_s", self.printer.action_confirmation_s),
            ("printer.action_retry_s", self.printer.action_retry_s),
            ("camera.stale_after_s", self.camera.stale_after_s),
            ("camera.reconnect_backoff_s", self.camera.reconnect_backoff_s),
            ("camera.open_timeout_s", self.camera.open_timeout_s),
            ("camera.read_timeout_s", self.camera.read_timeout_s),
            ("detector.interval_s", self.detector.interval_s),
            ("notify.timeout_s", self.notify.timeout_s),
            ("notify.ntfy.reply_reconnect_s", self.notify.ntfy.reply_reconnect_s),
        ] {
            range(name, value, 0.01, 3600.0);
        }
        for (name, value) in [
            ("detector.threshold", self.detector.threshold),
            ("detector.nms", self.detector.nms),
            (
                "detector.visualization_threshold",
                self.detector.visualization_threshold,
            ),
        ] {
            range(name, value, 0.0, 1.0);
        }
        for (name, value) in [
            ("decision.resume_grace_s", self.decision.resume_grace_s),
            ("notify.warning.cooldown_s", self.notify.warning.cooldown_s),
            ("notify.camera.cooldown_s", self.notify.camera.cooldown_s),
            ("notify.info.cooldown_s", self.notify.info.cooldown_s),
        ] {
            range(name, value, 0.0, 604_800.0);
        }
        for (name, value) in [
            ("decision.ewm_span", self.decision.ewm_span),
            ("decision.rolling_win_short", self.decision.rolling_win_short),
            ("decision.rolling_win_long", self.decision.rolling_win_long),
        ] {
            range(name, value as f64, 1.0, 1_000_000_000.0);
        }
        for (name, value) in [
            ("decision.init_safe_frame_num", self.decision.init_safe_frame_num),
            ("decision.baseline_prior_frames", self.decision.baseline_prior_frames),
        ] {
            range(name, value as f64, 0.0, 1_000_000_000.0);
        }
        range("decision.sensitivity", self.decision.sensitivity, 0.001, 100.0);
        range("decision.threshold_low", self.decision.threshold_low, 0.001, 10_000.0);
        range("decision.threshold_high", self.decision.threshold_high, 0.001, 10_000.0);
        range(
            "decision.escalating_factor",
            self.decision.escalating_factor,
            1.0,
            100.0,
        );
        range(
            "decision.rolling_mean_short_multiple",
            self.decision.rolling_mean_short_multiple,
            0.001,
            1000.0,
        );
        range("decision.min_frame_p", self.decision.min_frame_p, 0.0, 10_000.0);
        range("recording.frame_min_p", self.recording.frame_min_p, 0.0, 10_000.0);
        range("web.history_s", self.web.history_s, 0.0, 604_800.0);
        range(
            "printer.action_max_attempts",
            self.printer.action_max_attempts as f64,
            1.0,
            10.0,
        );
        if self.decision.threshold_low >= self.decision.threshold_high {
            errors.push("decision.threshold_low must be less than threshold_high".into());
        }
        if self.web.history_s / self.detector.interval_s > 100_000.0 {
            errors.push("web.history_s / detector.interval_s must be <= 100000 history points".into());
        }
        let loopback = self
            .web
            .host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
            || self.web.host == "localhost";
        if self.web.enabled && !loopback && self.web.token.is_empty() && !self.web.allow_unauthenticated {
            errors.push(
                "web.token is required for a non-loopback web.host (or explicitly set web.allow_unauthenticated)"
                    .into(),
            );
        }
        if !["http", "https"].contains(&self.printer.scheme.as_str()) {
            errors.push("printer.scheme must be http or https".into());
        }
        if !["tcp", "udp"].contains(&self.camera.transport.as_str()) {
            errors.push("camera.transport must be tcp or udp".into());
        }
        if !self.detector.expected_sha256.is_empty()
            && (self.detector.expected_sha256.len() != 64
                || !self.detector.expected_sha256.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            errors.push("detector.expected_sha256 must be empty or a 64-digit SHA-256 digest".into());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(format!(
                "Invalid config:\n  - {}",
                errors.join("\n  - ")
            )))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Config file not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

// ------------------------------------------------------------------ durations

static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(\d+(?:\.\d+)?)h)?\s*(?:(\d+(?:\.\d+)?)m)?\s*(?:(\d+(?:\.\d+)?)s)?\s*$").unwrap()
});

/// Seconds from 90, 90.5, "90", "90s", "2m", "1h30m", "1h 5m 10s".
pub fn parse_duration(value: &Value) -> Result<f64, String> {
    match value {
        Value::Bool(_) => Err(format!("invalid duration {}", yaml_repr(value))),
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| format!("invalid duration {}", yaml_repr(value))),
        Value::String(s) => parse_duration_str(s),
        _ => Err(format!(
            "invalid duration {} (examples: 90, 90s, 2m, 1h30m)",
            yaml_repr(value)
        )),
    }
}

pub fn parse_duration_str(s: &str) -> Result<f64, String> {
    let text = s.trim().to_lowercase();
    if let Some(v) = parse_python_float(&text) {
        return Ok(v);
    }
    if let Some(v) = parse_sexagesimal(&text) {
        return Ok(v);
    }
    let bad = || format!("invalid duration {s:?} (examples: 90, 90s, 2m, 1h30m)");
    if text.is_empty() {
        return Err(bad());
    }
    let caps = DURATION_RE.captures(&text).ok_or_else(bad)?;
    if (1..=3).all(|i| caps.get(i).is_none()) {
        return Err(bad());
    }
    let g = |i| caps.get(i).map(|m| m.as_str().parse::<f64>().unwrap()).unwrap_or(0.0);
    Ok(g(1) * 3600.0 + g(2) * 60.0 + g(3))
}

static SEXAGESIMAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+(?:\.[0-9_]*)?$").unwrap());

/// YAML 1.1 base-60 numbers: PyYAML read an unquoted `1:30` as 90 and `1:00:00` as 3600,
/// so configs written for the Python version may carry them in duration fields.
fn parse_sexagesimal(t: &str) -> Option<f64> {
    let t = t.trim().replace('_', "");
    if !SEXAGESIMAL_RE.is_match(&t) {
        return None;
    }
    let (sign, body) = match t.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, t.trim_start_matches('+')),
    };
    let mut total = 0.0;
    for part in body.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(sign * total)
}

/// Python float() for plain decimal strings (what YAML scalars / env values carry).
fn parse_python_float(t: &str) -> Option<f64> {
    let t = t.trim();
    if t.is_empty() || t.chars().any(|c| c.is_ascii_alphabetic() && !matches!(c, 'e' | 'E')) {
        return None;
    }
    t.replace('_', "").parse::<f64>().ok()
}

fn yaml_repr(v: &Value) -> String {
    match v {
        Value::String(s) => format!("{s:?}"),
        Value::Bool(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        other => serde_yaml::to_string(other).unwrap_or_default().trim().to_string(),
    }
}

// ------------------------------------------------------------------ ${VAR} expansion

static ENV_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").unwrap());

fn expand(value: &mut Value, environ: &BTreeMap<String, String>) {
    match value {
        Value::String(s) if s.contains("${") => {
            *s = ENV_RE
                .replace_all(s, |c: &regex::Captures<'_>| {
                    environ
                        .get(&c[1])
                        .cloned()
                        .unwrap_or_else(|| c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default())
                })
                .into_owned();
        }
        Value::Mapping(m) => m.values_mut().for_each(|value| expand(value, environ)),
        Value::Sequence(s) => s.iter_mut().for_each(|value| expand(value, environ)),
        _ => {}
    }
}

// ------------------------------------------------------------------ env overrides

pub const ENV_PREFIX: &str = "PRUSA_WATCH__";

/// `PRUSA_WATCH__A__B__C=value` -> `data["a"]["b"]["c"] = yaml(value)`.
pub fn apply_env_overrides(data: &mut Value, environ: &BTreeMap<String, String>) {
    if !data.is_mapping() {
        *data = Value::Mapping(Mapping::new());
    }
    for (name, raw) in environ {
        let Some(rest) = name.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let path: Vec<String> = rest
            .split("__")
            .filter(|p| !p.is_empty())
            .map(|p| p.to_lowercase())
            .collect();
        if path.is_empty() {
            continue;
        }
        let value = if raw.trim().is_empty() {
            Value::String(String::new())
        } else {
            serde_yaml::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.clone()))
        };
        let mut node = &mut *data;
        for key in &path[..path.len() - 1] {
            let map = node.as_mapping_mut().expect("mapping");
            let k = Value::String(key.clone());
            if !map.get(&k).is_some_and(Value::is_mapping) {
                map.insert(k.clone(), Value::Mapping(Mapping::new()));
            }
            node = map.get_mut(&k).unwrap();
        }
        node.as_mapping_mut()
            .unwrap()
            .insert(Value::String(path.last().unwrap().clone()), value);
    }
}

pub fn process_env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

// ------------------------------------------------------------------ build

/// Keys that existed in earlier versions -> where they live now.
const MOVED: &[((&str, &str), &str)] = &[
    (("decision", "action"), "escalation.policies.<name>.steps[].action"),
    (
        ("decision", "veto_window_s"),
        "escalation.policies.<name>.steps (the step 'at' times)",
    ),
    (("decision", "veto_snooze_s"), "escalation.snooze_s"),
    (
        ("decision", "schedules"),
        "escalation.schedules (with `policy:` instead of action/veto_window_s/quiet)",
    ),
    (("ntfy", "priority_warning"), "notify.warning.priority"),
    (("ntfy", "priority_failure"), "escalation step `priority`"),
    (("notify", "cooldown_s"), "notify.warning.cooldown_s"),
    (("notify", "notify_camera_down"), "notify.camera.enabled"),
];

/// Sections whose contents are free-form (validated elsewhere).
const OPAQUE: &[&str] = &["escalation"];

fn key_str(k: &Value) -> String {
    match k {
        Value::String(s) => s.clone(),
        other => yaml_repr(other),
    }
}

/// Check keys against the defaults tree and coerce values to the default's type
/// (env-expanded strings, `*_s` durations). Mirrors the Python `_build`/`_coerce`.
fn coerce_section(schema: &Mapping, data: &mut Mapping, path: &str) -> Result<(), ConfigError> {
    let section = path.rsplit('.').next().unwrap_or("");
    let keys: Vec<Value> = data.keys().cloned().collect();
    for k in keys {
        let key = key_str(&k);
        let where_ = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let Some(default) = schema.get(Value::String(key.clone())) else {
            if let Some((_, moved)) = MOVED.iter().find(|((s, n), _)| *s == section && *n == key) {
                return Err(ConfigError::Invalid(format!(
                    "Config key '{where_}' has moved to {moved} (see config.example.yaml)"
                )));
            }
            return Err(ConfigError::Invalid(format!("Unknown config key '{where_}'")));
        };
        let value = data.get_mut(&k).unwrap();
        if OPAQUE.contains(&where_.as_str()) {
            if value.is_null() {
                *value = Value::Mapping(Mapping::new());
            } else if !value.is_mapping() {
                return Err(ConfigError::Invalid(format!(
                    "Config key '{where_}': must be a mapping"
                )));
            }
            continue;
        }
        match default {
            Value::Mapping(sub) => {
                if value.is_null() {
                    data.remove(&k);
                    continue;
                }
                let Some(m) = value.as_mapping_mut() else {
                    return Err(ConfigError::Invalid(format!(
                        "Config section '{where_}' must be a mapping"
                    )));
                };
                coerce_section(sub, m, &where_)?;
            }
            _ => {
                if value.is_null() {
                    if !default.is_null() {
                        data.remove(&k); // keep the default
                    }
                    continue;
                }
                coerce_scalar(default, value, &key)
                    .map_err(|e| ConfigError::Invalid(format!("Config key '{where_}': {e}")))?;
            }
        }
    }
    Ok(())
}

fn coerce_scalar(default: &Value, value: &mut Value, key: &str) -> Result<(), String> {
    let is_float = matches!(default, Value::Number(n) if n.is_f64());
    if is_float && key.ends_with("_s") {
        *value = Value::from(parse_duration(value)?);
        return Ok(());
    }
    let Value::String(s) = value else {
        // Python stored whatever YAML produced; convert to the field's type the way it was used.
        match (default, &*value) {
            (Value::String(_), Value::Number(n)) => *value = Value::String(n.to_string()),
            (Value::String(_), Value::Bool(b)) => {
                *value = Value::String(if *b { "True".into() } else { "False".into() })
            }
            (Value::Bool(_), Value::Number(n)) => *value = Value::Bool(n.as_f64().is_some_and(|f| f != 0.0)),
            (Value::Number(d), Value::Number(n)) if !d.is_f64() && n.is_f64() => {
                let f = n.as_f64().unwrap();
                if f.fract() != 0.0 {
                    return Err(format!("expected a whole number, got {f}"));
                }
                *value = Value::from(f as i64);
            }
            (Value::Number(d), Value::Bool(b)) => {
                *value = if d.is_f64() {
                    Value::from(*b as i64 as f64)
                } else {
                    Value::from(*b as i64)
                }
            }
            _ => {}
        }
        return Ok(());
    };
    let s = s.clone();
    match default {
        Value::Bool(_) => {
            *value = Value::Bool(match s.trim().to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => return Err(format!("expected true/false (also yes/no, on/off, 1/0), got {s:?}")),
            })
        }
        Value::Number(n) if n.is_f64() => {
            *value =
                Value::from(parse_python_float(&s).ok_or_else(|| format!("could not convert string to float: {s:?}"))?)
        }
        Value::Number(_) => {
            *value = Value::from(
                s.trim()
                    .parse::<i64>()
                    .map_err(|_| format!("invalid literal for int(): {s:?}"))?,
            )
        }
        _ => {}
    }
    Ok(())
}

fn schema() -> Mapping {
    match serde_yaml::to_value(Config::default()).unwrap() {
        Value::Mapping(m) => m,
        _ => unreachable!(),
    }
}

/// Build a Config from already-parsed YAML data (after expansion/overrides).
pub fn build_config(mut data: Value) -> Result<Config, ConfigError> {
    if data.is_null() {
        data = Value::Mapping(Mapping::new());
    }
    let Some(map) = data.as_mapping_mut() else {
        return Err(ConfigError::Invalid("top level must be a mapping".into()));
    };
    coerce_section(&schema(), map, "")?;
    serde_yaml::from_value(data).map_err(|e| ConfigError::Invalid(format!("Config: {e}")))
}

/// Load `path` (if it exists), expand `${VAR}`, apply `PRUSA_WATCH__*` overrides from `environ`.
pub fn load_config(path: Option<&Path>, environ: &BTreeMap<String, String>) -> Result<Config, ConfigError> {
    let mut data = Value::Mapping(Mapping::new());
    if let Some(p) = path {
        if p.exists() {
            let text = std::fs::read_to_string(p).map_err(|e| ConfigError::Invalid(format!("{}: {e}", p.display())))?;
            data = serde_yaml::from_str(&text).map_err(|e| ConfigError::Invalid(format!("{}: {e}", p.display())))?;
            if data.is_null() {
                data = Value::Mapping(Mapping::new());
            }
            if !data.is_mapping() {
                return Err(ConfigError::Invalid(format!(
                    "{}: top level must be a mapping",
                    p.display()
                )));
            }
        } else {
            return Err(ConfigError::NotFound(p.display().to_string()));
        }
    }
    expand(&mut data, environ);
    apply_env_overrides(&mut data, environ);
    build_config(data)
}

const SECRET_KEYS: &[&str] = &["password", "token", "webhook_url", "topic", "reply_topic"];

/// Fully-resolved settings (defaults + file + env) as plain data, for `prusa-watch config`.
pub fn effective_config(cfg: &Config, redact: bool) -> Value {
    let mut v = serde_yaml::to_value(cfg).unwrap();
    let merged = crate::escalation::merge_escalation(Some(&cfg.escalation)).unwrap_or_else(|_| cfg.escalation.clone());
    v.as_mapping_mut()
        .unwrap()
        .insert("escalation".into(), Value::Mapping(merged));
    if redact {
        scrub(&mut v, "");
    }
    v
}

fn scrub(node: &mut Value, parent: &str) {
    match node {
        Value::Mapping(m) => {
            for (k, v) in m.iter_mut() {
                let key = key_str(k);
                let secret = SECRET_KEYS.contains(&key.as_str()) || (parent == "webhook" && key == "url");
                if secret && matches!(v, Value::String(s) if !s.is_empty()) {
                    *v = Value::String("***".into());
                } else {
                    scrub(v, &key);
                }
            }
        }
        Value::Sequence(s) => s.iter_mut().for_each(|v| scrub(v, parent)),
        _ => {}
    }
}
