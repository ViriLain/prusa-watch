//! Always-on recovery: stall watchdog, restart notes, printer-unreachable alerts, and the
//! dead-man's switch heartbeat.

mod common;

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use common::*;
use prusa_watch::config::{Config, effective_config};
use prusa_watch::heartbeat::{Beat, Heartbeat};
use prusa_watch::http::HttpResponse;
use prusa_watch::watchdog;

// ------------------------------------------------------------------ watchdog

#[test]
fn watchdog_fires_only_when_the_loop_stops_progressing() {
    let rig = rig();
    rig.advance(1);
    assert_eq!(watchdog::check(&rig.mon, 60.0), None);
    rig.clock.add(59.0);
    assert_eq!(watchdog::check(&rig.mon, 60.0), None);
    rig.clock.add(2.0);
    let reason = watchdog::check(&rig.mon, 60.0).expect("stall detected");
    assert!(reason.contains("no progress for 61 s"), "{reason}");
    rig.advance(1); // a completed tick clears it
    assert_eq!(watchdog::check(&rig.mon, 60.0), None);
}

#[test]
fn watchdog_counts_from_startup_when_the_loop_never_ran() {
    let rig = rig();
    rig.clock.add(30.0);
    assert_eq!(watchdog::check(&rig.mon, 60.0), None);
    rig.clock.add(31.0);
    assert!(watchdog::check(&rig.mon, 60.0).is_some());
}

#[test]
fn watchdog_does_not_need_the_locks_a_stuck_tick_would_hold() {
    let rig = rig();
    rig.advance(1);
    let _core = rig.mon.core(); // e.g. a tick wedged inside a PrusaLink call
    rig.clock.add(120.0);
    assert!(watchdog::check(&rig.mon, 60.0).is_some());
    assert!(rig.mon.loop_age_s() >= 120.0);
}

#[test]
fn watchdog_is_quiet_during_shutdown() {
    let rig = rig();
    rig.advance(1);
    rig.mon.stop();
    rig.clock.add(1000.0);
    assert_eq!(watchdog::check(&rig.mon, 60.0), None);
}

#[test]
fn watchdog_thread_reports_a_stall_once() {
    let rig = rig();
    rig.advance(1);
    let (tx, rx) = mpsc::channel();
    let handle = watchdog::spawn(rig.mon.clone(), 5.0, move |reason| tx.send(reason).unwrap());
    rig.clock.add(100.0);
    let reason = rx.recv_timeout(Duration::from_secs(10)).expect("watchdog fired");
    assert!(reason.contains("no progress"), "{reason}");
    handle.join().unwrap(); // returns after firing
    assert!(rx.try_recv().is_err());
}

#[test]
fn restart_note_distinguishes_clean_shutdown_stall_and_crash() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("data");
    assert_eq!(watchdog::begin_run(&state), None, "first start");
    watchdog::end_run(&state);
    assert_eq!(watchdog::begin_run(&state), None, "after a clean shutdown");

    // no end_run: the process died
    let crash = watchdog::begin_run(&state).expect("unclean exit noticed");
    assert!(crash.contains("without shutting down"), "{crash}");

    watchdog::record_restart(&state, "monitor loop made no progress for 400 s").unwrap();
    assert_eq!(
        watchdog::begin_run(&state).as_deref(),
        Some("monitor loop made no progress for 400 s"),
        "the stall reason wins over the generic crash note"
    );
    watchdog::end_run(&state);
    assert_eq!(watchdog::begin_run(&state), None, "notes are consumed");
}

#[test]
fn restart_notice_is_sent_on_the_health_route() {
    let rig = rig();
    rig.mon.notify_restarted("monitor loop made no progress for 400 s");
    let sent = rig.ntfy();
    assert_eq!(sent.len(), 1);
    assert!(header(&sent[0], "Title").contains("prusa-watch restarted"));
    assert_eq!(header(&sent[0], "Priority"), "4");
    assert!(String::from_utf8_lossy(&body_bytes(&sent[0])).contains("no progress for 400 s"));
}

