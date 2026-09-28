//! Escalation policies: what happens after the detector says "failure".
//!
//! A policy is an ordered list of timed steps, measured from the moment the
//! failure is detected (an *incident*). Each step can notify (which channels,
//! what priority, which buttons, what text) and/or act on the printer (pause /
//! stop). Schedules pick which policy applies by time of day.
//!
//! Sensible defaults are built in (`BUILTIN` below): ask first and pause after
//! 2 min during the day, pause silently at night (22:00-07:00). Your config
//! only needs the parts you want to change.
//!
//! An incident ends when you answer (keep / act / stop / resume / mute), when
//! you handle it at the printer, when the job ends, or when its steps run out
//! (unless the printer is sitting paused by us, in which case it stays open so
//! the Resume buttons keep working).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};

use base64::Engine;
use chrono_tz::Tz;
use serde_json::json;
use serde_yaml::{Mapping, Value};

use crate::config::{CHANNELS, parse_duration};
use crate::policy::{ScheduleRule, local_now, parse_schedules, py_list, py_repr, resolve_tz};

pub const ACTIONS: [&str; 2] = ["pause", "stop"];

/// button -> (reply command, default label)
pub fn button(name: &str) -> Option<(Option<&'static str>, Option<&'static str>)> {
    Some(match name {
        "keep" => (Some("veto"), Some("Keep printing")),
        "act" => (Some("act"), None), // label depends on the next action: "Pause now" / "Stop now"
        "stop" => (Some("stop"), Some("Cancel print")),
        "resume" => (Some("resume"), Some("Resume")),
        "mute" => (Some("mute"), Some("False alarm: resume + mute")),
        "dashboard" => (None, Some("Dashboard")), // opens web.public_url
        _ => return None,
    })
}

pub const BUTTON_NAMES: [&str; 6] = ["keep", "act", "stop", "resume", "mute", "dashboard"];

