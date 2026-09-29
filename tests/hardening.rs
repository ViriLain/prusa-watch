//! Operational contracts exercised through the same interfaces as the CLI and dashboard.
mod common;

use common::*;
use prusa_watch::config::build_config;

#[test]
fn invalid_numbers_are_rejected_before_starting_monitoring() {
    let dir = tempfile::tempdir().unwrap();
    for patch in [
        "decision: {ewm_span: -1}",
        "decision: {rolling_win_short: 0}",
        "decision: {rolling_win_long: 0}",
        "decision: {sensitivity: 0}",
        "decision: {threshold_low: 2, threshold_high: 1}",
        "decision: {escalating_factor: 0}",
        "decision: {min_frame_p: .nan}",
        "detector: {threshold: 2}",
        "detector: {interval_s: .inf}",
        "printer: {poll_interval_s: .inf}",
        "camera: {read_timeout_s: -5}",
        "web: {history_s: .inf}",
        "escalation: {policies: {ask_first: {steps: [{at: .nan, action: pause}]}}}",
    ] {
        let mut data = serde_yaml::to_value(base_config(dir.path())).unwrap();
        let patch_data: serde_yaml::Value = serde_yaml::from_str(patch).unwrap();
        for (section, values) in patch_data.as_mapping().unwrap() {
            for (name, value) in values.as_mapping().unwrap() {
                data[section]
                    .as_mapping_mut()
                    .unwrap()
                    .insert(name.clone(), value.clone());
            }
        }
        assert!(build_config(data).unwrap().validate().is_err(), "accepted {patch}");
    }
}

#[test]
fn invalid_boolean_text_is_not_silently_false() {
    let raw = serde_yaml::from_str("web: {enabled: flase}").unwrap();
    assert!(
        build_config(raw)
            .unwrap_err()
            .to_string()
            .contains("expected true/false")
    );
}

#[test]
fn invalid_prediction_checkpoint_does_not_poison_the_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prediction_state.json");
    for text in [
        "{\"lifetime_frame_num\": -2}",
        "{\"lifetime_frame_num\": 2.5}",
        "{\"lifetime_frame_num\": 9223372036854775800}",
        "{\"lifetime_frame_num\": 1, \"current_frame_num\": 20}",
        "{\"rolling_mean_long\": -3}",
    ] {
        std::fs::write(&path, text).unwrap();
        let mut decider = prusa_watch::decision::FailureDecider::new(Default::default(), Some(&path));
        decider.update(&[0.5]);
        assert!(decider.state.is_valid(), "invalid state after loading {text}");
    }
}

#[test]
fn network_controls_require_authentication_or_explicit_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path());
    cfg.web.host = "0.0.0.0".into();
    cfg.web.token.clear();
    assert!(
        cfg.validate()
            .unwrap_err()
            .to_string()
            .contains("web.token is required")
    );
    cfg.web.allow_unauthenticated = true;
    cfg.validate().unwrap();
}

fn pending(tweak: impl FnOnce(&mut prusa_watch::config::Config)) -> Rig {
    let rig = rig_with(|cfg| {
        cfg.escalation = serde_yaml::from_str("{default_policy: ask_first, schedules: []}").unwrap();
        tweak(cfg);
    });
    rig.printer.set("PRINTING", Some(7));
    rig.advance(60);
    rig.grabber.set_image(Some(solid(255)));
    for _ in 0..40 {
        rig.advance(1);
        if rig.mon.incident_id().is_some() {
            return rig;
        }
    }
    panic!("no failure incident");
}

#[test]
fn mute_retires_pending_actions_and_the_incident() {
    let rig = pending(|_| {});
    rig.mon.set_muted(true);
    rig.advance(20);
    assert!(rig.mon.snapshot().job.muted);
    assert!(rig.mon.incident_id().is_none());
    assert!(rig.printer.calls().is_empty());
}

#[test]
fn mute_while_paused_prevents_a_scheduled_cancellation() {
    let rig = pending(|cfg| {
        cfg.escalation = serde_yaml::from_str(
        "{default_policy: p, schedules: [], policies: {p: {steps: [{at: 0, action: pause}, {at: 60, action: stop}]}}}"
    ).unwrap()
    });
    assert_eq!(rig.printer.state(), "PAUSED");
    rig.mon.set_muted(true);
    rig.advance(10);
    assert_eq!(rig.printer.state(), "PAUSED");
    assert_eq!(rig.printer.count("stop", 7), 0);
}

