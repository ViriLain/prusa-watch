//! Shared test rig: the Rust equivalent of the Python tests' `rig` fixture.
//! Fake printer, fake camera, controllable clock, recorded notifications, and
//! the synthetic ONNX model (tests/fixtures/fake-model.onnx: confidence =
//! mean brightness * per-box scale, so a white frame looks like "lots of
//! spaghetti" and a black frame looks clean).
#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use image::RgbImage;
use prusa_watch::camera::{Frame, FrameSource};
use prusa_watch::config::Config;
use prusa_watch::detector::SpaghettiDetector;
use prusa_watch::http::{HttpRequest, HttpResponse, Transport};
use prusa_watch::monitor::{Monitor, Parts};
use prusa_watch::notify::Notifier;
use prusa_watch::prusalink::{Printer, PrinterStatus, PrusaLinkError};
use serde_json::{Value, json};

pub fn fake_model_path() -> String {
    format!("{}/tests/fixtures/fake-model.onnx", env!("CARGO_MANIFEST_DIR"))
}

pub fn fake_detector() -> Arc<SpaghettiDetector> {
    static DET: OnceLock<Arc<SpaghettiDetector>> = OnceLock::new();
    DET.get_or_init(|| Arc::new(SpaghettiDetector::load(fake_model_path(), false).unwrap()))
        .clone()
}

pub fn solid(v: u8) -> RgbImage {
    RgbImage::from_pixel(640, 360, image::Rgb([v, v, v]))
}

#[derive(Clone)]
pub struct Clock(pub Arc<AtomicU64>);

impl Clock {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(1_000_000f64.to_bits())))
    }
    pub fn t(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::SeqCst))
    }
    pub fn set(&self, t: f64) {
        self.0.store(t.to_bits(), Ordering::SeqCst)
    }
    pub fn add(&self, dt: f64) {
        self.set(self.t() + dt)
    }
    pub fn as_fn(&self) -> prusa_watch::monitor::Clock {
        let c = self.clone();
        Arc::new(move || c.t())
    }
}

pub struct PrinterState {
    pub state: String,
    pub job_id: Option<i64>,
    pub calls: Vec<(String, i64)>,
    pub reachable: bool,
    pub fail_pause: bool,
    /// emulate a pause still in flight: after pause(), report PRINTING for this many polls
    pub pause_lag_polls: usize,
    lag: usize,
}

pub struct FakePrinter(pub Mutex<PrinterState>);

impl FakePrinter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(PrinterState {
            state: "IDLE".into(),
            job_id: None,
            calls: vec![],
            reachable: true,
            fail_pause: false,
            pause_lag_polls: 0,
            lag: 0,
        })))
    }
    pub fn s(&self) -> std::sync::MutexGuard<'_, PrinterState> {
        self.0.lock().unwrap()
    }
    pub fn set(&self, state: &str, job_id: Option<i64>) {
        let mut s = self.s();
        s.state = state.into();
        s.job_id = job_id;
    }
    pub fn set_state(&self, state: &str) {
        self.s().state = state.into();
    }
    pub fn state(&self) -> String {
        self.s().state.clone()
    }
    pub fn calls(&self) -> Vec<(String, i64)> {
        self.s().calls.clone()
    }
    pub fn count(&self, what: &str, job: i64) -> usize {
        self.calls().iter().filter(|(w, j)| w == what && *j == job).count()
    }
}