pub const TEMPLATE_FIELDS: [(&str, &str); 9] = [
    ("printer", "core-one"),
    ("job", "benchy.bgcode"),
    ("score", "0.71"),
    ("policy", "ask_first"),
    ("schedule", "night"),
    ("next_action", "pause"),
    ("next_action_in", "1:30"),
    ("elapsed", "0:30"),
    ("action_taken", "paused"),
];

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
#[error("{0}")]
pub struct EscalationError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, EscalationError> {
    Err(EscalationError(msg.into()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// seconds after the incident started
    pub at: f64,
    /// "pause" | "stop" | None (notify only)
    pub action: Option<String>,
    /// channels; None = all configured, [] = silent
    pub notify: Option<Vec<String>>,
    /// ntfy 1..5
    pub priority: i64,
    /// None = automatic for the incident's state
    pub buttons: Option<Vec<String>>,
    pub title: Option<String>,
    pub message: Option<String>,
    pub attach_image: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub name: String,
    pub steps: Vec<Step>,
}

impl Policy {
    pub fn has_action(&self) -> bool {
        self.steps.iter().any(|s| s.action.is_some())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EscalationConfig {
    /// in definition order
    pub policies: Vec<(String, Arc<Policy>)>,
    pub default_policy: String,
    pub schedules: Vec<ScheduleRule>,
    pub snooze_s: f64,
}

impl EscalationConfig {
    pub fn policy(&self, name: &str) -> Option<&Arc<Policy>> {
        self.policies.iter().find(|(n, _)| n == name).map(|(_, p)| p)
    }
}

/// Built-in defaults. Your `escalation:` section is merged on top of this:
///   - policies merge BY NAME: define `ask_first` to replace that one policy, or add new names
///   - `schedules`, if you set it, replaces the list ([] turns the night schedule off)
///   - `default_policy` / `snooze_s` override
///
/// Deliberately non-destructive: nothing here ever cancels a print.
pub const BUILTIN_YAML: &str = r#"
default_policy: ask_first
snooze_s: 30m
policies:
  # Ask first, remind once, pause after 2 min of silence.
  ask_first:
    steps:
      - {at: 0, priority: 5, buttons: [keep, act, stop]}
      - {at: 1m, priority: 5, title: "{printer}: still failing, {next_action} in {next_action_in}", attach_image: false}
      - {at: 2m, action: pause, priority: 5}
  # Pause immediately, loud.
  pause_now:
    steps:
      - {at: 0, action: pause, priority: 5}
  # Pause immediately, silent notification (you'll see it in the morning).
  night:
    steps:
      - {at: 0, action: pause, priority: 2}
  # Never touch the printer; just tell me.
  watch_only:
    steps:
      - {at: 0, priority: 4, buttons: [stop, dashboard]}
schedules:
  - {name: night, start: "22:00", end: "07:00", policy: night}
"#;

pub static BUILTIN: LazyLock<Mapping> = LazyLock::new(|| serde_yaml::from_str(BUILTIN_YAML).unwrap());

const TOP_KEYS: [&str; 4] = ["default_policy", "policies", "schedules", "snooze_s"];

fn kstr(k: &Value) -> String {
    match k {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        Value::Null => "None".into(),
        _ => serde_yaml::to_string(k).unwrap_or_default().trim().into(),
    }
}

fn unknown_keys(m: &Mapping, allowed: &[&str]) -> Vec<String> {
    let mut u: Vec<String> = m.keys().map(kstr).filter(|k| !allowed.contains(&k.as_str())).collect();
    u.sort();
    u
}

/// Built-in escalation with the user's section layered on top (see BUILTIN).
pub fn merge_escalation(user: Option<&Mapping>) -> Result<Mapping, EscalationError> {
    let empty = Mapping::new();
    let user = user.unwrap_or(&empty);
    let unknown = unknown_keys(user, &TOP_KEYS);
    if !unknown.is_empty() {
        return err(format!("escalation: unknown keys {}", py_list(&unknown)));
    }
    let mut merged = BUILTIN.clone();
    for key in ["default_policy", "snooze_s", "schedules"] {
        if let Some(v) = user.get(key) {
            let nv = if v.is_null() {
                if key == "schedules" { Value::Sequence(vec![]) } else { merged[key].clone() }
            } else {
                v.clone()
            };
            merged.insert(key.into(), nv);
        }
    }
    let pols = match user.get("policies") {
        None | Some(Value::Null) => Mapping::new(),
        Some(Value::Mapping(m)) => m.clone(),
        Some(v) if !is_truthy(v) => Mapping::new(),
        Some(_) => return err("escalation.policies must be a mapping of name -> {steps: [...]}"),
    };
    let merged_pols = merged.get_mut("policies").unwrap().as_mapping_mut().unwrap();
    for (name, body) in pols {
        let name = Value::String(kstr(&name));
        if body.is_null() {
            merged_pols.shift_remove(&name); // `name: null` removes a built-in policy (keeping order)
        } else {
            merged_pols.insert(name, body);
        }
    }
    Ok(merged)
}

/// PyYAML (YAML 1.1) reads yes/no/on/off/true/false in any common casing as booleans;
/// serde_yaml (YAML 1.2) keeps them as strings. Configs written for the Python
/// version rely on the 1.1 reading, so honour it where a boolean is meaningful.
pub fn yaml11_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.as_str() {
            "yes" | "Yes" | "YES" | "on" | "On" | "ON" | "true" | "True" | "TRUE" => Some(true),
            "no" | "No" | "NO" | "off" | "Off" | "OFF" | "false" | "False" | "FALSE" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Sequence(s) => !s.is_empty(),
        Value::Mapping(m) => !m.is_empty(),
        Value::Tagged(_) => true,
    }
}

// ------------------------------------------------------------------ templates

/// Python `str.format(**fields)` for the subset that makes sense in notification
/// text: `{name}`, `{name:[[fill]align][width]}`, `{{` and `}}`.
pub fn format_template(t: &str, fields: &BTreeMap<&str, String>) -> Result<String, String> {
    let mut out = String::new();
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if chars.get(i + 1) == Some(&'{') {
                out.push('{');
                i += 2;
                continue;
            }
            let end = chars[i + 1..].iter().position(|&x| x == '}').ok_or("Single '{' encountered in format string")?;
            let inner: String = chars[i + 1..i + 1 + end].iter().collect();
            if inner.contains('{') {
                return Err("unexpected '{' in field name".into());
            }
            let (name, spec) = match inner.split_once(':') {
                Some((n, s)) => (n, Some(s)),
                None => (inner.as_str(), None),
            };
            if name.contains('!') {
                return Err(format!("conversions are not supported: {{{inner}}}"));
            }
            if name.is_empty() || name.chars().all(|c| c.is_ascii_digit()) {
                return Err("Replacement index 0 out of range for positional args tuple".into());
            }
            let val = fields.get(name).ok_or_else(|| format!("'{name}'"))?;
            out.push_str(&apply_spec(val, spec)?);
            i += end + 2;
        } else if c == '}' {
            if chars.get(i + 1) == Some(&'}') {
                out.push('}');
                i += 2;
                continue;
            }
            return Err("Single '}' encountered in format string".into());
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(out)
}

fn apply_spec(val: &str, spec: Option<&str>) -> Result<String, String> {
    let Some(spec) = spec.filter(|s| !s.is_empty()) else { return Ok(val.to_string()) };
    let chars: Vec<char> = spec.chars().collect();
    let (fill, align, rest) = if chars.len() >= 2 && matches!(chars[1], '<' | '>' | '^') {
        (chars[0], chars[1], &chars[2..])
    } else if matches!(chars[0], '<' | '>' | '^') {
        (' ', chars[0], &chars[1..])
    } else {
        (' ', '<', &chars[..])
    };
    let width: usize = if rest.is_empty() {
        0
    } else {
        rest.iter()
            .collect::<String>()
            .parse()
            .map_err(|_| format!("Invalid format specifier '{spec}' for object of type 'str'"))?
    };
    let len = val.chars().count();
    if len >= width {
        return Ok(val.to_string());
    }
    let pad = width - len;
    let f = |n: usize| std::iter::repeat_n(fill, n).collect::<String>();
    Ok(match align {
        '>' => format!("{}{val}", f(pad)),
        '^' => format!("{}{val}{}", f(pad / 2), f(pad - pad / 2)),
        _ => format!("{val}{}", f(pad)),
    })
}

fn template_example_fields() -> BTreeMap<&'static str, String> {
    TEMPLATE_FIELDS.iter().map(|(k, v)| (*k, v.to_string())).collect()
}

fn check_template(t: Option<&Value>, where_: &str) -> Result<Option<String>, EscalationError> {
    let Some(t) = t.filter(|v| !v.is_null()) else { return Ok(None) };
    let t = kstr(t);
    if let Err(e) = format_template(&t, &template_example_fields()) {
        let fields: Vec<&str> = {
            let mut f: Vec<&str> = TEMPLATE_FIELDS.iter().map(|(k, _)| *k).collect();
            f.sort();
            f
        };
        return err(format!("{where_}: bad template '{t}' ({e}); fields: {}", py_list(&fields)));
    }
    Ok(Some(t))
}

// ------------------------------------------------------------------ parsing

fn str_list(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Sequence(s) => Some(s.iter().map(kstr).collect()),
        _ => None,
    }
}

fn parse_step(raw: &Value, where_: &str) -> Result<Step, EscalationError> {
    let Some(m) = raw.as_mapping() else { return err(format!("{where_}: must be a mapping")) };
    let unknown = unknown_keys(m, &["at", "action", "notify", "priority", "buttons", "title", "message", "attach_image"]);
    if !unknown.is_empty() {
        return err(format!("{where_}: unknown keys {}", py_list(&unknown)));
    }
    let at = parse_duration(m.get("at").unwrap_or(&Value::from(0))).map_err(|e| EscalationError(format!("{where_}.at: {e}")))?;
    if at < 0.0 {
        return err(format!("{where_}.at must be >= 0"));
    }
    let action = match m.get("action") {
        None | Some(Value::Null) => None,
        Some(v) if yaml11_bool(v) == Some(false) => None,
        Some(Value::String(s)) if s == "none" || s == "notify" => None,
        Some(Value::String(s)) if ACTIONS.contains(&s.as_str()) => Some(s.clone()),
        Some(_) => return err(format!("{where_}.action must be one of {} or none", py_list(&ACTIONS))),
    };
    let notify = match m.get("notify") {
        None | Some(Value::Null) => None,
        Some(v) if yaml11_bool(v) == Some(true) => None,
        Some(v) if yaml11_bool(v) == Some(false) => Some(vec![]),
        Some(v) => {
            let list =
                str_list(v).ok_or_else(|| EscalationError(format!("{where_}.notify: must be a channel or list of channels")))?;
            let mut bad: Vec<String> = list.iter().filter(|c| !CHANNELS.contains(&c.as_str())).cloned().collect();
            if !bad.is_empty() {
                bad.sort();
                bad.dedup();
                return err(format!("{where_}.notify: unknown channels {} (use {})", py_list(&bad), py_list(&CHANNELS)));
            }
            Some(list)
        }
    };
    let priority = match m.get("priority") {
        None => 5,
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)).unwrap_or(0),
        Some(Value::String(s)) => {
            s.trim().parse::<i64>().map_err(|_| EscalationError(format!("{where_}.priority must be 1..5")))?
        }
        Some(Value::Bool(b)) => *b as i64,
        Some(_) => return err(format!("{where_}.priority must be 1..5")),
    };
    if !(1..=5).contains(&priority) {
        return err(format!("{where_}.priority must be 1..5"));
    }
    let buttons = match m.get("buttons") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let list = str_list(v).ok_or_else(|| EscalationError(format!("{where_}.buttons: must be a list")))?;
            let mut bad: Vec<String> = list.iter().filter(|b| button(b).is_none()).cloned().collect();
            if !bad.is_empty() {
                bad.sort();
                bad.dedup();
                return err(format!("{where_}.buttons: unknown {} (use {})", py_list(&bad), py_list(&BUTTON_NAMES)));
            }
            if list.len() > 3 {
                return err(format!("{where_}.buttons: ntfy allows at most 3"));
            }
            Some(list)
        }
    };
    Ok(Step {
        at,
        action,
        notify,
        priority,
        buttons,
        title: check_template(m.get("title"), &format!("{where_}.title"))?,
        message: check_template(m.get("message"), &format!("{where_}.message"))?,
        attach_image: m.get("attach_image").is_none_or(|v| yaml11_bool(v).unwrap_or_else(|| is_truthy(v))),
    })
}