#[test]
fn accepted_pause_is_not_claimed_complete_and_has_bounded_retries() {
    let rig = pending(|_| {});
    rig.printer.s().pause_lag_polls = 10_000;
    let id = rig.mon.incident_id().unwrap();
    assert!(rig.mon.handle_reply("act", &id).contains("awaiting confirmation"));
    assert_eq!(rig.mon.snapshot().job.action_taken, None);
    assert!(rig.mon.snapshot().pending_action.is_some());
    rig.advance(40);
    assert_eq!(rig.printer.state(), "PRINTING");
    assert_eq!(rig.printer.count("pause", 7), 3);
    assert!(rig.mon.snapshot().action_error.unwrap().contains("not confirmed"));
    assert!(rig.titles().iter().any(|title| title.contains("FAILED")));
}

#[test]
fn stalled_frame_is_consumed_only_once() {
    let rig = rig_with(|cfg| cfg.camera.stale_after_s = 120.0);
    rig.printer.set("PRINTING", Some(7));
    rig.advance(60);
    let before = rig.mon.counters().frames_analyzed;
    rig.grabber.set_image(Some(solid(255)));
    rig.grabber.st.lock().unwrap().connected = false;
    for _ in 0..10 {
        rig.clock.add(10.0);
        rig.mon.tick(); // no frame arrival
    }
    assert_eq!(rig.mon.counters().frames_analyzed, before + 1);
    assert!(rig.printer.calls().is_empty());
}

#[test]
fn watch_only_notification_remains_actionable() {
    let rig =
        pending(|cfg| cfg.escalation = serde_yaml::from_str("{default_policy: watch_only, schedules: []}").unwrap());
    let id = rig.mon.incident_id().unwrap();
    rig.advance(2);
    assert_eq!(rig.mon.handle_reply("stop", &id), "stopped");
    assert_eq!(rig.printer.state(), "STOPPED");
}