// ------------------------------------------------------------------ printer unreachable

fn titles_with(rig: &Rig, needle: &str) -> usize {
    rig.titles().iter().filter(|t| t.contains(needle)).count()
}

#[test]
fn printer_unreachable_mid_print_alerts_once_after_the_threshold_and_on_recovery() {
    let rig = rig();
    rig.printer.set("PRINTING", Some(7));
    rig.advance(3);
    rig.transport.clear();
    rig.printer.s().reachable = false;
    rig.advance(5); // 50 s < 60 s
    assert_eq!(titles_with(&rig, "unreachable mid-print"), 0);
    rig.advance(2);
    assert_eq!(titles_with(&rig, "unreachable mid-print"), 1);
    rig.advance(10);
    assert_eq!(titles_with(&rig, "unreachable mid-print"), 1, "no repeats while down");
    rig.printer.s().reachable = true;
    rig.advance(1);
    assert_eq!(titles_with(&rig, "printer reachable again"), 1);
    rig.advance(3);
    assert_eq!(titles_with(&rig, "printer reachable again"), 1);
}

#[test]
fn a_brief_blip_mid_print_is_not_reported() {
    let rig = rig();
    rig.printer.set("PRINTING", Some(7));
    rig.advance(3);
    rig.printer.s().reachable = false;
    rig.advance(3);
    rig.printer.s().reachable = true;
    rig.advance(1);
    rig.printer.s().reachable = false;
    rig.advance(3); // the clock restarted with the second outage
    assert_eq!(titles_with(&rig, "unreachable"), 0);
    assert_eq!(titles_with(&rig, "reachable again"), 0);
}

#[test]
fn an_idle_printer_going_offline_is_not_an_alert() {
    let rig = rig();
    rig.advance(3); // IDLE
    rig.printer.s().reachable = false;
    rig.advance(20);
    assert_eq!(titles_with(&rig, "unreachable"), 0);
}

#[test]
fn printer_down_alert_can_be_disabled() {
    let rig = rig_with(|cfg| cfg.health.printer_down_alert_s = 0.0);
    rig.printer.set("PRINTING", Some(7));
    rig.advance(3);
    rig.printer.s().reachable = false;
    rig.advance(20);
    assert_eq!(titles_with(&rig, "unreachable"), 0);
}

// ------------------------------------------------------------------ protection health

#[test]
fn protection_health_tolerates_an_idle_printer_being_off() {
    let rig = rig();
    rig.advance(2);
    assert_eq!(rig.mon.protection_health(), Ok(()));
    rig.printer.s().reachable = false;
    rig.advance(1);
    assert!(rig.mon.readiness().is_err(), "readiness still reports it");
    assert_eq!(rig.mon.protection_health(), Ok(()), "nothing to protect");
}

#[test]
fn protection_health_fails_when_a_print_cannot_be_protected() {
    let rig = rig();
    rig.printer.set("PRINTING", Some(7));
    rig.advance(3);
    assert_eq!(rig.mon.protection_health(), Ok(()));

    rig.grabber.set_stale(true);
    rig.advance(1);
    assert!(rig.mon.protection_health().unwrap_err().contains("camera"));
    rig.grabber.set_stale(false);
    rig.advance(2);
    assert_eq!(rig.mon.protection_health(), Ok(()));

    rig.printer.s().reachable = false;
    rig.advance(1);
    assert!(rig.mon.protection_health().unwrap_err().contains("printer"));
}

#[test]
fn protection_health_fails_when_the_loop_stalls() {
    let rig = rig();
    rig.advance(2);
    rig.clock.add(300.0);
    assert!(rig.mon.protection_health().unwrap_err().contains("stalled"));
}

// ------------------------------------------------------------------ heartbeat

fn heartbeat(url: &str, fail_url: &str) -> (Heartbeat, Arc<RecordingTransport>) {
    let mut cfg = Config::default().health;
    cfg.heartbeat_url = url.into();
    cfg.heartbeat_fail_url = fail_url.into();
    let transport = Arc::new(RecordingTransport::default());
    (Heartbeat::new(&cfg, 5.0, transport.clone()).unwrap(), transport)
}