/// Parse the user's escalation section merged over BUILTIN (builtin=false: parse as-is).
pub fn parse_escalation(user: Option<&Mapping>, builtin: bool) -> Result<EscalationConfig, EscalationError> {
    let raw = if builtin { merge_escalation(user)? } else { user.cloned().unwrap_or_default() };
    let unknown = unknown_keys(&raw, &TOP_KEYS);
    if !unknown.is_empty() {
        return err(format!("escalation: unknown keys {}", py_list(&unknown)));
    }
    let pols_raw = match raw.get("policies") {
        Some(Value::Mapping(m)) if !m.is_empty() => m.clone(),
        _ => return err("escalation.policies must define at least one policy"),
    };
    let mut policies: Vec<(String, Arc<Policy>)> = Vec::new();
    for (name, body) in &pols_raw {
        let name = kstr(name);
        let where_ = format!("escalation.policies.{name}");
        let steps_raw = match body {
            Value::Mapping(b) => b.get("steps").cloned().unwrap_or(Value::Null),
            other => other.clone(),
        };
        let steps_list = match &steps_raw {
            Value::Sequence(s) if !s.is_empty() => s,
            _ => return err(format!("{where_}.steps must be a non-empty list")),
        };
        if let Value::Mapping(b) = body {
            let extra = unknown_keys(b, &["steps"]);
            if !extra.is_empty() {
                return err(format!("{where_}: unknown keys {}", py_list(&extra)));
            }
        }
        let steps = steps_list
            .iter()
            .enumerate()
            .map(|(i, s)| parse_step(s, &format!("{where_}.steps[{i}]")))
            .collect::<Result<Vec<_>, _>>()?;
        if steps.windows(2).any(|w| w[1].at < w[0].at) {
            return err(format!("{where_}.steps must be in ascending `at` order"));
        }
        policies.push((name.clone(), Arc::new(Policy { name, steps })));
    }
    let default = match raw.get("default_policy") {
        Some(v) if is_truthy(v) => kstr(v),
        _ => policies[0].0.clone(),
    };
    if !policies.iter().any(|(n, _)| *n == default) {
        return err(format!("escalation.default_policy '{default}' is not defined in escalation.policies"));
    }
    let names: BTreeSet<String> = policies.iter().map(|(n, _)| n.clone()).collect();
    let schedules = parse_schedules(raw.get("schedules"), &names).map_err(|e| EscalationError(e.0))?;
    let snooze = parse_duration(raw.get("snooze_s").unwrap_or(&Value::from(1800))).map_err(EscalationError)?;
    if snooze < 0.0 {
        return err("escalation.snooze_s must be >= 0");
    }
    Ok(EscalationConfig { policies, default_policy: default, schedules, snooze_s: snooze })
}

