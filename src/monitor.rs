//! Main watch loop: printer state -> frame -> detection -> decision -> escalation.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use image::{Rgb, RgbImage};
use serde::Serialize;
use serde_json::{Value, json};

use crate::camera::{FrameSource, Grabber};
use crate::config::{Config, EventNotifyConfig};
use crate::decision::{FailureDecider, Verdict};
use crate::detector::{Detect, Detection, SpaghettiDetector, annotate};
use crate::escalation::{Incident, PolicyResolver, Step, fmt_duration, format_template, parse_escalation};
use crate::imaging::{crop_roi, draw_rect, encode_jpeg};
use crate::notify::{Event, Notifier};
use crate::prusalink::{Printer, PrinterStatus, PrusaLink, PrusaLinkError};
use crate::recording::Recorder;
use crate::replies::NtfyReplyListener;
pub use crate::session::JobContext;
use crate::session::{Action, ActionOutcome, PendingAction, Session};

pub const ACTIVE: [&str; 3] = ["PRINTING", "PAUSED", "ATTENTION"];

fn is_active(state: &str) -> bool {
    ACTIVE.contains(&state)
}

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// A one-off preview leaves the decision state unchanged.
pub struct DetectionPreview {
    pub detections: Vec<Detection>,
    pub jpeg: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Counters {
    pub frames_analyzed: u64,
    pub warnings: u64,
    /// incidents opened
    pub failures: u64,
    pub pauses: u64,
    pub stops: u64,
    pub printer_errors: u64,
    /// "Keep printing"
    pub vetoes: u64,
    /// actions taken because a step's time came with no answer
    pub auto_actions: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryPoint {
    pub ts: f64,
    pub p: f64,
    pub score: f64,
    pub verdict: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub printer_state: String,
    pub printer_reachable: bool,
    pub printer_error: Option<String>,
    pub job: JobContext,
    pub verdict: String,
    pub score: f64,
    pub current_p: f64,
    pub ewm_mean: f64,
    pub baseline: f64,
    pub frame_num: i64,
    pub grace_frames_left: i64,
    pub camera_connected: bool,
    pub frame_age_s: Option<f64>,
    pub inference_ms: f64,
    pub last_detections: Vec<Value>,
    pub last_analysis_ts: Option<f64>,
    pub progress: Option<f64>,
    pub incident: Option<Value>,
    pub policy: Value,
    pub background_io: Value,
    pub pending_action: Option<PendingAction>,
    pub action_error: Option<String>,
    pub detector_error: Option<String>,
    pub loop_age_s: Option<f64>,
    pub analysis_age_s: Option<f64>,
    pub protection_status: String,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            printer_state: "UNKNOWN".into(),
            printer_reachable: false,
            printer_error: None,
            job: JobContext::default(),
            verdict: Verdict::Ok.as_str().into(),
            score: 0.0,
            current_p: 0.0,
            ewm_mean: 0.0,
            baseline: 0.0,
            frame_num: 0,
            grace_frames_left: 0,
            camera_connected: false,
            frame_age_s: None,
            inference_ms: 0.0,
            last_detections: vec![],
            last_analysis_ts: None,
            progress: None,
            incident: None,
            policy: json!({}),
            background_io: json!({}),
            pending_action: None,
            action_error: None,
            detector_error: None,
            loop_age_s: None,
            analysis_age_s: None,
            protection_status: "starting".into(),
        }
    }
}

/// Everything the control loop and replies mutate (serialized by one lock).
pub struct Core {
    pub session: Session,
    pub decider: FailureDecider,
    pub recorder: Recorder,
    pub policies: PolicyResolver,
    pub counters: Counters,
    pub last_status: Option<PrinterStatus>,
    recovery: Option<crate::session::Checkpoint>,
    pub last_detect_ts: f64,
    last_frame_sequence: Option<u64>,
    camera_down_notified: bool,
    printer_error_logged: bool,
    last_routed: HashMap<String, f64>,
}

impl std::ops::Deref for Core {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}

impl std::ops::DerefMut for Core {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.session
    }
}

/// Read model for the dashboard/API (separate lock, never held during I/O).
struct View {
    snap: Snapshot,
    history: VecDeque<HistoryPoint>,
    history_max: usize,
    latest_annotated: Option<Arc<Vec<u8>>>,
    counters: Counters,
    incident: Option<Incident>,
    last_tick: Option<f64>,
    last_poll: Option<f64>,
    last_analysis: Option<f64>,
    last_frame: Option<f64>,
}

