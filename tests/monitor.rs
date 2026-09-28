//! End-to-end loop tests with a fake printer, fake camera, real detector (fake weights), real notifier.

mod common;

use common::*;
use prusa_watch::config::Config;

fn yaml(s: &str) -> serde_yaml::Mapping {
    serde_yaml::from_str(s).unwrap()
}

fn incident_id(r: &Rig) -> Option<String> {
    r.mon.core().incident.as_ref().map(|i| i.id.clone())
}

#[test]
fn idle_printer_is_not_analyzed() {
    let r = rig();
    r.grabber.set_image(Some(solid(255)));
    r.advance(50);
    assert_eq!(r.mon.counters().frames_analyzed, 0);
    assert!(r.printer.calls().is_empty() && r.sent().is_empty());
}

#[test]
fn spaghetti_pauses_print_and_notifies_with_image() {
    let r = rig();
    r.printer.set("PRINTING", Some(7));
    r.advance(60); // 10 clean minutes
    assert_eq!(r.mon.core().job.job_name.as_deref(), Some("part-7.bgcode"));
    assert!(r.printer.calls().is_empty());

    r.grabber.set_image(Some(solid(255))); // spaghetti everywhere (fake model p ~1.2/frame)
    r.advance(30);

    assert_eq!(r.printer.count("pause", 7), 1, "exactly once");
    assert_eq!(r.printer.state(), "PAUSED");
    assert_eq!(r.mon.counters().pauses, 1);

    let failure = r.with_priority("5");
    assert!(!failure.is_empty(), "failure notification not sent");
    let req = &failure[0];
    assert_eq!(req.method, "PUT");
    assert!(req.url.ends_with("/prusa-test"));
    assert_eq!(&body_bytes(req)[..2], b"\xff\xd8", "JPEG attached");
    assert!(header(req, "Message").contains("PAUSED"));
    let acts = header(req, "Actions");
    assert!(acts.contains("/api/incident/resume?token=tok&id="));
    assert!(acts.contains("False alarm") && acts.contains("Cancel print"));

    // snapshot of the failure saved to disk
    let saved: Vec<_> = std::fs::read_dir(r.state_dir().join("failures")).unwrap().collect();
    assert_eq!(saved.len(), 1);

    // while paused, no further analysis or actions
    let before = r.mon.counters().frames_analyzed;
    r.advance(10);
    assert_eq!(r.mon.counters().frames_analyzed, before);
}

#[test]
fn resume_after_ai_pause_rearms_after_grace() {
    let r = rig();
    r.printer.set("PRINTING", Some(8));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    r.advance(30);
    assert_eq!(r.printer.state(), "PAUSED");

    // user looks, decides to resume anyway (mess still visible)
    r.mon.resume(false).unwrap();
    let before = r.with_priority("4").len();
    r.advance(11); // inside resume_grace_s (120 s): no actions, no alerts
    assert_eq!(r.printer.count("pause", 8), 1);
    assert_eq!(r.with_priority("4").len(), before);
    assert_eq!(r.mon.core().job.action_taken, None);
    // Obico semantics: after a resume the per-print short mean has absorbed the
    // mess, so it only re-pauses if things get *worse*. Crank sensitivity to emulate.
    r.mon.core().decider.cfg.sensitivity = 3.0;
    r.advance(5);
    assert_eq!(r.printer.count("pause", 8), 2);
}

#[test]
fn resume_with_mute_stops_further_actions() {
    let r = rig();
    r.printer.set("PRINTING", Some(10));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    r.advance(30);
    assert!(header(&r.with_priority("5")[0], "Actions").contains("/api/incident/mute?"));
    assert!(r.mon.resume(true).unwrap().contains("muted"));
    r.advance(60);
    assert_eq!(r.printer.count("pause", 10), 1);
}

#[test]
fn mute_prevents_action() {
    let r = rig();
    r.printer.set("PRINTING", Some(9));
    r.advance(1);
    r.mon.set_muted(true);
    r.advance(59);
    r.grabber.set_image(Some(solid(255)));
    r.advance(40);
    assert!(r.printer.calls().is_empty());
}

#[test]
fn new_job_resets_state_and_mute() {
    let r = rig();
    r.printer.set("PRINTING", Some(1));
    r.advance(40);
    r.mon.set_muted(true);
    r.printer.set_state("FINISHED");
    r.advance(2);
    r.printer.set("PRINTING", Some(2));
    r.advance(1);
    let core = r.mon.core();
    assert!(core.job.job_id == Some(2) && !core.job.muted);
    assert_eq!(core.decider.state.current_frame_num, 1);
    assert!(core.decider.state.lifetime_frame_num > 40, "baseline history kept");
}