pub struct PolicyResolver {
    pub esc: EscalationConfig,
    pub tz: Option<Tz>,
}

impl PolicyResolver {
    pub fn new(esc: EscalationConfig, tz: &str) -> Result<Self, EscalationError> {
        Ok(Self { esc, tz: resolve_tz(tz).map_err(|e| EscalationError(e.0))? })
    }

    pub fn resolve(&self, ts: f64) -> (Arc<Policy>, Option<String>) {
        let now = local_now(ts, self.tz);
        for rule in &self.esc.schedules {
            if rule.matches(now) {
                return (self.esc.policy(&rule.policy).unwrap().clone(), Some(rule.name.clone()));
            }
        }
        (self.esc.policy(&self.esc.default_policy).unwrap().clone(), None)
    }
}

/// "m:ss" or "h:mm:ss", rounding like Python's round() (half to even).
pub fn fmt_duration(seconds: f64) -> String {
    let s = seconds.round_ties_even().max(0.0) as i64;
    let (h, rem) = (s / 3600, s % 3600);
    let (m, sec) = (rem / 60, rem % 60);
    if h > 0 { format!("{h}:{m:02}:{sec:02}") } else { format!("{m}:{sec:02}") }
}

pub fn new_incident_id() -> String {
    let bytes: [u8; 6] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Debug, Clone)]
