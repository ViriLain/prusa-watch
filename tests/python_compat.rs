//! Behaviours the Python version had through PyYAML / Python truthiness that the
//! Rust port must keep, so existing config files and state keep working.

use std::sync::Arc;

use prusa_watch::escalation::parse_escalation;
use prusa_watch::notify::{Event, Notifier};

fn esc(yaml: &str) -> prusa_watch::escalation::EscalationConfig {
    parse_escalation(Some(&serde_yaml::from_str(yaml).unwrap()), true).unwrap()
}

#[test]
fn yaml_1_1_booleans_in_steps() {
    let e = esc(
        "{default_policy: p, policies: {p: {steps: [{at: 0, notify: off, attach_image: no, action: no}, {at: 1m, notify: yes, attach_image: On}]}}}",
    );
    let s = &e.policy("p").unwrap().steps;
    assert_eq!(s[0].notify, Some(vec![]), "notify: off = silent");
    assert!(!s[0].attach_image, "attach_image: no = false");
    assert_eq!(s[0].action, None, "action: no = notify only");
    assert_eq!(s[1].notify, None, "notify: yes = all channels");
    assert!(s[1].attach_image);
}

#[test]
fn falsy_schedule_name_falls_back() {
    let e = esc(
        "{schedules: [{name: 0, start: '01:00', end: '02:00', policy: night}, {name: '', start: '03:00', end: '04:00', policy: night}]}",
    );
    assert_eq!(e.schedules[0].name, "schedule-0");
    assert_eq!(e.schedules[1].name, "schedule-1");
}

#[test]
fn empty_image_bytes_mean_no_attachment() {
    let mut cfg = prusa_watch::config::NotifyConfig::default();
    cfg.ntfy.topic = "t".into();
    let n = Notifier::with_transport(cfg, "http://x", "", Arc::new(prusa_watch::http::ReqwestTransport::new()));
    let mut e = Event::new("warning", "t", "m", "p");
    e.image_jpeg = Some(Arc::new(vec![]));
    let req = n.ntfy_request(&e);
    assert_eq!(req.method, "POST");
    assert!(req.get_header("Filename").is_none());
}

#[test]
fn state_file_with_float_counts_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("prediction_state.json");
    std::fs::write(&p, r#"{"current_frame_num": 3.0, "lifetime_frame_num": 603.0, "rolling_mean_long": 0.0157}"#).unwrap();
    let d = prusa_watch::decision::FailureDecider::new(Default::default(), Some(&p));
    assert_eq!(d.state.lifetime_frame_num, 603, "baseline kept, not reset to the prior");
    assert_eq!(d.state.rolling_mean_long, 0.0157);
}

#[test]
fn python_written_state_file_loads() {
    // exactly what the Python version wrote on the user's machine
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("prediction_state.json");
    std::fs::write(&p, "{\n  \"current_frame_num\": 93,\n  \"lifetime_frame_num\": 603,\n  \"current_p\": 1.237664520740509,\n  \"ewm_mean\": 0.5131945111987541,\n  \"rolling_mean_short\": 0.04485509338531088,\n  \"rolling_mean_long\": 0.01571360299524093,\n  \"normalized_p\": 0.7480915912834785\n}").unwrap();
    let d = prusa_watch::decision::FailureDecider::new(Default::default(), Some(&p));
    assert_eq!(d.state.lifetime_frame_num, 603);
    assert_eq!(d.state.rolling_mean_long, 0.01571360299524093);
}

fn load(yaml: &str) -> Result<prusa_watch::config::Config, prusa_watch::config::ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("c.yaml");
    std::fs::write(&p, yaml).unwrap();
    prusa_watch::config::load_config(Some(&p), &Default::default())
}

#[test]
fn yaml_1_1_sexagesimal_durations() {
    // PyYAML read unquoted 1:30 as 90 and 30:00 as 1800
    let e = esc("{snooze_s: 30:00, policies: {p: {steps: [{at: 0}, {at: 1:30, action: pause}]}}, default_policy: p}");
    assert_eq!(e.snooze_s, 1800.0);
    assert_eq!(e.policy("p").unwrap().steps[1].at, 90.0);
    let c = load("printer: {poll_interval_s: 0:05}\nweb: {history_s: 1:00:00}\n").unwrap();
    assert_eq!(c.printer.poll_interval_s, 5.0);
    assert_eq!(c.web.history_s, 3600.0);
}

#[test]
fn scalars_python_accepted_still_load() {
    let c = load("notify: {ntfy: {topic: 12345, reply_topic: 987654}}\nweb: {enabled: 1}\nsave_failure_frames: 0\ndecision: {init_safe_frame_num: 30.0}\n").unwrap();
    assert_eq!(c.notify.ntfy.topic, "12345");
    assert_eq!(c.notify.ntfy.reply_topic, "987654");
    assert!(c.web.enabled);
    assert!(!c.save_failure_frames);
    assert_eq!(c.decision.init_safe_frame_num, 30);
    assert!(load("decision: {init_safe_frame_num: 30.5}\n").is_err(), "a fractional count is still an error");
}

#[test]
fn removing_a_builtin_keeps_policy_order() {
    // first remaining policy is the fallback when default_policy is empty (e.g. an unset ${VAR})
    let e = esc("{default_policy: '', policies: {ask_first: null}}");
    let names: Vec<&str> = e.policies.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["pause_now", "night", "watch_only"]);
    assert_eq!(e.default_policy, "pause_now");
}