#[test]
fn notify_only_mode_does_not_touch_printer() {
    let r = rig_with(|c| c.escalation = yaml("{default_policy: watch, policies: {watch: {steps: [{at: 0}]}}}"));
    r.printer.set("PRINTING", Some(3));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    r.advance(30);
    assert!(r.printer.calls().is_empty());
    assert_eq!(r.with_priority("5").len(), 1, "notified once, not spammed");
}

#[test]
fn stop_mode() {
    let r = rig_with(|c| c.escalation = yaml("{default_policy: kill, policies: {kill: {steps: [{at: 0, action: stop}]}}}"));
    r.printer.set("PRINTING", Some(4));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    r.advance(30);
    assert!(r.printer.calls().contains(&("stop".into(), 4)) && r.printer.state() == "STOPPED");
}

#[test]
fn failed_pause_is_reported_loudly() {
    let r = rig();
    r.printer.s().fail_pause = true;
    r.printer.set("PRINTING", Some(5));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    r.advance(30);
    assert!(r.with_priority("5").iter().any(|q| header(q, "Title").contains("pause FAILED")));
}

#[test]
fn camera_down_while_printing_notifies_once() {
    let r = rig();
    r.printer.set("PRINTING", Some(6));
    r.advance(3);
    r.grabber.set_stale(true);
    r.advance(10);
    assert_eq!(r.titles().iter().filter(|t| t.contains("camera offline")).count(), 1);
    r.grabber.set_stale(false);
    r.advance(1);
    assert!(r.titles().iter().any(|t| t.contains("back online")));
}

#[test]
fn printer_unreachable_is_survivable() {
    let r = rig();
    r.printer.s().reachable = false;
    r.advance(5);
    let snap = r.mon.snapshot();
    assert!(!snap.printer_reachable && snap.printer_error.unwrap().contains("timed out"));
    r.printer.s().reachable = true;
    r.printer.set("PRINTING", Some(11));
    r.advance(2);
    assert!(r.mon.snapshot().printer_reachable && r.mon.counters().frames_analyzed >= 1);
}

#[test]
fn detection_interval_respected() {
    let r = rig();
    r.printer.set("PRINTING", Some(12));
    r.advance_by(20, 5.0); // poll every 5 s, detect every 10 s
    let n = r.mon.counters().frames_analyzed;
    assert!((9..=11).contains(&n), "{n}");
}

// ---------------------------------------------------------------- escalation policies

const ASK_FIRST: &str = r#"
default_policy: ask_first
snooze_s: 10m
policies:
  ask_first:
    steps:
      - {at: 0, notify: [ntfy], priority: 5, buttons: [keep, act, stop]}
      - {at: 1m, notify: [ntfy], priority: 4, title: "{printer}: reminder, {next_action} in {next_action_in}", attach_image: false}
      - {at: 2m, action: pause, notify: [ntfy, webhook]}
      - {at: 32m, action: stop}
  night: {steps: [{at: 0, action: pause, priority: 2}]}
  watch: {steps: [{at: 0, notify: [ntfy], priority: 3}]}
"#;

fn arm(c: &mut Config, esc: Option<serde_yaml::Mapping>, reply_topic: &str, webhook: bool) {
    c.escalation = esc.unwrap_or_else(|| yaml(ASK_FIRST));
    c.notify.ntfy.reply_topic = reply_topic.into();
    if webhook {
        c.notify.webhook.url = "https://ha.example/api/webhook/pw".into();
    }
    c.timezone = "UTC".into();
}

fn armed() -> Rig {
    rig_with(|c| arm(c, None, "reply-xyz", true))
}

fn ask_first_with(extra: &str) -> serde_yaml::Mapping {
    let mut m = yaml(ASK_FIRST);
    for (k, v) in yaml(extra) {
        m.insert(k, v);
    }
    m
}

fn spaghetti_until_incident(r: &Rig, job: i64) -> String {
    r.printer.set("PRINTING", Some(job));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    for _ in 0..40 {
        r.advance(1);
        if let Some(id) = incident_id(r) {
            return id;
        }
    }
    panic!("no incident opened");
}