pub struct Monitor {
    pub cfg: Config,
    pub printer: Arc<dyn Printer>,
    pub grabber: Arc<dyn FrameSource>,
    pub detector: Arc<dyn Detect>,
    pub notifier: Arc<Notifier>,
    pub clock: Clock,
    wall_clock: Clock,
    pub state_dir: PathBuf,
    storage: crate::storage::Storage,
    core: Mutex<Core>,
    tick_lock: Mutex<()>,
    view: Mutex<View>,
    stop: (Mutex<bool>, Condvar),
    thread: Mutex<Option<JoinHandle<()>>>,
    replies: Mutex<Option<Arc<NtfyReplyListener>>>,
    me: Weak<Monitor>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // a panic in one tick must not wedge the monitor forever
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
pub struct Parts {
    pub printer: Option<Arc<dyn Printer>>,
    pub grabber: Option<Arc<dyn FrameSource>>,
    pub detector: Option<Arc<dyn Detect>>,
    pub notifier: Option<Arc<Notifier>>,
    pub clock: Option<Clock>,
    pub wall_clock: Option<Clock>,
}

impl Monitor {
    /// Build with real components for anything not supplied in `parts`.
    pub fn new(cfg: Config, parts: Parts) -> anyhow::Result<Arc<Self>> {
        cfg.validate()?;
        let state_dir = PathBuf::from(&cfg.state_dir);
        std::fs::create_dir_all(&state_dir)?;
        let p = &cfg.printer;
        let printer = match parts.printer {
            Some(pr) => pr,
            None => Arc::new(PrusaLink::new(
                &p.host,
                &p.password,
                &p.username,
                &p.auth,
                &p.scheme,
                p.timeout_s,
            )),
        };
        let c = &cfg.camera;
        let grabber: Arc<dyn FrameSource> = match parts.grabber {
            Some(g) => g,
            None => Arc::new(Grabber::new(
                &c.url,
                &c.transport,
                c.reconnect_backoff_s,
                c.open_timeout_s,
                c.read_timeout_s,
            )),
        };
        let detector: Arc<dyn Detect> = match parts.detector {
            Some(d) => d,
            None => {
                crate::model::verify_digest(
                    std::path::Path::new(&cfg.detector.model_path),
                    &cfg.detector.expected_sha256,
                )?;
                Arc::new(SpaghettiDetector::load(&cfg.detector.model_path, cfg.detector.use_gpu)?)
            }
        };
        let notifier = match parts.notifier {
            Some(notifier) => notifier,
            None => {
                let mut notifier = Notifier::new(cfg.notify.clone(), &cfg.web.public_url, &cfg.web.token);
                notifier.signer = crate::capability::Signer::load(&state_dir.join("control-key"))?;
                Arc::new(notifier)
            }
        };
        let wall_clock = parts
            .wall_clock
            .unwrap_or_else(|| parts.clock.clone().unwrap_or_else(|| Arc::new(crate::now_ts)));
        let clock = parts.clock.unwrap_or_else(|| Arc::new(crate::monotonic_ts));
        let esc = parse_escalation(Some(&cfg.escalation), true)?;
        let policies = PolicyResolver::new(esc, &cfg.timezone)?;
        let digest = crate::session::Checkpoint::digest(&cfg);
        let recovery = std::fs::read(state_dir.join("session.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<crate::session::Checkpoint>(&bytes).ok())
            .filter(|saved| saved.valid(&digest, wall_clock()));
        let mut decider = FailureDecider::new(cfg.decision.clone(), Some(&state_dir.join("prediction_state.json")));
        // The storage worker persists combined snapshots; update() performs no disk I/O here.
        decider.state_path = None;
        let core = Core {
            session: Session::default(),
            decider,
            recovery,
            recorder: Recorder::new(cfg.recording.clone(), &state_dir),
            policies,
            counters: Counters::default(),
            last_status: None,
            last_detect_ts: f64::NEG_INFINITY,
            last_frame_sequence: None,
            camera_down_notified: false,
            printer_error_logged: false,
            last_routed: HashMap::new(),
        };
        let history_max = 10usize.max((cfg.web.history_s / cfg.detector.interval_s.max(1.0)) as usize + 1);
        let view = View {
            snap: Snapshot::default(),
            history: VecDeque::new(),
            history_max,
            latest_annotated: None,
            counters: Counters::default(),
            incident: None,
            last_tick: None,
            last_poll: None,
            last_analysis: None,
            last_frame: None,
        };
        let storage = crate::storage::Storage::new(cfg.recording.clone(), &state_dir);
        Ok(Arc::new_cyclic(|me| Self {
            cfg,
            printer,
            grabber,
            detector,
            notifier,
            clock,
            wall_clock,
            state_dir,
            storage,
            core: Mutex::new(core),
            tick_lock: Mutex::new(()),
            view: Mutex::new(view),
            stop: (Mutex::new(false), Condvar::new()),
            thread: Mutex::new(None),
            replies: Mutex::new(None),
            me: me.clone(),
        }))
    }

    /// Direct access to the control state (tests, diagnostics).
    pub fn core(&self) -> MutexGuard<'_, Core> {
        lock(&self.core)
    }

    /// (Re)parse an escalation section + timezone (tests change policies on the fly).
    pub fn reload_escalation(&self, escalation: &serde_yaml::Mapping, tz: &str) -> anyhow::Result<()> {
        let esc = parse_escalation(Some(escalation), true)?;
        lock(&self.core).policies = PolicyResolver::new(esc, tz)?;
        Ok(())
    }

    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    // ------------------------------------------------------------------ lifecycle
    pub fn start(self: &Arc<Self>) {
        let mut thread = lock(&self.thread);
        if thread.is_some() || *lock(&self.stop.0) {
            return;
        }
        self.grabber.start();
        let ntfy = &self.cfg.notify.ntfy;
        let mut replies = lock(&self.replies);
        if !ntfy.reply_topic.is_empty() && replies.is_none() {
            let weak = Arc::downgrade(self);
            let handler = Arc::new(move |cmd: &str, iid: &str| match weak.upgrade() {
                Some(m) => m.handle_reply(cmd, iid),
                None => "ignored: shutting down".to_string(),
            });
            let l = NtfyReplyListener::new(ntfy.clone(), handler);
            l.start();
            *replies = Some(l);
        }
        drop(replies);
        let me = self.clone();
        *thread = Some(
            std::thread::Builder::new()
                .name("monitor".into())
                .spawn(move || me.run())
                .expect("spawn monitor"),
        );
    }

    pub fn stop(&self) {
        *lock(&self.stop.0) = true;
        self.stop.1.notify_all();
        self.grabber.stop();
        if let Some(r) = lock(&self.replies).as_ref() {
            r.stop();
        }
        if let Some(h) = lock(&self.thread).take() {
            let _ = h.join();
        }
        self.checkpoint(&lock(&self.core));
        if let Err(error) = self.storage.flush() {
            tracing::error!("Checkpoint flush failed: {error}");
        }
        self.storage.stop();
        self.notifier.shutdown();
    }

    pub fn flush_storage(&self) -> Result<(), String> {
        self.storage.flush()
    }

    pub fn replies_connected(&self) -> bool {
        lock(&self.replies)
            .as_ref()
            .is_some_and(|r| r.connected.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn run(self: Arc<Self>) {
        tracing::info!(
            "Monitor: watching {} ({})",
            self.cfg.printer.name,
            self.cfg.printer.host
        );
        loop {
            if *lock(&self.stop.0) {
                break;
            }
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.tick())).is_err() {
                tracing::error!("Monitor: unexpected error in tick");
            }
            let g = lock(&self.stop.0);
            let _ = self.stop.1.wait_timeout_while(
                g,
                Duration::from_secs_f64(self.cfg.printer.poll_interval_s.max(0.05)),
                |s| !*s,
            );
        }
    }

    // ------------------------------------------------------------------ core loop
    /// One iteration. Public so tests can drive it deterministically.
    pub fn tick(&self) {
        let _tick = lock(&self.tick_lock);
        // Slow reads and inference do not prevent a user from retiring an incident.
        let result = self.printer.status().map(|mut status| {
            if is_active(&status.state) && status.job_id.is_none() {
                status.job_id = self
                    .printer
                    .job()
                    .ok()
                    .flatten()
                    .and_then(|job| job.get("id").and_then(Value::as_i64));
            }
            status
        });
        let name = result
            .as_ref()
            .ok()
            .filter(|status| is_active(&status.state) && status.job_id != lock(&self.core).job.job_id)
            .and_then(|_| self.printer.job_name());
        let input = {
            let mut core = lock(&self.core);
            let input = self.tick_inner(&mut core, result, name);
            self.sync_counters(&core);
            input
        };
        if let Some((image, status, session_id)) = input {
            self.analyze(&image, &status, &session_id);
        }
        self.sync_counters(&lock(&self.core));
        lock(&self.view).last_tick = Some(self.now());
    }

    fn checkpoint(&self, core: &Core) {
        let wall = (self.wall_clock)();
        let mut session = core.session.clone();
        session.shift_time(wall - self.now());
        let checkpoint = crate::session::Checkpoint {
            version: 1,
            config_digest: crate::session::Checkpoint::digest(&self.cfg),
            saved_at: wall,
            time_printing: core.last_status.as_ref().and_then(|s| s.time_printing),
            session,
            prediction: core.decider.state.clone(),
        };
        if let Ok(bytes) = serde_json::to_vec(&checkpoint) {
            self.storage.checkpoint(vec![
                (self.state_dir.join("session.json"), bytes),
                (
                    self.state_dir.join("prediction_state.json"),
                    core.decider.checkpoint_bytes(),
                ),
            ]);
        }
    }

    fn sync_counters(&self, core: &Core) {
        self.checkpoint(core);
        let mut view = lock(&self.view);
        view.counters = core.counters.clone();
        view.snap.job = core.job.clone();
        view.snap.pending_action = core.pending_action.clone();
        view.snap.action_error = core.action_error.clone();
    }

    fn tick_inner(
        &self,
        core: &mut Core,
        result: Result<PrinterStatus, PrusaLinkError>,
        name: Option<String>,
    ) -> Option<(Arc<RgbImage>, PrinterStatus, String)> {
        let mut status = self.poll_printer(core, result)?;
        if let Some(action) = core.session.reconcile(status.job_id, &status.state) {
            self.action_confirmed(core, action);
        }
        self.handle_job_transitions(core, &mut status, name);
        // A newly restored request may already have completed while the host was down.
        if let Some(action) = core.session.reconcile(status.job_id, &status.state) {
            self.action_confirmed(core, action);
        }
        if core.incident.is_none()
            && let Some(pending) = core.pending_action.clone()
            && self.now() >= pending.deadline
        {
            let _ = self.do_action(core, pending.action, Some(pending.job_id));
        }
        // Keep what the printer actually reported. run_step marks `status` PAUSED/STOPPED
        // for the rest of this tick; if that leaked into last_status, a pause still in
        // flight (next poll reads PRINTING) would look like a resume at the printer.
        core.last_status = Some(status.clone());

        let now = self.now();
        self.process_incident(core, &mut status, now);
        self.refresh_policy_snapshot(core, now);

        let frame = self.grabber.latest();
        let frame_age = frame.as_ref().map(|f| (now - f.monotonic_ts).max(0.0));
        {
            let mut v = lock(&self.view);
            v.snap.camera_connected = self.grabber.connected();
            v.snap.frame_age_s = frame_age;
            v.last_frame = frame.as_ref().map(|f| f.monotonic_ts);
        }

        if status.state != "PRINTING" {
            return None;
        }
        let camera_ok = frame.is_some() && frame_age.is_some_and(|a| a <= self.cfg.camera.stale_after_s);
        self.handle_camera_health(core, camera_ok);
        if !camera_ok {
            return None;
        }
        if now - core.last_detect_ts < self.cfg.detector.interval_s {
            return None;
        }
        let frame = frame.unwrap();
        if core.last_frame_sequence == Some(frame.sequence) {
            return None;
        }
        core.last_frame_sequence = Some(frame.sequence);
        core.last_detect_ts = now;
        Some((frame.image, status, core.job.session_id.clone()))
    }

    fn poll_printer(&self, core: &mut Core, result: Result<PrinterStatus, PrusaLinkError>) -> Option<PrinterStatus> {
        match result {
            Err(e) => {
                core.counters.printer_errors += 1;
                if !core.printer_error_logged {
                    tracing::error!("Monitor: printer unreachable: {e}");
                    core.printer_error_logged = true;
                }
                let mut v = lock(&self.view);
                v.snap.printer_reachable = false;
                v.snap.printer_error = Some(e.to_string());
                None
            }
            Ok(status) => {
                if core.printer_error_logged {
                    tracing::info!("Monitor: printer reachable again");
                }
                core.printer_error_logged = false;
                let mut v = lock(&self.view);
                v.snap.printer_reachable = true;
                v.snap.printer_error = None;
                v.snap.printer_state = status.state.clone();
                v.snap.progress = status.progress;
                v.last_poll = Some(self.now());
                Some(status)
            }
        }
    }

    fn handle_job_transitions(&self, core: &mut Core, status: &mut PrinterStatus, name: Option<String>) {
        let prev = core.last_status.clone();
        let active = is_active(&status.state);

        // New job started (id changed while printing)
        if let Some(new_id) = status.job_id.filter(|id| active && Some(*id) != core.job.job_id) {
            tracing::info!(
                "Monitor: new job {new_id} ({}) - resetting per-print state",
                name.as_deref().unwrap_or("None")
            );
            if core.incident.is_some() {
                self.close_incident(core, "a new job started", false);
            }
            let recovered = core.recovery.take().filter(|saved| {
                saved.session.job.job_id == status.job_id
                    && saved.session.job.job_name == name
                    && saved
                        .time_printing
                        .zip(status.time_printing)
                        .is_some_and(|(before, current)| {
                            let downtime = (self.wall_clock)() - saved.saved_at;
                            let expected = before as f64 + downtime;
                            if status.state == "PAUSED" {
                                // Print time may stop partway through downtime when a pause completes.
                                current >= before.saturating_sub(5) && current as f64 <= expected + 60.0
                            } else {
                                (current as f64 - expected).abs() <= 60.0
                            }
                        })
            });
            if let Some(saved) = recovered {
                core.session = saved.session;
                core.session.shift_time(self.now() - (self.wall_clock)());
                core.decider.state = saved.prediction;
                // Preserve an existing pause/mute, but never replay an overdue intervention.
                if let Some(pending) = &mut core.session.pending_action {
                    pending.attempts = self.cfg.printer.action_max_attempts;
                }
                if core.incident.as_ref().is_some_and(|incident| {
                    incident
                        .next_action()
                        .is_some_and(|(_, step)| self.now() >= incident.started_ts + step.at)
                }) {
                    core.session.incident = None;
                    core.session.action_error =
                        Some("Intervention expired during restart; check the printer before rearming".into());
                }
                self.refresh_incident_snapshot(core, self.now());
            } else {
                core.session.replace_job(status.job_id, name.clone());
                core.decider.reset_for_new_print();
            }
            self.storage
                .start_job(core.recorder.cfg.clone(), status.job_id, name, (self.wall_clock)());
            lock(&self.view).history.clear();
        }

        let prev_state = prev.as_ref().map(|p| p.state.as_str());
        // Resumed (anywhere: printer knob, Prusa app, dashboard) after we paused it
        if prev_state == Some("PAUSED")
            && status.state == "PRINTING"
            && core.job.action_taken.as_deref() == Some("paused")
        {
            self.after_resume(core);
        }
        // Back to printing after you paused at the printer during an incident: same grace as our
        // resume, so a mess you're still clearing doesn't open a new incident the moment it resumes.
        else if prev_state.is_some_and(|s| is_active(s) && s != "PRINTING")
            && status.state == "PRINTING"
            && core.job.printer_handled
        {
            let grace = self.cfg.decision.resume_grace_s;
            tracing::info!(
                "Monitor: job {} resumed after a pause at the printer - incidents re-arm in {grace:.0}s",
                core.job.job_id.map(|j| j.to_string()).unwrap_or("None".into())
            );
            core.job.printer_handled = false;
            core.job.rearm_at = core.job.rearm_at.max(self.now() + grace);
        }

        // Print ended
        if !active && core.job.job_id.is_some() && prev_state.is_some_and(is_active) {
            tracing::info!(
                "Monitor: job {} ended with state {}",
                core.job.job_id.unwrap(),
                status.state
            );
            if core.incident.is_some() {
                self.close_incident(core, &format!("job ended ({})", status.state), false);
            }
            core.decider.reset_for_new_print();
        }

        lock(&self.view).snap.job = core.job.clone();
    }

    fn after_resume(&self, core: &mut Core) {
        let grace = self.cfg.decision.resume_grace_s;
        tracing::info!(
            "Monitor: job {} resumed after AI pause - incidents re-arm in {grace:.0}s",
            core.job.job_id.map(|j| j.to_string()).unwrap_or("None".into())
        );
        core.job.action_taken = None;
        core.job.rearm_at = self.now() + grace;
        core.job.last_warning_ts = 0.0;
        if core.incident.is_some() {
            self.close_incident(core, "resumed", false);
        }
    }

    fn handle_camera_health(&self, core: &mut Core, camera_ok: bool) {
        let name = &self.cfg.printer.name;
        if camera_ok {
            if core.camera_down_notified {
                core.camera_down_notified = false;
                self.notify_routed(
                    core,
                    &self.cfg.notify.camera,
                    "camera_up",
                    &format!("{name}: camera back online"),
                    "AI monitoring resumed.",
                    None,
                );
            }
            return;
        }
        if !core.camera_down_notified {
            core.camera_down_notified = true;
            tracing::warn!("Monitor: printing but camera frame is stale/missing - AI monitoring blind");
            self.notify_routed(
                core,
                &self.cfg.notify.camera,
                "camera_down",
                &format!("{name}: camera offline"),
                "Printer is printing but no fresh frames from the Buddy3D camera. AI monitoring is blind. \
                 Check that RTSP is enabled in the Prusa app.",
                None,
            );
        }
    }

    fn analyze(&self, image: &RgbImage, status: &PrinterStatus, session_id: &str) {
        let cfg = &self.cfg;
        let roi_img = crop_roi(image, cfg.camera.roi.as_deref());
        let detections = match self
            .detector
            .try_detect(&roi_img, cfg.detector.threshold, cfg.detector.nms)
        {
            Ok(d) => d,
            Err(e) => {
                // Don't feed "nothing seen" into the averages/baseline for a frame we couldn't analyze.
                tracing::error!("Monitor: inference failed, skipping frame: {e}");
                lock(&self.view).snap.detector_error = Some(e);
                return;
            }
        };
        let zones = ignore_zones_in_crop(cfg, roi_img.width(), roi_img.height());
        let (detections, ignored) = drop_ignored(detections, &zones);
        if !ignored.is_empty() {
            tracing::debug!("Analyze: ignored {} box(es) in camera.ignore zones", ignored.len());
        }
        let confs: Vec<f64> = detections.iter().map(|d| d.confidence).collect();
        let mut core = lock(&self.core);
        if core.job.session_id != session_id || core.job.job_id != status.job_id {
            return;
        }
        let verdict = core.decider.update(&confs);
        let s = core.decider.state.clone();
        core.counters.frames_analyzed += 1;
        drop(core);

        let label = format!(
            "{}  score {:.2}  p {:.2}",
            verdict.as_str().to_uppercase(),
            s.normalized_p,
            s.current_p
        );
        let mut annotated = annotate(
            &roi_img,
            &detections,
            cfg.detector.visualization_threshold,
            Some(&label),
        );
        draw_zones(&mut annotated, &zones);
        let jpeg = Arc::new(encode_jpeg(&annotated, 85));
        let now = self.now();
        let inference_ms = self.detector.last_inference_ms();
        let mut guard = lock(&self.core);
        if guard.job.session_id != session_id || guard.job.job_id != status.job_id {
            return;
        }
        let core = &mut *guard;

        {
            let mut v = lock(&self.view);
            v.last_analysis = Some(now);
            v.latest_annotated = Some(jpeg.clone());
            v.history.push_back(HistoryPoint {
                ts: now,
                p: s.current_p,
                score: s.normalized_p,
                verdict: verdict.as_str().into(),
            });
            while v.history.len() > v.history_max {
                v.history.pop_front();
            }
            let snap = &mut v.snap;
            snap.verdict = verdict.as_str().into();
            snap.score = s.normalized_p;
            snap.current_p = s.current_p;
            snap.ewm_mean = s.ewm_mean;
            snap.baseline = s.rolling_mean_long;
            snap.frame_num = s.current_frame_num;
            snap.grace_frames_left = (cfg.decision.init_safe_frame_num - s.current_frame_num).max(0);
            snap.inference_ms = inference_ms;
            snap.last_detections = detections
                .iter()
                .filter(|d| d.confidence >= cfg.detector.visualization_threshold)
                .map(Detection::as_list)
                .collect();
            snap.last_analysis_ts = Some(now);
            snap.detector_error = None;
        }

        self.storage.record(
            core.recorder.cfg.clone(),
            crate::storage::Record {
                now: (self.wall_clock)(),
                frame_num: s.current_frame_num,
                progress: status.progress,
                confidences: confs,
                ewm: s.ewm_mean,
                baseline: s.rolling_mean_long,
                short_mean: s.rolling_mean_short,
                score: s.normalized_p,
                verdict: verdict.as_str().into(),
                inference_ms,
                jpeg: jpeg.clone(),
            },
        );

        tracing::debug!(
            "Analyze: {} dets p={:.3} ewm={:.3} base={:.3} -> {}",
            detections.len(),
            s.current_p,
            s.ewm_mean,
            s.rolling_mean_long,
            verdict.as_str()
        );

        if core.job.muted
            || core.job.action_taken.is_some()
            || core.pending_action.is_some()
            || core.action_error.is_some()
            || core.incident.is_some()
            || now < core.job.rearm_at
        {
            return;
        }
        match verdict {
            // Obico's verdict rides the moving average, which outlives a short burst; don't act
            // on a frame that itself shows (almost) nothing.
            Verdict::Failure if s.current_p < cfg.decision.min_frame_p => tracing::info!(
                "Monitor: failure verdict on job {} but this frame's p={:.2} < decision.min_frame_p={}; not opening an incident",
                core.job.job_id.map(|j| j.to_string()).unwrap_or("None".into()),
                s.current_p,
                cfg.decision.min_frame_p
            ),
            Verdict::Failure => self.open_incident(core, status, jpeg, &annotated, now),
            Verdict::Warning => self.on_warning(core, jpeg),
            Verdict::Ok => {}
        }
    }

    // ------------------------------------------------------------------ warnings / routed notifications
    fn notify_routed(
        &self,
        core: &mut Core,
        route: &EventNotifyConfig,
        kind: &str,
        title: &str,
        message: &str,
        jpeg: Option<Arc<Vec<u8>>>,
    ) {
        if !route.enabled {
            tracing::info!("Notify ({kind} disabled): {title}");
            return;
        }
        if kind != "warning" {
            // warnings use a per-print cooldown in on_warning
            let now = self.now();
            if now - core.last_routed.get(kind).copied().unwrap_or(f64::NEG_INFINITY) < route.cooldown_s {
                return;
            }
            core.last_routed.insert(kind.into(), now);
        }
        let mut ev = self.event(core, kind, title, message, jpeg, None);
        ev.priority = route.priority;
        self.notifier.send(ev, route.channels.as_deref());
    }

    fn on_warning(&self, core: &mut Core, jpeg: Arc<Vec<u8>>) {
        let now = self.now();
        let route = &self.cfg.notify.warning;
        if now - core.job.last_warning_ts < route.cooldown_s {
            return;
        }
        core.job.last_warning_ts = now;
        core.job.warnings += 1;
        core.counters.warnings += 1;
        let score = core.decider.state.normalized_p;
        let job = core
            .job
            .job_name
            .clone()
            .unwrap_or_else(|| core.job.job_id.map(|j| j.to_string()).unwrap_or("None".into()));
        tracing::warn!(
            "Monitor: possible failure on job {} (score {score:.2})",
            core.job.job_id.map(|j| j.to_string()).unwrap_or("None".into())
        );
        self.notify_routed(
            core,
            route,
            "warning",
            &format!("{}: possible print failure", self.cfg.printer.name),
            &format!("Spaghetti detector is seeing something on '{job}'. Not acting yet (score {score:.2})."),
            Some(jpeg),
        );
    }

    // ------------------------------------------------------------------ escalation
    fn open_incident(
        &self,
        core: &mut Core,
        status: &PrinterStatus,
        jpeg: Arc<Vec<u8>>,
        _annotated: &RgbImage,
        now: f64,
    ) {
        core.counters.failures += 1;
        let job_id = status.job_id.or(core.job.job_id);
        if self.cfg.save_failure_frames {
            let dir = self.state_dir.join("failures");
            let name = format!(
                "{}_job{}.jpg",
                now as i64,
                job_id.map(|j| j.to_string()).unwrap_or("None".into())
            );
            self.storage.failure(dir.join(name), jpeg.clone());
        }
        let (policy, schedule) = core.policies.resolve((self.wall_clock)());
        let inc = Incident::new(
            policy.clone(),
            schedule.clone(),
            job_id,
            now,
            core.decider.state.normalized_p,
            jpeg.to_vec(),
        );
        tracing::warn!(
            "Monitor: failure on job {} (score {:.2}) - incident {}, policy '{}'{}",
            job_id.map(|j| j.to_string()).unwrap_or("None".into()),
            inc.score,
            inc.id,
            policy.name,
            schedule
                .as_ref()
                .map(|s| format!(" (schedule '{s}')"))
                .unwrap_or_default()
        );
        core.incident = Some(inc);
        let mut st = status.clone();
        self.run_due_steps(core, &mut st, now, false);
        self.refresh_incident_snapshot(core, now);
    }

    fn process_incident(&self, core: &mut Core, status: &mut PrinterStatus, now: f64) {
        let Some(inc) = &core.incident else { return };
        if status.job_id.is_some() && inc.job_id.is_some() && status.job_id != inc.job_id {
            self.close_incident(core, "job changed", false);
        } else if !is_active(&status.state) {
            self.close_incident(core, &format!("printer is {}", status.state), false);
        } else if status.state != "PRINTING" && inc.acted.is_none() {
            core.job.printer_handled = true;
            self.close_incident(
                core,
                &format!("printer is {} (handled at the printer)", status.state),
                false,
            );
        } else {
            self.run_due_steps(core, status, now, true);
        }
        self.refresh_incident_snapshot(core, now);
    }

    fn run_due_steps(&self, core: &mut Core, status: &mut PrinterStatus, now: f64, auto: bool) {
        let Some(mut inc) = core.incident.take() else { return };
        // The incident is out of `core` while its steps run; a panic in there must not lose it.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (idx, step) in inc.due(now) {
                inc.next_idx = idx + 1;
                if !self.run_step(core, &mut inc, &step, status, now, auto && step.at > 0.0) {
                    // The pause/stop didn't go through: keep the incident open and retry this
                    // step on the next poll instead of moving on (and snoozing) as if it had.
                    inc.next_idx = idx;
                    return false;
                }
            }
            true
        }));
        core.incident = Some(inc);
        match outcome {
            Ok(true) => self.maybe_finish(core),
            Ok(false) => {}
            Err(_) => tracing::error!("Monitor: unexpected error running incident steps (incident kept)"),
        }
    }

    fn maybe_finish(&self, core: &mut Core) {
        let Some(inc) = &core.incident else { return };
        if inc.steps_done()
            && inc.acted.as_deref() != Some("paused")
            && (inc.policy.has_action() || self.now() - inc.started_ts >= core.policies.esc.snooze_s.max(60.0))
        {
            // Paused incidents stay open so Resume / False alarm buttons keep working.
            let cooldown = inc.acted.is_none();
            self.close_incident(core, "all steps done", cooldown);
        }
    }

    /// Run one step. Returns false if its pause/stop failed (caller retries).
    fn run_step(
        &self,
        core: &mut Core,
        inc: &mut Incident,
        step: &Step,
        status: &mut PrinterStatus,
        now: f64,
        auto: bool,
    ) -> bool {
        let outcome = match step.action.as_deref() {
            Some("pause") if inc.acted.is_none() && status.state == "PRINTING" => {
                self.do_action(core, Action::Pause, inc.job_id)
            }
            Some("stop") if inc.acted.as_deref() != Some("stopped") && is_active(&status.state) => {
                self.do_action(core, Action::Stop, inc.job_id)
            }
            Some("pause") if inc.acted.as_deref() == Some("paused") => ActionOutcome::Confirmed(Action::Pause),
            _ => {
                self.notify_step(core, inc, step, now, None);
                return true;
            }
        };
        let (taken, err) = match outcome {
            ActionOutcome::Confirmed(action) => (Some(action.result().to_string()), None),
            ActionOutcome::Requested(_) => return false,
            ActionOutcome::Failed(error) => (None, Some(error)),
        };
        if let Some(t) = &taken {
            inc.acted = Some(t.clone());
            status.state = if t == "paused" {
                "PAUSED".into()
            } else {
                "STOPPED".into()
            };
            if auto {
                core.counters.auto_actions += 1;
            }
        }
        if let Some(err) = err {
            let action = step.action.as_deref().unwrap_or_default();
            inc.action_errors = inc.action_errors.saturating_add(1);
            if inc.action_errors == 1 {
                // Never let a failed pause/stop go quietly: every channel, max priority. Once per
                // incident; retries follow every poll and the normal alert goes out if one succeeds.
                let name = core
                    .job
                    .job_name
                    .clone()
                    .unwrap_or_else(|| inc.job_id.map(|j| j.to_string()).unwrap_or("None".into()));
                let mut ev = self.event(
                    core,
                    "failure",
                    &format!("{}: {action} FAILED", self.cfg.printer.name),
                    &format!(
                        "Spaghetti detected on '{name}' but the {action} command failed: {err}. Retries are bounded by the configured attempt limit. Check the printer NOW."
                    ),
                    Some(inc.jpeg.clone()),
                    None,
                );
                ev.priority = 5;
                ev.incident_id = Some(inc.id.clone());
                ev.policy = Some(inc.policy.name.clone());
                self.notifier.send(ev, None);
            } else {
                tracing::warn!(
                    "Monitor: {action} retry {} for incident {} failed: {err}",
                    inc.action_errors,
                    inc.id
                );
            }
            return false;
        }
        self.notify_step(core, inc, step, now, taken.as_deref());
        true
    }

    fn fresh_status(&self, job_id: i64) -> Result<PrinterStatus, PrusaLinkError> {
        let mut status = self.printer.status()?;
        if status.job_id.is_none() && is_active(&status.state) {
            status.job_id = self
                .printer
                .job()?
                .and_then(|job| job.get("id").and_then(Value::as_i64));
        }
        if status.job_id.is_none() && !is_active(&status.state) {
            status.job_id = Some(job_id);
        }
        if status.job_id != Some(job_id) {
            return Err(PrusaLinkError(
                "job changed; refresh the dashboard before acting".into(),
            ));
        }
        Ok(status)
    }

    fn action_confirmed(&self, core: &mut Core, action: Action) {
        match action {
            Action::Pause => core.counters.pauses += 1,
            Action::Stop => core.counters.stops += 1,
            Action::Resume => self.after_resume(core),
        }
    }

    fn do_action(&self, core: &mut Core, action: Action, job_id: Option<i64>) -> ActionOutcome {
        let Some(job_id) = job_id else {
            return ActionOutcome::Failed("no job id".into());
        };
        let now = self.now();
        let attempts = match &core.pending_action {
            Some(pending) if pending.action != action || pending.job_id != job_id => {
                return ActionOutcome::Failed("another printer action is awaiting confirmation".into());
            }
            Some(pending) if now < pending.deadline => return ActionOutcome::Requested(action),
            Some(pending) if pending.attempts >= self.cfg.printer.action_max_attempts => {
                let error = format!(
                    "{action:?} was not confirmed after {} attempts; check the printer",
                    pending.attempts
                );
                if core.action_error.as_deref() != Some(&error) {
                    let mut event = self.event(
                        core,
                        "failure",
                        &format!("{}: {action:?} FAILED", self.cfg.printer.name),
                        &error,
                        None,
                        None,
                    );
                    event.priority = 5;
                    self.notifier.send(event, None);
                }
                core.session.action_error = Some(error.clone());
                return ActionOutcome::Failed(error);
            }
            Some(pending) => pending.attempts + 1,
            None => 1,
        };
        let status = match self.fresh_status(job_id) {
            Ok(status) => status,
            Err(error) => {
                core.session
                    .request(action, job_id, now, self.cfg.printer.action_retry_s, attempts);
                core.session.action_error = Some(error.to_string());
                return ActionOutcome::Failed(error.to_string());
            }
        };
        if action.confirmed(&status.state) {
            if core.session.reconcile(Some(job_id), &status.state).is_some() {
                self.action_confirmed(core, action);
            }
            return ActionOutcome::Confirmed(action);
        }
        if (action == Action::Pause && status.state != "PRINTING")
            || (action == Action::Resume && status.state != "PAUSED")
            || (action == Action::Stop && !is_active(&status.state))
        {
            return ActionOutcome::Failed(format!("cannot {} while printer is {}", action.result(), status.state));
        }
        let res = match action {
            Action::Pause => self.printer.pause(job_id),
            Action::Resume => self.printer.resume(job_id),
            Action::Stop => self.printer.stop(job_id),
        };
        match res {
            Ok(()) => {
                core.session
                    .request(action, job_id, now, self.cfg.printer.action_confirmation_s, attempts);
                if let Ok(status) = self.fresh_status(job_id)
                    && core.session.reconcile(status.job_id, &status.state).is_some()
                {
                    self.action_confirmed(core, action);
                    ActionOutcome::Confirmed(action)
                } else {
                    ActionOutcome::Requested(action)
                }
            }
            Err(e) => {
                core.session
                    .request(action, job_id, now, self.cfg.printer.action_retry_s, attempts);
                tracing::error!("Monitor: FAILED to {:?} job {job_id}: {e}", action);
                core.session.action_error = Some(e.to_string());
                ActionOutcome::Failed(e.to_string())
            }
        }
    }

    fn notify_step(&self, core: &mut Core, inc: &Incident, step: &Step, now: f64, taken: Option<&str>) {
        if step.notify.as_ref().is_some_and(|n| n.is_empty()) {
            return;
        }
        let f = inc.template_fields(
            now,
            &self.cfg.printer.name,
            core.job.job_name.as_deref(),
            core.decider.state.normalized_p,
        );
        let na = inc.next_action();
        let fill = |t: &str| format_template(t, &f).unwrap_or_else(|_| t.to_string());
        // Python treated empty strings as unset (`if step.title:`)
        let title = if let Some(t) = step.title.as_ref().filter(|t| !t.is_empty()) {
            fill(t)
        } else if let Some(t) = taken {
            format!("{}: print {}", f["printer"], t.to_uppercase())
        } else if na.is_some() {
            let verb = if f["next_action"] == "pause" {
                "pausing"
            } else {
                "stopping"
            };
            format!("{}: {verb} in {} unless you respond", f["printer"], f["next_action_in"])
        } else {
            format!("{}: print failure detected", f["printer"])
        };
        let message = if let Some(m) = step.message.as_ref().filter(|m| !m.is_empty()) {
            fill(m)
        } else {
            let head = format!("Spaghetti detected on '{}' (score {}).", f["job"], f["score"]);
            let mut m = if let Some(t) = taken {
                let mut m = format!("{head} Print was {}.", t.to_uppercase());
                if na.is_some() {
                    m += &format!(
                        " Next: {} in {} unless you respond.",
                        f["next_action"], f["next_action_in"]
                    );
                }
                m
            } else if na.is_some() {
                format!(
                    "{head} Tap Keep printing if it's a false alarm. No response = {}.",
                    f["next_action"]
                )
            } else {
                format!(
                    "{head} Printer still running (policy '{}' does not act).",
                    inc.policy.name
                )
            };
            m += &format!(
                " [{}{}]",
                inc.policy.name,
                inc.schedule.as_ref().map(|s| format!(" / {s}")).unwrap_or_default()
            );
            m
        };
        let kind = if taken.is_some() { "failure" } else { "incident" };
        let jpeg = step.attach_image.then(|| inc.jpeg.clone());
        let mut ev = self.event(core, kind, &title, &message, jpeg, taken.map(str::to_string));
        ev.priority = step.priority;
        ev.buttons = step.buttons.clone().unwrap_or_else(|| inc.default_buttons());
        ev.incident_id = Some(inc.id.clone());
        ev.policy = Some(inc.policy.name.clone());
        if let Some((_, s)) = na {
            ev.next_action = s.action.clone();
            ev.next_action_ts = Some(inc.started_ts + s.at);
        }
        self.notifier.send(ev, step.notify.as_deref());
    }

    fn close_incident(&self, core: &mut Core, reason: &str, cooldown: bool) {
        let Some(inc) = core.incident.take() else { return };
        if cooldown {
            let snooze = core.policies.esc.snooze_s;
            if snooze > 0.0 {
                core.job.rearm_at = self.now() + snooze;
            } else {
                core.job.muted = true;
            }
        }
        tracing::info!("Monitor: incident {} closed - {reason}", inc.id);
        self.refresh_incident_snapshot(core, self.now());
    }

    /// Button / dashboard command for the current incident: veto|act|stop|resume|mute.
    pub fn handle_reply(&self, cmd: &str, incident_id: &str) -> String {
        let mut core = lock(&self.core);
        let now = self.now();
        let Some(inc) = &core.incident else {
            tracing::info!("Monitor: ignoring '{cmd}' for unknown/closed incident");
            return "ignored: no matching open incident".into();
        };
        if !constant_time_eq(incident_id.as_bytes(), inc.id.as_bytes()) {
            tracing::info!("Monitor: ignoring '{cmd}' for unknown/closed incident");
            return "ignored: no matching open incident".into();
        }
        if !inc.commands().contains(&cmd) {
            return format!(
                "ignored: '{cmd}' not applicable (incident is {})",
                inc.acted.clone().unwrap_or("waiting".into())
            );
        }
        let result = match self.apply_reply(&mut core, cmd, now) {
            Ok(r) => r,
            Err(e) => {
                self.sync_counters(&core);
                return format!("failed: {e}");
            }
        };
        self.refresh_incident_snapshot(&core, now);
        lock(&self.view).snap.job = core.job.clone();
        self.sync_counters(&core);
        result
    }

    fn apply_reply(&self, core: &mut Core, cmd: &str, now: f64) -> Result<String, PrusaLinkError> {
        let snooze = core.policies.esc.snooze_s;
        let snooze_txt = if snooze > 0.0 {
            format!("no new alerts for {}", fmt_duration(snooze))
        } else {
            "alerts muted for the rest of this print".into()
        };
        let inc_job = core.incident.as_ref().and_then(|i| i.job_id);
        let need_job = || inc_job.ok_or_else(|| PrusaLinkError("no job id".into()));
        match cmd {
            "veto" => {
                if core
                    .incident
                    .as_ref()
                    .is_some_and(|i| i.acted.as_deref() == Some("paused"))
                {
                    self.manual_action(core, Action::Resume, need_job()?)?;
                }
                core.counters.vetoes += 1;
                core.session.pending_action = core.pending_action.take().filter(|p| p.action == Action::Resume);
                self.close_incident(core, "kept printing by user", true);
                let name = self.cfg.printer.name.clone();
                self.notify_routed(
                    core,
                    &self.cfg.notify.info,
                    "info",
                    &format!("{name}: keeping the print running"),
                    &format!("Got it: {snooze_txt}."),
                    None,
                );
                Ok(format!("vetoed ({snooze_txt})"))
            }
            "act" => {
                let mut inc = core.incident.take().unwrap();
                let (idx, step) = {
                    let (i, s) = inc.next_action().expect("commands() guarantees a next action");
                    (i, s.clone())
                };
                inc.next_idx = idx + 1; // skip any reminders before it
                let last_state = core
                    .last_status
                    .as_ref()
                    .map(|s| s.state.clone())
                    .unwrap_or("PRINTING".into());
                let mut status = PrinterStatus::new(&last_state, inc.job_id);
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.run_step(core, &mut inc, &step, &mut status, now, false)
                }))
                .unwrap_or(false); // incident kept either way
                let action = step.action.as_deref().unwrap_or_default();
                if !ok {
                    inc.next_idx = idx;
                    inc.retry_idx = Some(idx); // the loop retries it on the next poll, not at its scheduled time
                    core.incident = Some(inc);
                    if core.pending_action.is_some() && core.action_error.is_none() {
                        return Ok(format!("{action} requested; awaiting confirmation"));
                    }
                    return Ok(format!("failed: {action} command failed; retrying"));
                }
                let acted = inc.acted.clone();
                core.incident = Some(inc);
                self.maybe_finish(core);
                Ok(acted.unwrap_or_else(|| format!("{action} skipped (printer is {})", status.state)))
            }
            "stop" => {
                let outcome = self.do_action(core, Action::Stop, inc_job);
                let taken = match outcome {
                    ActionOutcome::Confirmed(action) => Some(action.result().into()),
                    ActionOutcome::Requested(_) => return Ok("stop requested; awaiting confirmation".into()),
                    ActionOutcome::Failed(error) => return Err(PrusaLinkError(error)),
                };
                if let Some(i) = core.incident.as_mut() {
                    i.acted = taken;
                }
                self.close_incident(core, "stopped by user", false);
                Ok("stopped".into())
            }
            "resume" | "mute" => {
                let result = self.manual_action(core, Action::Resume, need_job()?)?;
                if cmd == "mute" {
                    self.set_muted_locked(core, true);
                    return Ok(format!("{result} (alerts muted for this print)"));
                }
                Ok(result)
            }
            other => Ok(format!("ignored: unknown command '{other}'")),
        }
    }

    fn refresh_incident_snapshot(&self, core: &Core, now: f64) {
        let mut v = lock(&self.view);
        v.snap.incident = core.incident.as_ref().map(|i| i.public(now));
        v.incident = core.incident.clone();
    }

    fn refresh_policy_snapshot(&self, core: &Core, _now: f64) {
        let (pol, sched) = core.policies.resolve((self.wall_clock)());
        let steps: Vec<Value> = pol
            .steps
            .iter()
            .map(|s| json!({"at": s.at, "action": s.action, "notify": s.notify, "priority": s.priority}))
            .collect();
        let replies_connected = self.replies_connected();
        lock(&self.view).snap.policy = json!({
            "policy": pol.name, "schedule": sched, "steps": steps, "replies_connected": replies_connected,
        });
    }

    fn event(
        &self,
        core: &Core,
        kind: &str,
        title: &str,
        message: &str,
        jpeg: Option<Arc<Vec<u8>>>,
        action_taken: Option<String>,
    ) -> Event {
        let mut e = Event::new(kind, title, message, &self.cfg.printer.name);
        e.job_id = core.job.job_id;
        e.job_name = core.job.job_name.clone();
        e.score = Some(core.decider.state.normalized_p);
        e.action_taken = action_taken;
        e.image_jpeg = jpeg;
        e
    }

    // ------------------------------------------------------------------ controls (web UI)
    /// Atomically bind browser controls to the session rendered when the button was created.
    pub fn control(&self, job_id: i64, session_id: &str, command: &str, mute: bool) -> Result<String, PrusaLinkError> {
        let mut core = lock(&self.core);
        if !core.session.accepts(job_id, session_id) {
            return Err(PrusaLinkError("job/session changed; refresh the dashboard".into()));
        }
        if matches!(command, "mute" | "unmute") {
            self.set_muted_locked(&mut core, command == "mute");
            return Ok(format!("{}d", command));
        }
        let action = Action::parse(command).ok_or_else(|| PrusaLinkError("unknown action".into()))?;
        let result = self.manual_action(&mut core, action, job_id)?;
        if mute {
            self.set_muted_locked(&mut core, true);
        }
        Ok(result)
    }

    pub fn resume(&self, mute: bool) -> Result<String, PrusaLinkError> {
        let mut core = lock(&self.core);
        let job_id = core.job.job_id.ok_or_else(|| PrusaLinkError("no active job".into()))?;
        let out = self.manual_action(&mut core, Action::Resume, job_id)?;
        let out = if mute {
            self.set_muted_locked(&mut core, true);
            format!("{out} (alerts muted for this print)")
        } else {
            out
        };
        self.sync_counters(&core);
        Ok(out)
    }

    pub fn stop_print(&self) -> Result<String, PrusaLinkError> {
        let mut core = lock(&self.core);
        let job_id = core.job.job_id.ok_or_else(|| PrusaLinkError("no active job".into()))?;
        self.manual_action(&mut core, Action::Stop, job_id)
    }

    pub fn pause(&self) -> Result<String, PrusaLinkError> {
        let mut core = lock(&self.core);
        let job_id = core.job.job_id.ok_or_else(|| PrusaLinkError("no active job".into()))?;
        self.manual_action(&mut core, Action::Pause, job_id)
    }

    fn manual_action(&self, core: &mut Core, action: Action, job_id: i64) -> Result<String, PrusaLinkError> {
        // A human command supersedes an older request; old scheduled steps must not resume later.
        core.session.pending_action = None;
        let outcome = self.do_action(core, action, Some(job_id));
        if matches!(outcome, ActionOutcome::Confirmed(Action::Stop | Action::Resume)) {
            self.close_incident(core, "handled by user", false);
        }
        self.refresh_incident_snapshot(core, self.now());
        self.sync_counters(core);
        match outcome {
            ActionOutcome::Confirmed(action) => Ok(action.result().into()),
            ActionOutcome::Requested(action) => Ok(format!("{} requested; awaiting confirmation", action.verb())),
            ActionOutcome::Failed(error) => Err(PrusaLinkError(error)),
        }
    }

    pub fn set_muted(&self, muted: bool) {
        let mut core = lock(&self.core);
        self.set_muted_locked(&mut core, muted);
    }

    fn set_muted_locked(&self, core: &mut Core, muted: bool) {
        let resume = core.pending_action.take().filter(|p| p.action == Action::Resume);
        core.session.mute(muted);
        if !muted {
            core.session.action_error = None;
            core.session.pending_action = None;
        }
        if resume.is_some() {
            core.session.pending_action = resume;
        }
        self.refresh_incident_snapshot(core, self.now());
        tracing::info!(
            "Monitor: alerts {} for job {}",
            if muted { "muted" } else { "unmuted" },
            core.job.job_id.map(|j| j.to_string()).unwrap_or("None".into())
        );
        self.sync_counters(core);
    }

    /// Run the model on the current frame without touching decision state (ROI tuning).
    pub fn test_detection(&self) -> Result<Option<DetectionPreview>, String> {
        let Some(frame) = self.grabber.latest() else {
            return Ok(None);
        };
        let img = crop_roi(&frame.image, self.cfg.camera.roi.as_deref());
        let dets = self
            .detector
            .try_detect(&img, self.cfg.detector.threshold, self.cfg.detector.nms)?;
        let zones = ignore_zones_in_crop(&self.cfg, img.width(), img.height());
        let (dets, _) = drop_ignored(dets, &zones);
        let total: f64 = dets.iter().map(|d| d.confidence).fold(0.0, |a, b| a + b);
        let mut annotated = annotate(
            &img,
            &dets,
            self.cfg.detector.visualization_threshold,
            Some(&format!("TEST  sum p {total:.2}")),
        );
        draw_zones(&mut annotated, &zones);
        Ok(Some(DetectionPreview {
            detections: dets,
            jpeg: encode_jpeg(&annotated, 85),
        }))
    }

    // ------------------------------------------------------------------ read models
    pub fn snapshot(&self) -> Snapshot {
        let v = lock(&self.view);
        let mut snap = v.snap.clone();
        let now = self.now();
        use std::sync::atomic::Ordering;
        snap.background_io = json!({
            "storage_errors": self.storage.stats().errors.load(Ordering::Relaxed),
            "storage_dropped": self.storage.stats().dropped.load(Ordering::Relaxed),
            "notification_errors": self.notifier.errors.load(Ordering::Relaxed),
            "notification_dropped": self.notifier.dropped(),
            "channels": self.notifier.delivery_health(),
        });
        snap.loop_age_s = v.last_tick.map(|time| (now - time).max(0.0));
        snap.analysis_age_s = v.last_analysis.map(|time| (now - time).max(0.0));
        snap.frame_age_s = v.last_frame.map(|time| (now - time).max(0.0));
        snap.protection_status = if *lock(&self.stop.0) {
            "stopped"
        } else if snap.loop_age_s.is_none() {
            "starting"
        } else if snap.loop_age_s.is_some_and(|age| age > self.loop_budget()) {
            "monitor loop stalled"
        } else if !snap.printer_reachable {
            "printer offline"
        } else if snap.action_error.is_some() {
            "printer action failed"
        } else if snap.pending_action.is_some() {
            "awaiting printer confirmation"
        } else if snap.job.muted {
            "muted for this print"
        } else if snap.printer_state != "PRINTING" {
            "waiting for a print"
        } else if snap.detector_error.is_some() {
            "detector failed"
        } else if !snap.camera_connected || !snap.frame_age_s.is_some_and(|age| age <= self.cfg.camera.stale_after_s) {
            "camera unavailable"
        } else if snap.grace_frames_left > 0 {
            "startup grace"
        } else if now < snap.job.rearm_at {
            "resume grace"
        } else {
            "monitoring"
        }
        .into();
        if let Some(inc) = &v.incident {
            // keep the countdown live between ticks
            snap.incident = Some(inc.public(self.now()));
        }
        snap
    }

    /// Process liveness and protection readiness have separate contracts.
    fn loop_budget(&self) -> f64 {
        (self.cfg.printer.poll_interval_s * 3.0 + self.cfg.printer.timeout_s * 2.0 + self.cfg.detector.interval_s * 2.0)
            .max(15.0)
    }

    pub fn readiness(&self) -> Result<(), String> {
        let snap = self.snapshot();
        if *lock(&self.stop.0) {
            return Err("monitor stopped".into());
        }
        let budget = self.loop_budget();
        if !snap.loop_age_s.is_some_and(|age| age <= budget) {
            return Err("monitor loop is starting, stalled, or stopped".into());
        }
        if !snap.printer_reachable
            || !lock(&self.view)
                .last_poll
                .is_some_and(|time| self.now() - time <= budget)
        {
            return Err("printer status is unavailable or stale".into());
        }
        if let Some(error) = snap.action_error {
            return Err(error);
        }
        if snap.printer_state == "PRINTING" {
            if let Some(error) = snap.detector_error {
                return Err(error);
            }
            if !snap.camera_connected || !snap.frame_age_s.is_some_and(|age| age <= self.cfg.camera.stale_after_s) {
                return Err("camera frames are unavailable or stale".into());
            }
            if !snap.analysis_age_s.is_some_and(|age| age <= budget) {
                return Err("successful inference is unavailable or stale".into());
            }
        }
        Ok(())
    }

    pub fn counters(&self) -> Counters {
        lock(&self.view).counters.clone()
    }

    pub fn incident_id(&self) -> Option<String> {
        lock(&self.view).incident.as_ref().map(|i| i.id.clone())
    }

    pub fn history_points(&self) -> Vec<HistoryPoint> {
        lock(&self.view).history.iter().cloned().collect()
    }

    pub fn annotated_jpeg(&self) -> Option<Arc<Vec<u8>>> {
        lock(&self.view).latest_annotated.clone()
    }

    pub fn raw_jpeg(&self) -> Option<Vec<u8>> {
        let frame = self.grabber.latest()?;
        let mut img = (*frame.image).clone();
        if let Some(r) = self.cfg.camera.roi.as_ref().filter(|r| r.len() == 4) {
            let (w, h) = (img.width() as f64, img.height() as f64);
            draw_rect(
                &mut img,
                ((r[0] * w) as i64, (r[1] * h) as i64),
                ((r[2] * w) as i64, (r[3] * h) as i64),
                Rgb([0, 200, 255]),
                2,
            );
        }
        let (w, h) = (img.width() as f64, img.height() as f64);
        let full: Vec<[f64; 4]> = self
            .cfg
            .camera
            .ignore
            .iter()
            .filter(|z| z.len() == 4)
            .map(|z| [z[0] * w, z[1] * h, z[2] * w, z[3] * h])
            .collect();
        draw_zones(&mut img, &full);
        Some(encode_jpeg(&img, 80))
    }

    pub fn weak(&self) -> Weak<Monitor> {
        self.me.clone()
    }
}

