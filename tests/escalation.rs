//! Port of tests/test_escalation.py: escalation policies, schedules, durations,
//! config loading/validation around them, and the `config` CLI command.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::TimeZone;
use prusa_watch::config::{Config, load_config, parse_duration};
use prusa_watch::escalation::{EscalationConfig, Incident, PolicyResolver, parse_escalation};
use serde_yaml::{Mapping, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Parse a Python-dict-shaped literal (written as JSON/YAML) into a mapping.
fn m(text: &str) -> Mapping {
    serde_yaml::from_str(text).unwrap()
}

fn parse(text: &str) -> Result<EscalationConfig, String> {
    parse_escalation(Some(&m(text)), true).map_err(|e| e.0)
}

fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> f64 {
    chrono_tz::America::New_York.with_ymd_and_hms(y, mo, d, h, mi, 0).earliest().unwrap().timestamp() as f64
}

const BASE: &str = r#"{
  "default_policy": "day",
  "policies": {
    "day": {"steps": [{"at": 0, "buttons": ["keep", "act", "stop"]}, {"at": "2m", "action": "pause"}]},
    "night": {"steps": [{"at": 0, "action": "pause", "priority": 2}]},
    "work": {"steps": [{"at": 0}, {"at": "5m", "action": "pause"}]},
    "all": {"steps": [{"at": 0}]}
  }
}"#;

const NIGHT: &str = r#"{"name": "night", "start": "22:00", "end": "07:00", "policy": "night"}"#;
const WORK: &str =
    r#"{"name": "work", "days": ["mon", "tue", "wed", "thu", "fri"], "start": "09:00", "end": "17:00", "policy": "work"}"#;

fn resolver(schedules: &[&str]) -> PolicyResolver {
    let mut base = m(BASE);
    let list: Vec<Value> = schedules.iter().map(|s| serde_yaml::from_str(s).unwrap()).collect();
    base.insert("schedules".into(), Value::Sequence(list));
    PolicyResolver::new(parse_escalation(Some(&base), true).unwrap(), "America/New_York").unwrap()
}