#[test]
fn policy_steps_run_on_schedule_with_per_step_routing() {
    let r = armed();
    let inc = spaghetti_until_incident(&r, 30);
    assert!(r.printer.calls().is_empty());
    let first = r.ntfy().last().cloned().unwrap();
    assert_eq!(header(&first, "Priority"), "5");
    assert!(header(&first, "Title").contains("pausing in 2:00"));
    let acts = header(&first, "Actions");
    assert!(acts.contains(&format!("https://ntfy.example/reply-xyz, method=POST, body=veto {inc}")));
    assert!(acts.contains(&format!("body=act {inc}")) && acts.contains(&format!("body=stop {inc}")));
    assert!(r.hooks().iter().all(|h| h["kind"] == "warning"), "step 0 is ntfy-only");

    r.advance(6); // 1 min: reminder, custom template, no image, priority 4
    let rem = r.ntfy().last().cloned().unwrap();
    assert_eq!(header(&rem, "Title"), "core-one: reminder, pause in 1:00");
    assert_eq!(header(&rem, "Priority"), "4");
    assert_eq!(rem.method, "POST", "no attachment");
    assert!(r.printer.calls().is_empty());

    r.advance(6); // 2 min: pause, ntfy + webhook, default resume buttons
    assert_eq!(r.printer.calls(), vec![("pause".to_string(), 30)]);
    assert_eq!(r.mon.counters().auto_actions, 1);
    let paused = r.ntfy().last().cloned().unwrap();
    assert!(header(&paused, "Title").contains("PAUSED"));
    assert!(
        header(&paused, "Actions").contains(&format!("body=resume {inc}"))
            && header(&paused, "Actions").contains(&format!("body=mute {inc}"))
    );
    let hook = r.hooks().last().cloned().unwrap();
    assert!(hook["action_taken"] == "paused" && hook["incident_id"] == inc.as_str() && hook["next_action"] == "stop");
    assert!(hook["command_urls"]["resume"].as_str().unwrap().ends_with(&format!("id={inc}")));
    assert_eq!(incident_id(&r).as_deref(), Some(inc.as_str()), "stays open while paused so Resume works");

    r.advance(179); // nobody answered for 30 min -> stop
    assert!(!r.printer.calls().contains(&("stop".into(), 30)));
    r.advance(2);
    assert!(r.printer.calls().contains(&("stop".into(), 30)));
    assert_eq!(r.mon.counters().auto_actions, 2);
    r.advance(1);
    assert!(incident_id(&r).is_none());
}

#[test]
fn keep_printing_closes_incident_and_snoozes() {
    let r = armed();
    let inc = spaghetti_until_incident(&r, 31);
    assert!(r.mon.handle_reply("veto", &inc).starts_with("vetoed (no new alerts for 10:00)"));
    assert!(incident_id(&r).is_none());
    assert_eq!(r.mon.counters().vetoes, 1);
    r.advance(55); // 550 s inside the 10 min snooze
    assert!(r.printer.calls().is_empty() && incident_id(&r).is_none());
    r.mon.core().decider.cfg.sensitivity = 3.0; // "worse" (Obico's short mean absorbed the mess meanwhile)
    r.advance(10);
    let new = incident_id(&r).expect("new incident");
    assert_ne!(new, inc);
    assert!(r.printer.calls().is_empty(), "asks again, doesn't pause straight away");
}

#[test]
fn keep_printing_after_pause_resumes() {
    let r = armed();
    let inc = spaghetti_until_incident(&r, 32);
    r.advance(13);
    assert_eq!(r.printer.state(), "PAUSED");
    // the paused alert doesn't offer "keep", but the command still means "false alarm, carry on"
    assert!(r.mon.handle_reply("veto", &inc).contains("vetoed"));
    assert_eq!(r.printer.calls().last().cloned(), Some(("resume".to_string(), 32)));
    assert!(incident_id(&r).is_none());
}

#[test]
fn act_now_skips_reminders_and_stop_and_bad_ids() {
    let r = armed();
    let inc = spaghetti_until_incident(&r, 33);
    assert!(r.mon.handle_reply("veto", "wrong").starts_with("ignored"));
    assert!(r.mon.handle_reply("resume", &inc).starts_with("ignored"), "not paused yet");
    assert_eq!(r.mon.handle_reply("act", &inc), "paused");
    assert_eq!(r.printer.calls(), vec![("pause".to_string(), 33)]);
    let before = r.ntfy().len();
    r.advance(12); // the 1-min reminder was skipped by "act"
    assert_eq!(r.ntfy().len(), before);
    assert_eq!(r.mon.handle_reply("stop", &inc), "stopped");
    assert!(r.printer.calls().contains(&("stop".into(), 33)) && incident_id(&r).is_none());
    assert!(r.mon.handle_reply("resume", &inc).starts_with("ignored"), "one-time id");
}

