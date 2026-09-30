//! Time-of-day schedule primitives (which escalation policy applies when).

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDateTime, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde_yaml::Value;

pub const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
#[error("{0}")]
pub struct ScheduleError(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleRule {
    pub name: String,
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub policy: String,
    /// weekday numbers (Mon=0); None = every day
    pub days: Option<BTreeSet<u32>>,
}

impl ScheduleRule {
    pub fn matches(&self, now: NaiveDateTime) -> bool {
        let t = now.time();
        if self.start == self.end {
            return self.day_ok(now);
        }
        if self.start < self.end {
            return self.start <= t && t < self.end && self.day_ok(now);
        }
        // crosses midnight: the part after midnight belongs to the previous day's window
        if t >= self.start {
            return self.day_ok(now);
        }
        if t < self.end {
            return self.day_ok(now - Duration::days(1));
        }
        false
    }

    fn day_ok(&self, dt: NaiveDateTime) -> bool {
        self.days
            .as_ref()
            .is_none_or(|d| d.contains(&dt.weekday().num_days_from_monday()))
    }
}

fn scalar_str(v: &Value) -> String {
    match v {
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
        other => serde_yaml::to_string(other).unwrap_or_default().trim().to_string(),
    }
}

fn parse_time(v: &Value, where_: &str) -> Result<NaiveTime, ScheduleError> {
    let s = scalar_str(v);
    let err = || ScheduleError(format!("{where_}: invalid time {} (use \"HH:MM\")", py_repr(v)));
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() != 2 {
        return Err(err());
    }
    let hh: u32 = parts[0].trim().parse().map_err(|_| err())?;
    let mm: u32 = parts[1].trim().parse().map_err(|_| err())?;
    NaiveTime::from_hms_opt(hh, mm, 0).ok_or_else(err)
}

pub(crate) fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => format!("'{s}'"),
        other => scalar_str(other),
    }
}

pub fn parse_schedules(raw: Option<&Value>, policies: &BTreeSet<String>) -> Result<Vec<ScheduleRule>, ScheduleError> {
    let items = match raw {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::Sequence(s)) => s,
        Some(_) => return Err(ScheduleError("escalation.schedules must be a list".into())),
    };
    let mut rules = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let where_ = format!("escalation.schedules[{i}]");
        let Some(m) = item.as_mapping() else {
            return Err(ScheduleError(format!("{where_}: must be a mapping")));
        };
        let mut unknown: Vec<String> = m
            .keys()
            .map(scalar_str)
            .filter(|k| !["name", "start", "end", "days", "policy"].contains(&k.as_str()))
            .collect();
        if !unknown.is_empty() {
            unknown.sort();
            return Err(ScheduleError(format!(
                "{where_}: unknown keys {} (a schedule only picks a `policy`)",
                py_list(&unknown)
            )));
        }
        for k in ["start", "end", "policy"] {
            if !m.contains_key(k) {
                return Err(ScheduleError(format!("{where_}: `{k}` is required")));
            }
        }
        let policy = scalar_str(&m["policy"]);
        if !policies.contains(&policy) {
            return Err(ScheduleError(format!(
                "{where_}: policy {} is not defined in escalation.policies",
                py_repr(&m["policy"])
            )));
        }
        let days = match m.get("days") {
            Some(Value::Sequence(ds)) if !ds.is_empty() => {
                let mut set = BTreeSet::new();
                for d in ds {
                    let s = scalar_str(d).trim().to_lowercase();
                    let key: String = s.chars().take(3).collect();
                    let idx = DAYS
                        .iter()
                        .position(|x| *x == key)
                        .ok_or_else(|| ScheduleError(format!("{where_}: days must be from {}", py_list(&DAYS))))?;
                    set.insert(idx as u32);
                }
                Some(set)
            }
            // Python iterates a string's characters, which never match a 3-letter day name.
            Some(v) if is_truthy(v) => {
                return Err(ScheduleError(format!("{where_}: days must be from {}", py_list(&DAYS))));
            }
            _ => None,
        };
        let name = match m.get("name") {
            Some(v) if is_truthy(v) => scalar_str(v),
            _ => format!("schedule-{i}"),
        };
        rules.push(ScheduleRule {
            name,
            start: parse_time(&m["start"], &where_)?,
            end: parse_time(&m["end"], &where_)?,
            policy,
            days,
        });
    }
    Ok(rules)
}

fn is_truthy(v: &Value) -> bool {
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

pub(crate) fn py_list<S: AsRef<str>>(items: &[S]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| format!("'{}'", s.as_ref()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Empty name = system local time.
pub fn resolve_tz(name: &str) -> Result<Option<Tz>, ScheduleError> {
    if name.is_empty() {
        return Ok(None);
    }
    name.parse::<Tz>()
        .map(Some)
        .map_err(|_| ScheduleError(format!("timezone: unknown IANA zone '{name}'")))
}

/// Wall-clock time at unix timestamp `ts` in `tz` (or system local time).
pub fn local_now(ts: f64, tz: Option<Tz>) -> NaiveDateTime {
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()) * 1e9) as u32;
    match tz {
        Some(tz) => {
            let utc = DateTime::from_timestamp(secs, nanos).unwrap_or_default();
            utc.with_timezone(&tz).naive_local()
        }
        None => Local
            .timestamp_opt(secs, nanos)
            .single()
            .map(|d| d.naive_local())
            .unwrap_or_default(),
    }
}