fn restart(rig: &Rig) -> std::sync::Arc<prusa_watch::monitor::Monitor> {
    use prusa_watch::monitor::{Monitor, Parts};
    rig.mon.flush_storage().unwrap();
    Monitor::new(
        rig.mon.cfg.clone(),
        Parts {
            printer: Some(rig.printer.clone()),
            grabber: Some(rig.grabber.clone()),
            detector: Some(fake_detector()),
            notifier: Some(rig.mon.notifier.clone()),
            clock: Some(rig.clock.as_fn()),
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn restart_restores_same_print_mute_frame_count_and_generation() {
    let rig = rig();
    rig.printer.set("PRINTING", Some(7));
    rig.advance(60);
    rig.mon.set_muted(true);
    let before = rig.mon.snapshot();
    let recovered = restart(&rig);
    recovered.tick();
    let after = recovered.snapshot();
    assert!(after.job.muted);
    assert_eq!(before.job.session_id, after.job.session_id);
    assert_eq!(after.frame_num, before.frame_num + 1);
    rig.printer.set("PRINTING", Some(8));
    recovered.tick();
    assert!(!recovered.snapshot().job.muted);
    assert_ne!(after.job.session_id, recovered.snapshot().job.session_id);
}

#[test]
fn restart_preserves_pending_and_paused_incidents_but_never_replays_overdue_steps() {
    let rig = pending(|_| {});
    let id = rig.mon.incident_id().unwrap();
    let recovered = restart(&rig);
    recovered.tick();
    assert_eq!(recovered.incident_id(), Some(id.clone()));
    assert_eq!(recovered.handle_reply("act", &id), "paused");
    recovered.flush_storage().unwrap();
    // Save from the recovered owner, then load another instance with identical components.
    let paused = prusa_watch::monitor::Monitor::new(
        recovered.cfg.clone(),
        prusa_watch::monitor::Parts {
            printer: Some(rig.printer.clone()),
            grabber: Some(rig.grabber.clone()),
            detector: Some(fake_detector()),
            notifier: Some(rig.mon.notifier.clone()),
            clock: Some(rig.clock.as_fn()),
            ..Default::default()
        },
    )
    .unwrap();
    paused.tick();
    assert_eq!(paused.snapshot().job.action_taken.as_deref(), Some("paused"));
    assert_eq!(paused.incident_id(), Some(id));

    let overdue = pending(|cfg| {
        cfg.escalation = serde_yaml::from_str(
            "{default_policy: p, schedules: [], policies: {p: {steps: [{at: 0}, {at: 60, action: stop}]}}}",
        )
        .unwrap()
    });
    overdue.mon.flush_storage().unwrap();
    overdue.clock.add(120.0);
    // Printer duration advances with downtime, as on a real running print.
    // The unchanged fake duration is within the recovery tolerance only for short gaps;
    // edit the saved elapsed print duration to model the earlier checkpoint.
    let path = overdue.state_dir().join("session.json");
    let mut saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved["time_printing"] = serde_json::json!(0);
    std::fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let recovered = restart(&overdue);
    recovered.tick();
    assert!(overdue.printer.calls().is_empty());
    assert!(recovered.snapshot().action_error.is_some());
}

#[test]
fn browser_owner_and_capability_cannot_be_reused_for_another_target() {
    let rig = pending(|_| {});
    let owner = rig.mon.snapshot().job;
    rig.printer.set("PRINTING", Some(8));
    rig.advance(1);
    assert!(rig.mon.control(7, &owner.session_id, "stop", false).is_err());
    assert!(rig.printer.calls().is_empty());
    let signer = prusa_watch::capability::Signer::default();
    let reference = prusa_watch::capability::Signer(std::array::from_fn(|index| index as u8));
    assert_eq!(
        reference.sign("veto", "one", 1100),
        "78459a4b94e1a4f24a3cc2e2bb27614bbe39c21316931c029099c2de01df63b8"
    );
    let signed = signer.sign("veto", "one", 1100);
    assert!(signer.verify("veto", "one", 1100, &signed, 1000.0));
    assert!(!signer.verify("stop", "one", 1100, &signed, 1000.0));
    assert!(!signer.verify("veto", "two", 1100, &signed, 1000.0));
    assert!(!signer.verify("veto", "one", 1100, &signed, 1101.0));
    let url = rig.mon.notifier.dashboard_url("veto", "a+b /?");
    let url = reqwest::Url::parse(&url).unwrap();
    assert!(!url.query_pairs().any(|(key, _)| key == "token"));
    assert!(url.query_pairs().any(|(key, value)| key == "id" && value == "a+b /?"));
}

#[test]
fn restarting_a_paused_print_never_replays_an_overdue_cancellation() {
    let rig = pending(|cfg| {
        cfg.escalation = serde_yaml::from_str(
        "{default_policy: p, schedules: [], policies: {p: {steps: [{at: 0, action: pause}, {at: 60, action: stop}]}}}").unwrap()
    });
    assert_eq!(rig.printer.state(), "PAUSED");
    rig.mon.flush_storage().unwrap();
    rig.clock.add(600.0); // paused print time remains unchanged
    let recovered = restart(&rig);
    recovered.tick();
    assert_eq!(recovered.snapshot().job.action_taken.as_deref(), Some("paused"));
    assert!(recovered.snapshot().action_error.is_some());
    assert!(recovered.incident_id().is_none());
    assert_eq!(rig.printer.count("stop", 7), 0);
}

#[test]
fn restart_reconciles_a_pause_that_completed_during_downtime() {
    let rig = pending(|_| {});
    rig.printer.s().pause_lag_polls = 10_000;
    let id = rig.mon.incident_id().unwrap();
    assert!(rig.mon.handle_reply("act", &id).contains("awaiting confirmation"));
    rig.mon.flush_storage().unwrap();
    rig.printer.set_state("PAUSED");
    rig.clock.add(30.0);
    let recovered = restart(&rig);
    recovered.tick();
    assert_eq!(recovered.incident_id(), Some(id));
    assert!(recovered.snapshot().pending_action.is_none());
    assert_eq!(recovered.snapshot().job.action_taken.as_deref(), Some("paused"));
    assert_eq!(
        rig.printer.count("pause", 7),
        1,
        "the completed request must not be resent"
    );
}

#[test]
fn bounded_worker_reports_overload_orders_work_and_joins() {
    use std::sync::{Arc, Mutex, mpsc};
    type Task = Box<dyn FnOnce() + Send>;
    let (started, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let results = Arc::new(Mutex::new(vec![]));
    let worker = prusa_watch::worker::Worker::new("test-worker", 1, |task: Task| {
        task();
        Ok(())
    });
    let first = results.clone();
    assert!(worker.submit(Box::new(move || {
        started.send(()).unwrap();
        wait.recv().unwrap();
        first.lock().unwrap().push(1);
    })));
    ready.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let second = results.clone();
    assert!(worker.submit(Box::new(move || {
        second.lock().unwrap().push(2);
    })));
    assert!(!worker.submit(Box::new(|| panic!("overloaded task must not run"))));
    release.send(()).unwrap();
    worker
        .flush(|done| {
            Box::new(move || {
                done.send(()).unwrap();
            })
        })
        .unwrap();
    assert_eq!(*results.lock().unwrap(), [1, 2]);
    assert_eq!(worker.stats.dropped.load(std::sync::atomic::Ordering::Relaxed), 1);
    worker.stop();
    assert!(!worker.submit(Box::new(|| {})));
}

#[test]
fn slow_inference_does_not_block_a_user_veto() {
    use image::RgbImage;
    use prusa_watch::detector::{Detect, Detection};
    use std::sync::{Arc, Mutex, mpsc};
    struct Slow {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl Detect for Slow {
        fn try_detect(&self, _: &RgbImage, _: f64, _: f64) -> Result<Vec<Detection>, String> {
            self.started.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Ok(vec![])
        }
        fn last_inference_ms(&self) -> f64 {
            1000.0
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let clock = Clock::new();
    let printer = FakePrinter::new();
    printer.set("PRINTING", Some(7));
    let (started, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let mon = prusa_watch::monitor::Monitor::new(
        base_config(dir.path()),
        prusa_watch::monitor::Parts {
            printer: Some(printer),
            grabber: Some(FakeGrabber::new(clock.clone())),
            detector: Some(Arc::new(Slow {
                started,
                release: Mutex::new(wait),
            })),
            clock: Some(clock.as_fn()),
            ..Default::default()
        },
    )
    .unwrap();
    let monitor = mon.clone();
    let tick = std::thread::spawn(move || monitor.tick());
    ready.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let (done, result) = mpsc::channel();
    let monitor = mon.clone();
    let mute = std::thread::spawn(move || {
        monitor.set_muted(true);
        done.send(()).unwrap();
    });
    let completed = result.recv_timeout(std::time::Duration::from_secs(2));
    release.send(()).unwrap();
    tick.join().unwrap();
    mute.join().unwrap();
    assert!(completed.is_ok(), "control waited for inference");
    assert!(mon.snapshot().job.muted);
}

#[test]
fn real_monitor_lifecycle_stops_workers_and_reports_stopped_readiness() {
    let rig = rig_with(|cfg| cfg.printer.poll_interval_s = 0.01);
    rig.printer.set("PRINTING", Some(7));
    rig.mon.start();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while rig.mon.counters().frames_analyzed == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(rig.mon.readiness().is_ok());
    rig.mon.stop();
    assert_eq!(rig.mon.snapshot().protection_status, "stopped");
    assert!(rig.mon.readiness().is_err());
    assert!(rig.state_dir().join("session.json").is_file());
}

#[test]
fn production_notification_worker_retries_in_order_and_shuts_down() {
    use prusa_watch::config::NotifyConfig;
    use prusa_watch::http::HttpResponse;
    use prusa_watch::notify::{Event, Notifier};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let transport = Arc::new(RecordingTransport::default());
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts;
    *transport.respond.lock().unwrap() = Some(Box::new(move |_| {
        HttpResponse::new(
            if count.fetch_add(1, Ordering::Relaxed) == 0 {
                503
            } else {
                200
            },
            vec![],
        )
    }));
    let mut cfg = NotifyConfig::default();
    cfg.ntfy.topic = "test".into();
    let notifier = Arc::new(Notifier::with_transport(cfg, "", "", transport.clone()));
    for title in ["one", "two", "three"] {
        notifier.send(Event::new("info", title, "message", "printer"), None);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while notifier.sent.load(Ordering::Relaxed) < 3 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    notifier.shutdown();
    assert_eq!(notifier.sent.load(Ordering::Relaxed), 3);
    assert_eq!(notifier.errors.load(Ordering::Relaxed), 0);
    assert_eq!(
        transport
            .requests()
            .iter()
            .map(|r| header(r, "Title"))
            .collect::<Vec<_>>(),
        ["one", "one", "two", "three"]
    );
}

#[test]
fn notification_channels_do_not_wait_for_each_other() {
    use prusa_watch::config::NotifyConfig;
    use prusa_watch::http::{HttpRequest, HttpResponse, Transport};
    use prusa_watch::notify::{Event, Notifier};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    struct SlowChannel {
        first: AtomicBool,
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl Transport for SlowChannel {
        fn send(&self, request: &HttpRequest, _: std::time::Duration) -> Result<HttpResponse, String> {
            if request.url.contains("ntfy") && !self.first.swap(true, Ordering::Relaxed) {
                self.started.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            Ok(HttpResponse::new(200, vec![]))
        }
    }
    let (started, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let transport = Arc::new(SlowChannel {
        first: AtomicBool::new(false),
        started,
        release: Mutex::new(wait),
    });
    let mut cfg = NotifyConfig::default();
    cfg.ntfy.topic = "test".into();
    cfg.discord.webhook_url = "https://discord.example/hook".into();
    let notifier = Arc::new(Notifier::with_transport(cfg, "", "", transport));
    notifier.send(Event::new("info", "test", "message", "printer"), None);
    ready.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while notifier.sent.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let independent = notifier.sent.load(Ordering::Relaxed) == 1;
    release.send(()).unwrap();
    notifier.shutdown();
    assert!(independent, "Discord was delayed by the ntfy endpoint");
}
