//! Failure modes: the monitor must degrade loudly, never silently, and never wedge.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use image::RgbImage;
use prusa_watch::detector::{Detect, Detection};
use prusa_watch::monitor::{Monitor, Parts};
use prusa_watch::notify::Notifier;

struct BrokenDetector;

impl Detect for BrokenDetector {
    fn try_detect(&self, _: &RgbImage, _: f64, _: f64) -> Result<Vec<Detection>, String> {
        Err("tract: out of memory".into())
    }
    fn last_inference_ms(&self) -> f64 {
        0.0
    }
}

#[test]
fn inference_error_skips_the_frame_instead_of_reading_as_clean() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = base_config(dir.path());
    let clock = Clock::new();
    let printer = FakePrinter::new();
    let grabber = FakeGrabber::new(clock.clone());
    let transport = Arc::new(RecordingTransport::default());
    let mut n = Notifier::with_transport(cfg.notify.clone(), "", "", transport);
    n.blocking = true;
    let mon = Monitor::new(
        cfg,
        Parts {
            printer: Some(printer.clone()),
            grabber: Some(grabber),
            detector: Some(Arc::new(BrokenDetector)),
            notifier: Some(Arc::new(n)),
            clock: Some(clock.as_fn()),
        },
    )
    .unwrap();
    let before = mon.core().decider.state.clone();
    printer.set("PRINTING", Some(1));
    for _ in 0..20 {
        clock.add(10.0);
        mon.tick();
    }
    assert_eq!(mon.counters().frames_analyzed, 0, "no frame counts as analyzed");
    let after = mon.core().decider.state.clone();
    assert_eq!(after.lifetime_frame_num, before.lifetime_frame_num, "baseline not fed zeros");
}

#[test]
fn dashboard_state_does_not_wait_for_a_busy_monitor() {
    let r = rig();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    let app = prusa_watch::web::router(r.mon.clone());
    rt.spawn(async move { axum::serve(listener, app).await.unwrap() });

    // hold the control lock the way a tick does during a slow PrusaLink call
    let guard = r.mon.core();
    let client = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
    let t = Instant::now();
    let resp = client.get(format!("http://{addr}/api/state")).send().unwrap();
    let health = client.get(format!("http://{addr}/healthz")).send().unwrap();
    let took = t.elapsed();
    drop(guard);
    assert_eq!(resp.status(), 200);
    assert!(health.status() == 200 || health.status() == 503);
    assert!(took < Duration::from_secs(2), "dashboard blocked on the control lock for {took:?}");
}

#[test]
fn empty_step_title_uses_the_default() {
    let r = rig_with(|c| {
        c.escalation =
            serde_yaml::from_str("{default_policy: p, policies: {p: {steps: [{at: 0, title: '', message: ''}]}}}").unwrap()
    });
    r.printer.set("PRINTING", Some(3));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    r.advance(30);
    let alert = r.ntfy().into_iter().find(|q| header(q, "Tags").contains("hourglass") || header(q, "Priority") == "5").unwrap();
    assert!(header(&alert, "Title").contains("print failure detected"), "{}", header(&alert, "Title"));
    assert!(header(&alert, "Message").contains("Spaghetti detected"));
}

#[test]
fn reply_handler_panic_does_not_kill_the_listener() {
    use prusa_watch::replies::{NtfyReplyListener, StreamOpener};
    struct NoStream;
    impl StreamOpener for NoStream {
        fn open(&self, _: &str, _: &str, _: &[(String, String)]) -> Result<Box<dyn std::io::BufRead + Send>, String> {
            Err("unused".into())
        }
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c2 = calls.clone();
    let handler: prusa_watch::replies::Handler = Arc::new(move |cmd: &str, _: &str| {
        c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if cmd == "veto" {
            panic!("boom");
        }
        "ok".to_string()
    });
    let l = NtfyReplyListener::with_opener(prusa_watch::config::NtfyConfig::default(), handler, Arc::new(NoStream));
    let line = |m: &str| format!(r#"{{"id":"x","event":"message","message":"{m}"}}"#);
    assert_eq!(l.handle_line(&line("veto abc")), None);
    assert_eq!(l.handle_line(&line("act abc")).as_deref(), Some("ok"), "still handling after a panic");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}