pub struct Incident {
    pub policy: Arc<Policy>,
    pub schedule: Option<String>,
    pub job_id: Option<i64>,
    pub started_ts: f64,
    pub score: f64,
    pub jpeg: Arc<Vec<u8>>,
    pub id: String,
    /// first step not yet executed
    pub next_idx: usize,
    /// "paused" | "stopped" once we acted on the printer
    pub acted: Option<String>,
    /// failed pause/stop attempts (retried every poll)
    pub action_errors: u32,
    /// step to run on the next poll regardless of its `at` (failed "act now")
    pub retry_idx: Option<usize>,
}

impl Incident {
    pub fn new(
        policy: Arc<Policy>,
        schedule: Option<String>,
        job_id: Option<i64>,
        started_ts: f64,
        score: f64,
        jpeg: Vec<u8>,
    ) -> Self {
        Self {
            policy,
            schedule,
            job_id,
            started_ts,
            score,
            jpeg: Arc::new(jpeg),
            id: new_incident_id(),
            next_idx: 0,
            acted: None,
            action_errors: 0,
            retry_idx: None,
        }
    }

    pub fn due(&self, now: f64) -> Vec<(usize, Step)> {
        let mut out = Vec::new();
        for i in self.next_idx..self.policy.steps.len() {
            let s = &self.policy.steps[i];
            if self.started_ts + s.at <= now || Some(i) == self.retry_idx {
                out.push((i, s.clone()));
            } else {
                break;
            }
        }
        out
    }