fn names(esc: &EscalationConfig) -> BTreeSet<String> {
    esc.policies.iter().map(|(n, _)| n.clone()).collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn builtin_defaults() {
    let esc = parse_escalation(Some(&Mapping::new()), true).unwrap();
    assert!(esc.default_policy == "ask_first" && esc.snooze_s == 1800.0);
    assert_eq!(names(&esc), set(&["ask_first", "pause_now", "night", "watch_only"]));
    let ask = esc.policy("ask_first").unwrap();
    let steps: Vec<(f64, Option<&str>)> = ask.steps.iter().map(|s| (s.at, s.action.as_deref())).collect();
    assert_eq!(steps, vec![(0.0, None), (60.0, None), (120.0, Some("pause"))]);
    // built-ins never cancel
    assert!(!esc.policies.iter().any(|(_, p)| p.steps.iter().any(|s| s.action.as_deref() == Some("stop"))));
    let sch: Vec<(&str, &str)> = esc.schedules.iter().map(|r| (r.name.as_str(), r.policy.as_str())).collect();
    assert_eq!(sch, vec![("night", "night")]);
}

#[test]
fn user_section_merges_by_policy_name() {
    let esc = parse(
        r#"{"policies": {"ask_first": {"steps": [{"at": 0}, {"at": "5m", "action": "pause"}]}, "mine": {"steps": [{"at": 0}]}}}"#,
    )
    .unwrap();
    let at: Vec<f64> = esc.policy("ask_first").unwrap().steps.iter().map(|s| s.at).collect();
    assert_eq!(at, vec![0.0, 300.0]); // replaced wholesale
    assert!(set(&["pause_now", "night", "watch_only", "mine"]).is_subset(&names(&esc))); // others kept
    let sch: Vec<&str> = esc.schedules.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(sch, vec!["night"]); // schedules untouched when not given
}

#[test]
fn schedules_replace_and_policies_can_be_removed() {
    let esc = parse(r#"{"schedules": [], "policies": {"night": null}, "default_policy": "watch_only"}"#).unwrap();
    assert!(esc.schedules.is_empty() && !names(&esc).contains("night") && esc.default_policy == "watch_only");
    // removed policy still scheduled
    let e = parse(r#"{"policies": {"night": null}}"#).unwrap_err();
    assert!(e.contains("not defined"), "{e}");
}

#[test]
fn builtin_false_parses_as_is() {
    let esc = parse_escalation(Some(&m(r#"{"policies": {"p": {"steps": [{}]}}}"#)), false).unwrap();
    let n: Vec<&str> = esc.policies.iter().map(|(n, _)| n.as_str()).collect();
    assert!(n == vec!["p"] && esc.schedules.is_empty());
}

#[test]
fn default_policy_when_no_schedule_matches() {
    let (pol, sched) = resolver(&[NIGHT]).resolve(ts(2026, 9, 26, 14, 0));
    assert_eq!((pol.name.as_str(), sched), ("day", None));
}

#[test]
fn overnight_window_boundaries() {
    let cases: [(u32, u32, Option<&str>); 6] = [
        (21, 59, None),
        (22, 0, Some("night")),
        (23, 30, Some("night")),
        (0, 0, Some("night")),
        (6, 59, Some("night")),
        (7, 0, None),
    ];
    let r = resolver(&[NIGHT]);
    for (h, mi, expect) in cases {
        assert_eq!(r.resolve(ts(2026, 9, 26, h, mi)).1.as_deref(), expect, "{h:02}:{mi:02}");
    }
}

#[test]
fn days_filter_and_overnight_day_attribution() {
    let fri = r#"{"name": "fri-night", "days": ["fri"], "start": "22:00", "end": "07:00", "policy": "night"}"#;
    let r = resolver(&[fri]);
    assert_eq!(r.resolve(ts(2026, 9, 25, 23, 0)).1.as_deref(), Some("fri-night")); // Fri 23:00
    assert_eq!(r.resolve(ts(2026, 9, 26, 3, 0)).1.as_deref(), Some("fri-night")); // Sat 03:00 belongs to Fri's window
    assert_eq!(r.resolve(ts(2026, 9, 26, 23, 0)).1, None); // Sat 23:00
}

#[test]
fn first_match_wins_and_full_day_rule() {
    let r = resolver(&[WORK, r#"{"name": "weekend", "start": "00:00", "end": "00:00", "policy": "all"}"#]);
    assert_eq!(r.resolve(ts(2026, 9, 28, 10, 0)).0.name, "work"); // Monday
    assert_eq!(r.resolve(ts(2026, 9, 26, 10, 0)).0.name, "all"); // Saturday
}

#[test]
fn timezone_is_respected() {
    let utc3 = chrono::Utc.with_ymd_and_hms(2026, 9, 27, 3, 0, 0).unwrap().timestamp() as f64; // 23:00 New York
    assert_eq!(resolver(&[NIGHT]).resolve(utc3).1.as_deref(), Some("night"));
}

#[test]
fn durations() {
    let cases: Vec<(Value, f64)> = vec![
        (Value::from(90), 90.0),
        (Value::from("90"), 90.0),
        (Value::from("90s"), 90.0),
        (Value::from("2m"), 120.0),
        (Value::from("1h30m"), 5400.0),
        (Value::from("1h 5m 10s"), 3910.0),
        (Value::from(1.5), 1.5),
        (Value::from("0"), 0.0),
    ];
    for (v, sec) in cases {
        assert_eq!(parse_duration(&v), Ok(sec), "{v:?}");
    }
}

#[test]
fn bad_duration() {
    assert!(parse_duration(&Value::from("soon")).is_err());
}

#[test]
fn validation() {
    let cases: &[(&str, &str)] = &[
        (
            r#"{"policies": {"ask_first": null, "pause_now": null, "night": null, "watch_only": null}, "schedules": []}"#,
            "at least one policy",
        ),
        (r#"{"policies": {"p": {"steps": []}}}"#, "non-empty"),
        (r#"{"policies": {"p": {"steps": [{"at": "2m"}, {"at": "1m"}]}}}"#, "ascending"),
        (r#"{"policies": {"p": {"steps": [{"action": "explode"}]}}}"#, "action"),
        (r#"{"policies": {"p": {"steps": [{"notify": ["sms"]}]}}}"#, "unknown channels"),
        (r#"{"policies": {"p": {"steps": [{"priority": 9}]}}}"#, "priority"),
        (r#"{"policies": {"p": {"steps": [{"buttons": ["keep", "act", "stop", "mute"]}]}}}"#, "at most 3"),
        (r#"{"policies": {"p": {"steps": [{"buttons": ["launch"]}]}}}"#, "unknown"),
        (r#"{"policies": {"p": {"steps": [{"title": "{nope}"}]}}}"#, "bad template"),
        (r#"{"policies": {"p": {"steps": [{"at": "later"}]}}}"#, "invalid duration"),
        (r#"{"policies": {"p": {"steps": [{"when": 0}]}}}"#, "unknown keys"),
        (r#"{"default_policy": "q", "policies": {"p": {"steps": [{}]}}}"#, "default_policy"),
        (
            r#"{"policies": {"p": {"steps": [{}]}}, "schedules": [{"start": "22:00", "end": "07:00", "policy": "zzz"}]}"#,
            "not defined",
        ),
        (
            r#"{"policies": {"p": {"steps": [{}]}}, "schedules": [{"start": "25:99", "end": "07:00", "policy": "p"}]}"#,
            "invalid time",
        ),
        (
            r#"{"policies": {"p": {"steps": [{}]}}, "schedules": [{"start": "1:00", "end": "2:00", "policy": "p", "action": "pause"}]}"#,
            "only picks a `policy`",
        ),
        (r#"{"policies": {"p": {"steps": [{}]}}, "retries": 3}"#, "unknown keys"),
    ];
    for (esc, msg) in cases {
        match parse(esc) {
            Ok(_) => panic!("expected error matching {msg:?} for {esc}"),
            Err(e) => assert!(e.contains(msg), "{esc}: {e:?} does not contain {msg:?}"),
        }
    }
}

#[test]
fn incident_navigation() {
    let esc = parse(BASE).unwrap();
    let pol = esc.policy("day").unwrap().clone();
    let mut inc = Incident::new(pol, None, Some(1), 1000.0, 0.7, Vec::new());
    let idx = |v: Vec<(usize, prusa_watch::escalation::Step)>| v.into_iter().map(|(i, _)| i).collect::<Vec<_>>();
    assert_eq!(idx(inc.due(1000.0)), vec![0]);
    assert_eq!(inc.next_action().unwrap().0, 1);
    assert_eq!(inc.commands(), vec!["veto", "act", "stop"]);
    inc.next_idx = 1;
    assert!(inc.due(1100.0).is_empty());
    assert_eq!(idx(inc.due(1120.0)), vec![1]);
    let f = inc.template_fields(1030.0, "core-one", Some("cube.bgcode"), 0.7);
    assert_eq!(f["next_action_in"], "1:30");
    assert_eq!(f["elapsed"], "0:30");
    assert_eq!(f["next_action"], "pause");
    inc.acted = Some("paused".into());
    assert_eq!(inc.default_buttons(), vec!["resume", "mute", "stop"]);
}

fn base_cfg() -> Config {
    let mut cfg = Config::default();
    cfg.printer.host = "h".into();
    cfg.printer.password = "p".into();
    cfg.camera.url = "rtsp://c/live".into();
    cfg
}

#[test]
fn config_validate_reports_escalation_tz_and_channel_errors() {
    let mut cfg = base_cfg();
    cfg.escalation = m(r#"{"policies": {"p": {"steps": [{"at": "nope"}]}}}"#);
    cfg.timezone = "Mars/Olympus_Mons".into();
    cfg.notify.warning.channels = Some(vec!["pager".into()]);
    let e = cfg.validate().unwrap_err().to_string();
    assert!(e.contains("invalid duration") && e.contains("timezone") && e.contains("pager"), "{e}");
}

fn write_cfg(dir: &Path, text: &str) -> PathBuf {
    let p = dir.join("c.yaml");
    std::fs::write(&p, text).unwrap();
    p
}

fn no_env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

#[test]
fn moved_keys_explain_where_they_went() {
    let cases = [
        ("decision: {action: pause}", "escalation.policies.<name>.steps[].action"),
        ("decision: {veto_window_s: 120}", "step 'at' times"),
        ("decision: {schedules: []}", "escalation.schedules"),
        ("notify: {cooldown_s: 300}", "notify.warning.cooldown_s"),
        ("notify: {ntfy: {priority_failure: 5}}", "escalation step `priority`"),
        ("notify: {notify_camera_down: false}", "notify.camera.enabled"),
    ];
    for (yaml_text, where_) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_cfg(tmp.path(), yaml_text);
        let e = load_config(Some(&p), &no_env()).unwrap_err().to_string();
        assert!(e.contains("has moved to"), "{yaml_text}: {e}");
        assert!(e.contains(where_), "{yaml_text}: {e}");
    }
}

#[test]
fn duration_strings_in_regular_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let p = write_cfg(
        tmp.path(),
        "decision: {resume_grace_s: 3m}\nnotify: {warning: {cooldown_s: 10m, channels: [ntfy], priority: 2}}\nweb: {history_s: 4h}\n",
    );
    let cfg = load_config(Some(&p), &no_env()).unwrap();
    assert!(cfg.decision.resume_grace_s == 180.0 && cfg.notify.warning.cooldown_s == 600.0 && cfg.web.history_s == 14400.0);
    assert_eq!(cfg.notify.warning.channels, Some(vec!["ntfy".to_string()]));
    assert_eq!(cfg.notify.warning.priority, 2);
}

#[test]
fn example_config_is_minimal_and_valid() {
    let example = root().join("config.example.yaml");
    let env = [("PRUSALINK_PASSWORD", "x"), ("NTFY_TOPIC", "t")];
    let old: Vec<(&str, Option<String>)> = env.iter().map(|(k, _)| (*k, std::env::var(k).ok())).collect();
    // SAFETY: std serializes its own env access; nothing in this test binary reads
    // the environment through libc directly. Restored below (like the Python test).
    unsafe {
        for (k, v) in env {
            std::env::set_var(k, v);
        }
    }
    let res = load_config(Some(&example), &no_env());
    unsafe {
        for (k, v) in old {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
    let cfg = res.unwrap();
    cfg.validate().unwrap();
    assert!(cfg.escalation.is_empty()); // uses built-in escalation
    let text = std::fs::read_to_string(&example).unwrap();
    let lines = text.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).count();
    assert!(lines <= 15, "the starter config should stay small");
}

/// config.reference.yaml must be exactly the built-in defaults (docs can't drift).
#[test]
fn reference_file_equals_builtin_defaults() {
    let mut r = load_config(Some(&root().join("config.reference.yaml")), &no_env()).unwrap();
    assert_eq!(parse_escalation(Some(&r.escalation), false).unwrap(), parse_escalation(Some(&Mapping::new()), true).unwrap());
    r.escalation = Mapping::new();
    assert_eq!(r, Config::default());
}

#[test]
fn env_overrides_beat_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let p = write_cfg(tmp.path(), "decision: {sensitivity: 1.1}\nescalation: {default_policy: night}\n");
    let env: BTreeMap<String, String> = [
        ("PRUSA_WATCH__DECISION__SENSITIVITY", "1.4"),
        ("PRUSA_WATCH__ESCALATION__DEFAULT_POLICY", "watch_only"),
        ("PRUSA_WATCH__NOTIFY__WARNING__CHANNELS", "[ntfy]"),
        ("PRUSA_WATCH__NOTIFY__WARNING__ENABLED", "false"),
        ("PRUSA_WATCH__CAMERA__STALE_AFTER_S", "2m"),
        ("PRUSA_WATCH_CONFIG", "ignored.yaml"), // single underscore: not an override
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let cfg = load_config(Some(&p), &env).unwrap();
    assert_eq!(cfg.decision.sensitivity, 1.4);
    assert_eq!(parse_escalation(Some(&cfg.escalation), true).unwrap().default_policy, "watch_only");
    assert!(cfg.notify.warning.channels == Some(vec!["ntfy".to_string()]) && !cfg.notify.warning.enabled);
    assert_eq!(cfg.camera.stale_after_s, 120.0);
    let bad: BTreeMap<String, String> = [("PRUSA_WATCH__DECISION__SENSITIVTY".to_string(), "1".to_string())].into();
    let e = load_config(Some(&p), &bad).unwrap_err().to_string();
    assert!(e.contains("Unknown config key 'decision.sensitivty'"), "{e}");
}

/// The CLI binary, isolated from the host's PRUSA_WATCH_* settings and .env files.
fn cli(cwd: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_prusa-watch"));
    c.current_dir(cwd);
    for (k, _) in std::env::vars() {
        if k.starts_with("PRUSA_WATCH") {
            c.env_remove(k);
        }
    }
    c
}

#[test]
fn config_command_prints_effective_config_with_secrets_masked() {
    let tmp = tempfile::tempdir().unwrap();
    let p = write_cfg(
        tmp.path(),
        "printer: {host: 10.0.0.5, password: hunter2}\ncamera: {url: rtsp://c/live}\n\
         notify: {ntfy: {topic: secret-topic}}\nescalation: {default_policy: watch_only}\n",
    );
    let out = cli(tmp.path()).args(["config", "-c"]).arg(&p).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let y: Value = serde_yaml::from_slice(&out.stdout).unwrap();
    assert!(y["printer"]["host"] == "10.0.0.5" && y["printer"]["password"] == "***");
    assert_eq!(y["notify"]["ntfy"]["topic"], Value::from("***"));
    assert_eq!(y["escalation"]["default_policy"], Value::from("watch_only"));
    let pols: BTreeSet<String> =
        y["escalation"]["policies"].as_mapping().unwrap().keys().map(|k| k.as_str().unwrap().to_string()).collect();
    assert!(set(&["ask_first", "night"]).is_subset(&pols)); // merged built-ins shown
    let out = cli(tmp.path()).args(["config", "--defaults"]).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}

/// Guard: a new setting must be added to config.reference.yaml too.
#[test]
fn example_config_documents_every_setting() {
    let raw: Value = serde_yaml::from_str(&std::fs::read_to_string(root().join("config.reference.yaml")).unwrap()).unwrap();
    let defaults = serde_yaml::to_value(Config::default()).unwrap();
    let mut missing = Vec::new();

    fn walk(obj: &Mapping, data: &Value, path: &str, missing: &mut Vec<String>) {
        for (k, v) in obj {
            let name = k.as_str().unwrap();
            let where_ = format!("{path}{name}");
            let Some(d) = data.as_mapping().and_then(|d| d.get(name)) else {
                missing.push(where_);
                continue;
            };
            // Python recurses into nested dataclasses only; `escalation` is a plain dict.
            if let (Value::Mapping(sub), false) = (v, name == "escalation") {
                walk(sub, d, &format!("{where_}."), missing);
            }
        }
    }

    walk(defaults.as_mapping().unwrap(), &raw, "", &mut missing);
    assert!(missing.is_empty(), "undocumented settings: {missing:?}");
}