#[test]
fn resume_reply_rearms_after_grace() {
    let r = armed();
    let inc = spaghetti_until_incident(&r, 34);
    r.mon.handle_reply("act", &inc);
    assert_eq!(r.mon.handle_reply("resume", &inc), "resumed");
    assert!(r.printer.state() == "PRINTING" && incident_id(&r).is_none());
    assert_eq!(r.mon.core().job.rearm_at, r.clock.t() + r.mon.cfg.decision.resume_grace_s);
}

#[test]
fn incident_closed_when_you_pause_at_the_printer() {
    let r = armed();
    spaghetti_until_incident(&r, 35);
    r.printer.set_state("PAUSED"); // knob on the printer / Prusa app
    r.advance(1);
    assert!(incident_id(&r).is_none());
    r.advance(30);
    assert!(r.printer.calls().is_empty());
}

#[test]
fn schedule_picks_policy() {
    let esc = ask_first_with(r#"{schedules: [{name: nap, start: "13:00", end: "16:00", policy: night}]}"#);
    let r = rig_with(|c| arm(c, Some(esc), "reply-xyz", true)); // rig clock starts at 1970-01-12 13:46 UTC
    r.printer.set("PRINTING", Some(36));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    r.advance(30);
    assert_eq!(r.printer.calls(), vec![("pause".to_string(), 36)]);
    let first = r.ntfy().last().cloned().unwrap();
    assert_eq!(header(&first, "Priority"), "2");
    assert!(header(&first, "Message").contains("[night / nap]"));
    let pol = r.mon.snapshot().policy;
    assert!(pol["policy"] == "night" && pol["schedule"] == "nap");
}

#[test]
fn notify_only_policy_cools_down() {
    let r = rig_with(|c| arm(c, Some(ask_first_with("{default_policy: watch, snooze_s: 300}")), "reply-xyz", true));
    r.printer.set("PRINTING", Some(37));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    r.advance(12);
    assert_eq!(r.mon.counters().failures, 1);
    assert!(incident_id(&r).is_none(), "notify-only closes right away");
    assert!(r.printer.calls().is_empty());
    assert!(header(r.ntfy().last().unwrap(), "Message").contains("does not act"));
    assert!(r.mon.core().job.rearm_at > r.clock.t(), "cooldown = escalation.snooze_s");
    r.advance(20);
    assert_eq!(r.mon.counters().failures, 1);
}

#[test]
fn silent_step_and_dashboard_fallback_links() {
    let esc = yaml(
        "{default_policy: p, policies: {p: {steps: [{at: 0, notify: []}, {at: 30, buttons: [keep, act, dashboard]}, {at: 60, action: pause, notify: false}]}}}",
    );
    let r = rig_with(|c| arm(c, Some(esc), "", false));
    let inc = spaghetti_until_incident(&r, 38);
    assert!(r.ntfy().iter().all(|q| !header(q, "Tags").contains("hourglass")));
    r.advance(3);
    let alert = r.ntfy().into_iter().rfind(|q| header(q, "Tags").contains("hourglass")).unwrap();
    let acts = header(&alert, "Actions");
    assert!(acts.contains(&format!("http://watch.lan:8484/api/incident/veto?token=tok&id={inc}")));
    assert!(acts.contains("view, Dashboard, http://watch.lan:8484"));
    let n = r.sent().len();
    r.advance(4);
    assert_eq!(r.printer.calls(), vec![("pause".to_string(), 38)]);
    assert_eq!(r.sent().len(), n, "pause step was silent");
}

/// PrusaLink can still report PRINTING for a poll or two after the pause command
/// is accepted (firmware finishing the current move / parking). That must not be
/// read as "resumed at the printer", which would close the incident and kill the
/// Resume / False alarm buttons.
#[test]
fn pause_in_flight_is_not_mistaken_for_a_resume() {
    let r = rig();
    r.printer.set("PRINTING", Some(7));
    r.advance(60);
    r.printer.s().pause_lag_polls = 2;
    r.grabber.set_image(Some(solid(255)));
    for _ in 0..40 {
        r.advance(1);
        if r.printer.count("pause", 7) > 0 {
            break;
        }
    }
    assert_eq!(r.printer.count("pause", 7), 1);
    let inc = incident_id(&r).expect("incident");
    assert_eq!(r.mon.core().incident.as_ref().unwrap().acted.as_deref(), Some("paused"));

    r.advance(3); // PRINTING (in flight), then PAUSED
    assert_eq!(r.printer.state(), "PAUSED");
    assert_eq!(incident_id(&r).as_deref(), Some(inc.as_str()), "incident closed by a phantom resume");
    assert_eq!(r.mon.core().job.action_taken.as_deref(), Some("paused"));
    assert_eq!(r.mon.handle_reply("resume", &inc), "resumed");
    assert_eq!(r.printer.state(), "PRINTING");
}

/// A failed pause must not be treated as done: the incident stays open, the step is
/// retried every poll, the loud alert goes out once, and the normal PAUSED alert follows.
#[test]
fn failed_pause_is_retried_until_it_works() {
    let r = rig();
    r.printer.s().fail_pause = true;
    let inc = spaghetti_until_incident(&r, 5); // pause_now policy
    assert_eq!(r.printer.count("pause", 5), 1);
    {
        let core = r.mon.core();
        let i = core.incident.as_ref().unwrap();
        assert!(i.id == inc && i.acted.is_none() && i.next_idx == 0);
    }
    r.advance_by(3, 5.0); // still failing: retried each poll, incident stays open
    assert_eq!(r.printer.count("pause", 5), 4);
    assert_eq!(r.mon.core().incident.as_ref().unwrap().action_errors, 4);
    assert_eq!(r.titles().iter().filter(|t| t.contains("pause FAILED")).count(), 1, "FAILED alert once, not every retry");

    r.printer.s().fail_pause = false;
    r.advance_by(1, 5.0);
    assert_eq!(r.printer.state(), "PAUSED");
    let core = r.mon.core();
    assert_eq!(core.incident.as_ref().map(|i| i.id.clone()), Some(inc), "paused incidents stay open");
    assert_eq!(core.incident.as_ref().unwrap().acted.as_deref(), Some("paused"));
    assert_eq!(core.job.action_taken.as_deref(), Some("paused"));
    drop(core);
    assert!(r.titles().iter().any(|t| t.contains("print PAUSED")));
    assert_eq!(r.mon.core().job.rearm_at, 0.0, "a failed attempt must not snooze detection");
}

#[test]
fn act_reply_reports_failed_pause_and_keeps_retrying() {
    let r = armed();
    let inc = spaghetti_until_incident(&r, 12);
    r.printer.s().fail_pause = true;
    assert!(r.mon.handle_reply("act", &inc).starts_with("failed:"));
    assert!(r.mon.core().incident.as_ref().is_some_and(|i| i.id == inc && i.acted.is_none()));
    r.printer.s().fail_pause = false;
    r.advance_by(1, 5.0); // the loop retries the pending pause on the next poll
    assert_eq!(r.printer.state(), "PAUSED");
    assert_eq!(r.mon.core().incident.as_ref().unwrap().acted.as_deref(), Some("paused"));
}

#[test]
fn pause_at_printer_then_resume_gets_grace() {
    let r = armed();
    spaghetti_until_incident(&r, 35);
    r.printer.set_state("PAUSED"); // you paused it at the printer
    r.advance(1);
    assert!(incident_id(&r).is_none() && r.mon.core().job.printer_handled);

    r.advance(3); // still paused, time passes
    // crank sensitivity so the mess still in view reads as a failure despite the absorbed baseline
    r.mon.core().decider.cfg.sensitivity = 3.0;
    r.printer.set_state("PRINTING"); // resumed at the printer; the spaghetti is still in view
    r.advance(1);
    let t_resume = r.clock.t();
    let grace = r.mon.cfg.decision.resume_grace_s;
    assert!(!r.mon.core().job.printer_handled);
    assert_eq!(r.mon.core().job.rearm_at, t_resume + grace);
    r.advance((grace / 10.0) as usize - 1);
    assert!(incident_id(&r).is_none(), "no new incident during the resume grace");
    r.advance(2);
    assert!(incident_id(&r).is_some(), "re-arms after the grace");
    assert!(r.printer.calls().is_empty());
}