    pub fn next_action(&self) -> Option<(usize, &Step)> {
        (self.next_idx..self.policy.steps.len())
            .find(|&i| self.policy.steps[i].action.is_some())
            .map(|i| (i, &self.policy.steps[i]))
    }

    pub fn steps_done(&self) -> bool {
        self.next_idx >= self.policy.steps.len()
    }

    /// Reply commands that make sense right now.
    pub fn commands(&self) -> Vec<&'static str> {
        match self.acted.as_deref() {
            Some("paused") => vec!["resume", "mute", "stop", "veto"],
            Some(_) => vec![],
            None if self.next_action().is_some() => vec!["veto", "act", "stop"],
            None => vec!["veto", "stop"],
        }
    }

    pub fn default_buttons(&self) -> Vec<String> {
        let b: &[&str] = match self.acted.as_deref() {
            Some("paused") => &["resume", "mute", "stop"],
            Some(_) => &["dashboard"],
            None if self.next_action().is_some() => &["keep", "act", "stop"],
            None => &["keep", "stop", "dashboard"],
        };
        b.iter().map(|s| s.to_string()).collect()
    }

    pub fn template_fields(&self, now: f64, printer: &str, job: Option<&str>, score: f64) -> BTreeMap<&'static str, String> {
        let na = self.next_action();
        let mut f = BTreeMap::new();
        f.insert("printer", printer.to_string());
        f.insert(
            "job",
            job.filter(|j| !j.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| self.job_id.map(|j| j.to_string()).unwrap_or_else(|| "print".into())),
        );
        f.insert("score", format!("{score:.2}"));
        f.insert("policy", self.policy.name.clone());
        f.insert("schedule", self.schedule.clone().unwrap_or_default());
        f.insert("next_action", na.map(|(_, s)| s.action.clone().unwrap()).unwrap_or_default());
        f.insert("next_action_in", na.map(|(_, s)| fmt_duration(self.started_ts + s.at - now)).unwrap_or_default());
        f.insert("elapsed", fmt_duration(now - self.started_ts));
        f.insert("action_taken", self.acted.clone().unwrap_or_default());
        f
    }

    pub fn public(&self, now: f64) -> serde_json::Value {
        let na = self.next_action();
        json!({
            "id": self.id,
            "policy": self.policy.name,
            "schedule": self.schedule,
            "job_id": self.job_id,
            "started_ts": self.started_ts,
            "elapsed_s": now - self.started_ts,
            "score": self.score,
            "acted": self.acted,
            "next_action": na.map(|(_, s)| s.action.clone()),
            "next_action_ts": na.map(|(_, s)| self.started_ts + s.at),
            "next_action_in_s": na.map(|(_, s)| (self.started_ts + s.at - now).max(0.0)),
            "commands": self.commands(),
            "steps": self.policy.steps.iter().enumerate().map(|(i, s)| json!({
                "at": s.at, "action": s.action, "notify": s.notify, "done": i < self.next_idx,
            })).collect::<Vec<_>>(),
        })
    }
}

#[allow(dead_code)]
pub(crate) fn repr(v: &Value) -> String {
    py_repr(v)
}