impl Printer for FakePrinter {
    fn status(&self) -> Result<PrinterStatus, PrusaLinkError> {
        let mut s = self.s();
        if s.lag > 0 {
            s.lag -= 1;
            if s.lag == 0 {
                s.state = "PAUSED".into();
            }
        }
        if !s.reachable {
            return Err(PrusaLinkError("GET /api/v1/status: timed out".into()));
        }
        Ok(PrinterStatus {
            state: s.state.clone(),
            job_id: s.job_id,
            progress: Some(10.0),
            time_printing: Some(100),
            temp_nozzle: Some(215.0),
            temp_bed: Some(60.0),
            raw: json!({}),
        })
    }
    fn job(&self) -> Result<Option<Value>, PrusaLinkError> {
        Ok(self
            .s()
            .job_id
            .map(|id| json!({"id": id, "file": {"display_name": format!("part-{id}.bgcode")}})))
    }
    fn job_name(&self) -> Option<String> {
        self.s().job_id.map(|id| format!("part-{id}.bgcode"))
    }
    fn pause(&self, job_id: i64) -> Result<(), PrusaLinkError> {
        let mut s = self.s();
        if s.job_id != Some(job_id) || s.state != "PRINTING" {
            return Err(PrusaLinkError("pause: wrong job or state".into()));
        }
        s.calls.push(("pause".into(), job_id));
        if s.fail_pause {
            return Err(PrusaLinkError("PUT pause: HTTP 409".into()));
        }
        if s.pause_lag_polls > 0 {
            s.lag = s.pause_lag_polls; // command accepted, not paused yet
        } else {
            s.state = "PAUSED".into();
        }
        Ok(())
    }
    fn resume(&self, job_id: i64) -> Result<(), PrusaLinkError> {
        let mut s = self.s();
        if s.job_id != Some(job_id) || s.state != "PAUSED" {
            return Err(PrusaLinkError("resume: wrong job or state".into()));
        }
        s.calls.push(("resume".into(), job_id));
        s.state = "PRINTING".into();
        Ok(())
    }
    fn stop(&self, job_id: i64) -> Result<(), PrusaLinkError> {
        let mut s = self.s();
        if s.job_id != Some(job_id) || !["PRINTING", "PAUSED", "ATTENTION"].contains(&s.state.as_str()) {
            return Err(PrusaLinkError("stop: wrong job or state".into()));
        }
        s.calls.push(("stop".into(), job_id));
        s.state = "STOPPED".into();
        Ok(())
    }
}

pub struct GrabberState {
    pub image: Option<RgbImage>,
    pub connected: bool,
    pub stale: bool,
    pub sequence: u64,
    pub decoded_at: f64,
}

pub struct FakeGrabber {
    pub clock: Clock,
    pub st: Mutex<GrabberState>,
}

impl FakeGrabber {
    pub fn new(clock: Clock) -> Arc<Self> {
        let decoded_at = clock.t();
        Arc::new(Self {
            clock,
            st: Mutex::new(GrabberState {
                image: Some(solid(0)),
                connected: true,
                stale: false,
                sequence: 1,
                decoded_at,
            }),
        })
    }
    pub fn set_image(&self, img: Option<RgbImage>) {
        self.st.lock().unwrap().image = img;
        self.publish();
    }
    pub fn set_stale(&self, stale: bool) {
        self.st.lock().unwrap().stale = stale;
    }
    pub fn publish(&self) {
        let mut state = self.st.lock().unwrap();
        state.sequence += 1;
        state.decoded_at = self.clock.t();
    }
}

impl FrameSource for FakeGrabber {
    fn start(&self) {}
    fn stop(&self) {}
    fn latest(&self) -> Option<Frame> {
        let s = self.st.lock().unwrap();
        let img = s.image.clone()?;
        let ts = s.decoded_at - if s.stale { 120.0 } else { 0.0 };
        Some(Frame {
            sequence: s.sequence,
            image: Arc::new(img),
            ts,
            monotonic_ts: ts,
        })
    }
    fn connected(&self) -> bool {
        self.st.lock().unwrap().connected
    }
}

pub type Responder = Box<dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync>;

/// Records every request; answers 200 (or a scripted response).
#[derive(Default)]
pub struct RecordingTransport {
    pub sent: Mutex<Vec<HttpRequest>>,
    pub respond: Mutex<Option<Responder>>,
}

impl RecordingTransport {
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.sent.lock().unwrap().clone()
    }
    pub fn clear(&self) {
        self.sent.lock().unwrap().clear();
    }
}