/// `camera.ignore` zones (full-frame normalized) mapped into pixel coordinates of the
/// ROI-cropped image the model sees: [x1, y1, x2, y2].
pub fn ignore_zones_in_crop(cfg: &Config, crop_w: u32, crop_h: u32) -> Vec<[f64; 4]> {
    let roi: &[f64] = cfg
        .camera
        .roi
        .as_deref()
        .filter(|r| r.len() == 4)
        .unwrap_or(&[0.0, 0.0, 1.0, 1.0]);
    let (rw, rh) = (roi[2] - roi[0], roi[3] - roi[1]);
    if rw <= 0.0 || rh <= 0.0 {
        return vec![];
    }
    let (cw, ch) = (crop_w as f64, crop_h as f64);
    cfg.camera
        .ignore
        .iter()
        .filter(|z| z.len() == 4)
        .map(|z| {
            let fx = |x: f64| ((x - roi[0]) / rw).clamp(0.0, 1.0) * cw;
            let fy = |y: f64| ((y - roi[1]) / rh).clamp(0.0, 1.0) * ch;
            [fx(z[0]), fy(z[1]), fx(z[2]), fy(z[3])]
        })
        .filter(|z| z[2] > z[0] && z[3] > z[1])
        .collect()
}

/// Split detections into (kept, ignored) by whether the box centre lies in any zone.
pub fn drop_ignored(dets: Vec<Detection>, zones: &[[f64; 4]]) -> (Vec<Detection>, Vec<Detection>) {
    dets.into_iter().partition(|d| {
        let (cx, cy) = (d.bbox[0], d.bbox[1]);
        !zones
            .iter()
            .any(|z| cx >= z[0] && cx <= z[2] && cy >= z[1] && cy <= z[3])
    })
}

fn draw_zones(img: &mut RgbImage, zones: &[[f64; 4]]) {
    for z in zones {
        draw_rect(
            img,
            (z[0] as i64, z[1] as i64),
            (z[2] as i64, z[3] as i64),
            Rgb([150, 150, 150]),
            1,
        );
    }
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