#[test]
fn heartbeat_is_off_without_a_url() {
    let cfg = Config::default().health;
    assert!(Heartbeat::new(&cfg, 5.0, Arc::new(RecordingTransport::default())).is_none());
}

#[test]
fn heartbeat_pings_the_success_url_when_healthy() {
    let (hb, t) = heartbeat("https://hc-ping.com/abc", "https://hc-ping.com/abc/fail");
    assert_eq!(hb.beat(Ok(())), Ok(Beat::Up));
    let sent = t.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "GET");
    assert_eq!(sent[0].url, "https://hc-ping.com/abc");
}

#[test]
fn heartbeat_pings_the_fail_url_with_an_encoded_reason() {
    let (hb, t) = heartbeat(
        "https://kuma.lan/api/push/tok?status=up&msg=OK",
        "https://kuma.lan/api/push/tok?status=down&msg={reason}",
    );
    let reason = "camera frames are unavailable or stale".to_string();
    assert_eq!(hb.beat(Err(reason.clone())), Ok(Beat::Down(reason)));
    assert_eq!(
        t.requests()[0].url,
        "https://kuma.lan/api/push/tok?status=down&msg=camera%20frames%20are%20unavailable%20or%20stale"
    );
}

#[test]
fn heartbeat_without_a_fail_url_stays_silent_so_the_timer_runs_out() {
    let (hb, t) = heartbeat("https://hc-ping.com/abc", "");
    assert_eq!(
        hb.beat(Err("printer status is unavailable or stale".into())),
        Ok(Beat::Skipped("printer status is unavailable or stale".into()))
    );
    assert!(t.requests().is_empty());
}

#[test]
fn heartbeat_reports_a_failed_ping() {
    let (hb, t) = heartbeat("https://hc-ping.com/abc", "");
    *t.respond.lock().unwrap() = Some(Box::new(|_| HttpResponse::new(500, "")));
    assert_eq!(hb.beat(Ok(())), Err("HTTP 500".into()));
}

// ------------------------------------------------------------------ config

#[test]
fn stall_limit_is_never_shorter_than_twice_the_loop_budget() {
    let mut cfg = Config::default();
    assert_eq!(cfg.stall_exit_after_s(), Some(300.0));
    cfg.detector.interval_s = 600.0; // loop budget 15 + 10 + 1200
    assert_eq!(cfg.stall_exit_after_s(), Some(2.0 * 1225.0));
    cfg.health.stall_exit_s = 0.0;
    assert_eq!(cfg.stall_exit_after_s(), None);
}

#[test]
fn heartbeat_settings_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path());
    assert!(cfg.validate().is_ok());
    cfg.health.heartbeat_fail_url = "https://hc-ping.com/abc/fail".into();
    let err = cfg.validate().unwrap_err().to_string();
    assert!(err.contains("heartbeat_fail_url needs health.heartbeat_url"), "{err}");
    cfg.health.heartbeat_url = "hc-ping.com/abc".into();
    let err = cfg.validate().unwrap_err().to_string();
    assert!(err.contains("health.heartbeat_url must be an http"), "{err}");
    cfg.health.heartbeat_url = "https://hc-ping.com/abc".into();
    cfg.health.heartbeat_interval_s = 1.0;
    let err = cfg.validate().unwrap_err().to_string();
    assert!(err.contains("health.heartbeat_interval_s"), "{err}");
}

#[test]
fn heartbeat_urls_are_redacted_like_other_secrets() {
    let mut cfg = Config::default();
    cfg.health.heartbeat_url = "https://hc-ping.com/secret-uuid".into();
    cfg.health.heartbeat_fail_url = "https://hc-ping.com/secret-uuid/fail".into();
    let shown = serde_yaml::to_string(&effective_config(&cfg, true)).unwrap();
    assert!(!shown.contains("secret-uuid"), "{shown}");
}