impl Transport for RecordingTransport {
    fn send(&self, req: &HttpRequest, _t: Duration) -> Result<HttpResponse, String> {
        self.sent.lock().unwrap().push(req.clone());
        if let Some(f) = self.respond.lock().unwrap().as_ref() {
            return Ok(f(req));
        }
        Ok(HttpResponse::json(200, json!({"id": "x"})))
    }
}

pub struct Rig {
    pub mon: Arc<Monitor>,
    pub printer: Arc<FakePrinter>,
    pub grabber: Arc<FakeGrabber>,
    pub clock: Clock,
    pub transport: Arc<RecordingTransport>,
    pub dir: tempfile::TempDir,
}

pub fn base_config(dir: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.printer.host = "printer".into();
    cfg.printer.password = "x".into();
    cfg.camera.url = "rtsp://cam/live".into();
    cfg.state_dir = dir.join("data").to_string_lossy().into_owned();
    cfg.notify.ntfy.topic = "prusa-test".into();
    cfg.notify.ntfy.url = "https://ntfy.example".into();
    cfg.web.public_url = "http://watch.lan:8484".into();
    cfg.web.token = "tok".into();
    // Most tests exercise the simplest policy; escalation tests override this.
    cfg.escalation = serde_yaml::from_str("{default_policy: pause_now, schedules: []}").unwrap();
    cfg
}

pub fn rig() -> Rig {
    rig_with(|_| {})
}

pub fn rig_with(tweak: impl FnOnce(&mut Config)) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path());
    tweak(&mut cfg);
    let clock = Clock::new();
    let printer = FakePrinter::new();
    let grabber = FakeGrabber::new(clock.clone());
    let transport = Arc::new(RecordingTransport::default());
    let mut notifier = Notifier::with_transport(
        cfg.notify.clone(),
        &cfg.web.public_url,
        &cfg.web.token,
        transport.clone(),
    );
    notifier.blocking = true;
    let mon = Monitor::new(
        cfg,
        Parts {
            printer: Some(printer.clone()),
            grabber: Some(grabber.clone()),
            detector: Some(fake_detector()),
            notifier: Some(Arc::new(notifier)),
            clock: Some(clock.as_fn()),
            ..Default::default()
        },
    )
    .unwrap();
    Rig {
        mon,
        printer,
        grabber,
        clock,
        transport,
        dir,
    }
}

impl Rig {
    pub fn advance(&self, n: usize) {
        self.advance_by(n, 10.0)
    }
    pub fn advance_by(&self, n: usize, step: f64) {
        for _ in 0..n {
            self.clock.add(step);
            self.grabber.publish();
            self.mon.tick();
            let _ = self.mon.flush_storage(); // write faults are asserted through monitor telemetry
        }
    }
    pub fn sent(&self) -> Vec<HttpRequest> {
        self.transport.requests()
    }
    pub fn ntfy(&self) -> Vec<HttpRequest> {
        self.sent()
            .into_iter()
            .filter(|r| r.url.contains("ntfy.example"))
            .collect()
    }
    pub fn hooks(&self) -> Vec<Value> {
        self.sent()
            .into_iter()
            .filter(|r| r.url.contains("ha.example"))
            .filter_map(|r| match r.body {
                prusa_watch::http::Body::Json(v) => Some(v),
                _ => None,
            })
            .collect()
    }
    pub fn with_priority(&self, p: &str) -> Vec<HttpRequest> {
        self.sent()
            .into_iter()
            .filter(|r| r.get_header("Priority") == Some(p))
            .collect()
    }
    pub fn titles(&self) -> Vec<String> {
        self.sent()
            .iter()
            .filter_map(|r| r.get_header("Title").map(str::to_string))
            .collect()
    }
    pub fn state_dir(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.mon.cfg.state_dir)
    }
}

pub fn header<'a>(r: &'a HttpRequest, k: &str) -> &'a str {
    r.get_header(k).unwrap_or("")
}

pub fn body_bytes(r: &HttpRequest) -> Vec<u8> {
    match &r.body {
        prusa_watch::http::Body::Bytes(b) => b.clone(),
        _ => vec![],
    }
}
