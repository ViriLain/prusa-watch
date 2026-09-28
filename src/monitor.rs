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
use crate::recording::{FrameRecord, Recorder};
use crate::replies::NtfyReplyListener;

pub const ACTIVE: [&str; 3] = ["PRINTING", "PAUSED", "ATTENTION"];

fn is_active(state: &str) -> bool {
    ACTIVE.contains(&state)
}

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct JobContext {
    pub job_id: Option<i64>,
    pub job_name: Option<String>,
    /// user said "not a failure" -> no more incidents this job
    pub muted: bool,
    /// "paused" / "stopped" by us
    pub action_taken: Option<String>,
    pub last_warning_ts: f64,
    pub warnings: i64,
    /// no new incidents until this time (after resume / keep printing)
    pub rearm_at: f64,
    /// you paused at the printer during an incident -> grace on resume
    pub printer_handled: bool,
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
        }
    }
}

/// Everything the control loop and replies mutate (serialized by one lock).
pub struct Core {
    pub job: JobContext,
    pub incident: Option<Incident>,
    pub decider: FailureDecider,
    pub recorder: Recorder,
    pub policies: PolicyResolver,
    pub counters: Counters,
    pub last_status: Option<PrinterStatus>,
    pub last_detect_ts: f64,
    camera_down_notified: bool,
    printer_error_logged: bool,
    last_routed: HashMap<String, f64>,
}

/// Read model for the dashboard/API (separate lock, never held during I/O).
struct View {
    snap: Snapshot,
    history: VecDeque<HistoryPoint>,
    history_max: usize,
    latest_annotated: Option<Arc<Vec<u8>>>,
    counters: Counters,
    incident: Option<Incident>,
}

pub struct Monitor {
    pub cfg: Config,
    pub printer: Arc<dyn Printer>,
    pub grabber: Arc<dyn FrameSource>,
    pub detector: Arc<dyn Detect>,
    pub notifier: Arc<Notifier>,
    pub clock: Clock,
    pub state_dir: PathBuf,
    core: Mutex<Core>,
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
}

impl Monitor {
    /// Build with real components for anything not supplied in `parts`.
    pub fn new(cfg: Config, parts: Parts) -> anyhow::Result<Arc<Self>> {
        let state_dir = PathBuf::from(&cfg.state_dir);
        std::fs::create_dir_all(&state_dir)?;
        let p = &cfg.printer;
        let printer = match parts.printer {
            Some(pr) => pr,
            None => Arc::new(PrusaLink::new(&p.host, &p.password, &p.username, &p.auth, &p.scheme, p.timeout_s)),
        };
        let c = &cfg.camera;
        let grabber: Arc<dyn FrameSource> = match parts.grabber {
            Some(g) => g,
            None => Arc::new(Grabber::new(&c.url, &c.transport, c.reconnect_backoff_s, c.open_timeout_s, c.read_timeout_s)),
        };
        let detector: Arc<dyn Detect> = match parts.detector {
            Some(d) => d,
            None => Arc::new(SpaghettiDetector::load(&cfg.detector.model_path, cfg.detector.use_gpu)?),
        };
        let notifier =
            parts.notifier.unwrap_or_else(|| Arc::new(Notifier::new(cfg.notify.clone(), &cfg.web.public_url, &cfg.web.token)));
        let clock = parts.clock.unwrap_or_else(|| Arc::new(crate::now_ts));
        let esc = parse_escalation(Some(&cfg.escalation), true)?;
        let policies = PolicyResolver::new(esc, &cfg.timezone)?;
        let core = Core {
            job: JobContext::default(),
            incident: None,
            decider: FailureDecider::new(cfg.decision.clone(), Some(&state_dir.join("prediction_state.json"))),
            recorder: Recorder::new(cfg.recording.clone(), &state_dir),
            policies,
            counters: Counters::default(),
            last_status: None,
            last_detect_ts: 0.0,
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
        };
        Ok(Arc::new_cyclic(|me| Self {
            cfg,
            printer,
            grabber,
            detector,
            notifier,
            clock,
            state_dir,
            core: Mutex::new(core),
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
        *lock(&self.thread) =
            Some(std::thread::Builder::new().name("monitor".into()).spawn(move || me.run()).expect("spawn monitor"));
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
        lock(&self.core).decider.save();
    }

    pub fn replies_connected(&self) -> bool {
        lock(&self.replies).as_ref().is_some_and(|r| r.connected.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn run(self: Arc<Self>) {
        tracing::info!("Monitor: watching {} ({})", self.cfg.printer.name, self.cfg.printer.host);
        loop {
            if *lock(&self.stop.0) {
                break;
            }
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.tick())).is_err() {
                tracing::error!("Monitor: unexpected error in tick");
            }
            let g = lock(&self.stop.0);
            let _ =
                self.stop.1.wait_timeout_while(g, Duration::from_secs_f64(self.cfg.printer.poll_interval_s.max(0.05)), |s| !*s);
        }
    }

    // ------------------------------------------------------------------ core loop
    /// One iteration. Public so tests can drive it deterministically.
    pub fn tick(&self) {
        let mut core = lock(&self.core);
        self.tick_inner(&mut core);
        self.sync_counters(&core);
    }

    fn sync_counters(&self, core: &Core) {
        lock(&self.view).counters = core.counters.clone();
    }

    fn tick_inner(&self, core: &mut Core) {
        let Some(mut status) = self.poll_printer(core) else { return };
        self.handle_job_transitions(core, &mut status);
        // Keep what the printer actually reported. run_step marks `status` PAUSED/STOPPED
        // for the rest of this tick; if that leaked into last_status, a pause still in
        // flight (next poll reads PRINTING) would look like a resume at the printer.
        core.last_status = Some(status.clone());

        let now = self.now();
        self.process_incident(core, &mut status, now);
        self.refresh_policy_snapshot(core, now);

        let frame = self.grabber.latest();
        let frame_age = frame.as_ref().map(|f| now - f.ts);
        {
            let mut v = lock(&self.view);
            v.snap.camera_connected = self.grabber.connected();
            v.snap.frame_age_s = frame_age;
        }

        if status.state != "PRINTING" {
            return;
        }
        let camera_ok = frame.is_some() && frame_age.is_some_and(|a| a <= self.cfg.camera.stale_after_s);
        self.handle_camera_health(core, camera_ok);
        if !camera_ok {
            return;
        }
        if now - core.last_detect_ts < self.cfg.detector.interval_s {
            return;
        }
        core.last_detect_ts = now;
        self.analyze(core, &frame.unwrap().image, &status);
    }

    fn poll_printer(&self, core: &mut Core) -> Option<PrinterStatus> {
        match self.printer.status() {
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
                Some(status)
            }
        }
    }

    fn handle_job_transitions(&self, core: &mut Core, status: &mut PrinterStatus) {
        let prev = core.last_status.clone();
        let active = is_active(&status.state);

        if active && status.job_id.is_none() {
            // Some firmware builds omit job.id from /status; fall back to /job.
            if let Ok(j) = self.printer.job() {
                status.job_id = j.and_then(|j| j.get("id").and_then(Value::as_i64));
            }
        }

        // New job started (id changed while printing)
        if let Some(new_id) = status.job_id.filter(|id| active && Some(*id) != core.job.job_id) {
            let name = self.printer.job_name();
            tracing::info!("Monitor: new job {new_id} ({}) - resetting per-print state", name.as_deref().unwrap_or("None"));
            if core.incident.is_some() {
                self.close_incident(core, "a new job started", false);
            }
            core.job = JobContext { job_id: status.job_id, job_name: name.clone(), ..Default::default() };
            core.decider.reset_for_new_print();
            core.recorder.start_job(status.job_id, name.as_deref(), self.now());
            lock(&self.view).history.clear();
        }

        let prev_state = prev.as_ref().map(|p| p.state.as_str());
        // Resumed (anywhere: printer knob, Prusa app, dashboard) after we paused it
        if prev_state == Some("PAUSED") && status.state == "PRINTING" && core.job.action_taken.as_deref() == Some("paused") {
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
            tracing::info!("Monitor: job {} ended with state {}", core.job.job_id.unwrap(), status.state);
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

    fn analyze(&self, core: &mut Core, image: &RgbImage, status: &PrinterStatus) {
        let cfg = &self.cfg;
        let roi_img = crop_roi(image, cfg.camera.roi.as_deref());
        let detections = match self.detector.try_detect(&roi_img, cfg.detector.threshold, cfg.detector.nms) {
            Ok(d) => d,
            Err(e) => {
                // Don't feed "nothing seen" into the averages/baseline for a frame we couldn't analyze.
                tracing::error!("Monitor: inference failed, skipping frame: {e}");
                return;
            }
        };
        let confs: Vec<f64> = detections.iter().map(|d| d.confidence).collect();
        let verdict = core.decider.update(&confs);
        let s = core.decider.state.clone();
        core.counters.frames_analyzed += 1;

        let label = format!("{}  score {:.2}  p {:.2}", verdict.as_str().to_uppercase(), s.normalized_p, s.current_p);
        let annotated = annotate(&roi_img, &detections, cfg.detector.visualization_threshold, Some(&label));
        let jpeg = Arc::new(encode_jpeg(&annotated, 85));
        let now = self.now();
        let inference_ms = self.detector.last_inference_ms();

        {
            let mut v = lock(&self.view);
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
        }

        core.recorder.record(&FrameRecord {
            now,
            frame_num: s.current_frame_num,
            progress: status.progress,
            confidences: &confs,
            ewm: s.ewm_mean,
            baseline: s.rolling_mean_long,
            short_mean: s.rolling_mean_short,
            score: s.normalized_p,
            verdict: verdict.as_str(),
            inference_ms,
            annotated_jpeg: Some(&jpeg),
        });

        tracing::debug!(
            "Analyze: {} dets p={:.3} ewm={:.3} base={:.3} -> {}",
            detections.len(),
            s.current_p,
            s.ewm_mean,
            s.rolling_mean_long,
            verdict.as_str()
        );

        if core.job.muted || core.job.action_taken.is_some() || core.incident.is_some() || now < core.job.rearm_at {
            return;
        }
        match verdict {
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
        let job = core.job.job_name.clone().unwrap_or_else(|| core.job.job_id.map(|j| j.to_string()).unwrap_or("None".into()));
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
    fn open_incident(&self, core: &mut Core, status: &PrinterStatus, jpeg: Arc<Vec<u8>>, annotated: &RgbImage, now: f64) {
        core.counters.failures += 1;
        let job_id = status.job_id.or(core.job.job_id);
        if self.cfg.save_failure_frames {
            let dir = self.state_dir.join("failures");
            let name = format!("{}_job{}.jpg", now as i64, job_id.map(|j| j.to_string()).unwrap_or("None".into()));
            let bytes = encode_jpeg(annotated, 95);
            if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(name), bytes)) {
                tracing::warn!("Monitor: could not save failure frame: {e}");
            }
        }
        let (policy, schedule) = core.policies.resolve(now);
        let inc = Incident::new(policy.clone(), schedule.clone(), job_id, now, core.decider.state.normalized_p, jpeg.to_vec());
        tracing::warn!(
            "Monitor: failure on job {} (score {:.2}) - incident {}, policy '{}'{}",
            job_id.map(|j| j.to_string()).unwrap_or("None".into()),
            inc.score,
            inc.id,
            policy.name,
            schedule.as_ref().map(|s| format!(" (schedule '{s}')")).unwrap_or_default()
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
            self.close_incident(core, &format!("printer is {} (handled at the printer)", status.state), false);
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
        if inc.steps_done() && inc.acted.as_deref() != Some("paused") {
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
        let (mut taken, mut err) = (None, None);
        match step.action.as_deref() {
            Some("pause") if inc.acted.is_none() && status.state == "PRINTING" => {
                (taken, err) = self.do_action(core, "pause", inc.job_id)
            }
            Some("stop") if inc.acted.as_deref() != Some("stopped") && is_active(&status.state) => {
                (taken, err) = self.do_action(core, "stop", inc.job_id)
            }
            _ => {}
        }
        if let Some(t) = &taken {
            inc.acted = Some(t.clone());
            status.state = if t == "paused" { "PAUSED".into() } else { "STOPPED".into() };
            if auto {
                core.counters.auto_actions += 1;
            }
        }
        if let Some(err) = err {
            let action = step.action.clone().unwrap_or_default();
            inc.action_errors += 1;
            if inc.action_errors == 1 {
                // Never let a failed pause/stop go quietly: every channel, max priority. Once per
                // incident; retries follow every poll and the normal alert goes out if one succeeds.
                let name =
                    core.job.job_name.clone().unwrap_or_else(|| inc.job_id.map(|j| j.to_string()).unwrap_or("None".into()));
                let mut ev = self.event(
                    core,
                    "failure",
                    &format!("{}: {action} FAILED", self.cfg.printer.name),
                    &format!(
                        "Spaghetti detected on '{name}' but the {action} command failed: {err}. Retrying every few seconds. Check the printer NOW."
                    ),
                    Some(inc.jpeg.clone()),
                    None,
                );
                ev.priority = 5;
                ev.incident_id = Some(inc.id.clone());
                ev.policy = Some(inc.policy.name.clone());
                self.notifier.send(ev, None);
            } else {
                tracing::warn!("Monitor: {action} retry {} for incident {} failed: {err}", inc.action_errors, inc.id);
            }
            return false;
        }
        self.notify_step(core, inc, step, now, taken.as_deref());
        true
    }

    fn do_action(&self, core: &mut Core, action: &str, job_id: Option<i64>) -> (Option<String>, Option<String>) {
        let Some(job_id) = job_id else { return (None, Some("no job id".into())) };
        let res = if action == "pause" { self.printer.pause(job_id) } else { self.printer.stop(job_id) };
        match res {
            Ok(()) => {
                let taken = if action == "pause" {
                    core.counters.pauses += 1;
                    "paused"
                } else {
                    core.counters.stops += 1;
                    "stopped"
                };
                core.job.action_taken = Some(taken.into());
                (Some(taken.into()), None)
            }
            Err(e) => {
                tracing::error!("Monitor: FAILED to {action} job {job_id}: {e}");
                (None, Some(e.to_string()))
            }
        }
    }

    fn notify_step(&self, core: &mut Core, inc: &Incident, step: &Step, now: f64, taken: Option<&str>) {
        if step.notify.as_ref().is_some_and(|n| n.is_empty()) {
            return;
        }
        let f = inc.template_fields(now, &self.cfg.printer.name, core.job.job_name.as_deref(), core.decider.state.normalized_p);
        let na = inc.next_action();
        let fill = |t: &str| format_template(t, &f).unwrap_or_else(|_| t.to_string());
        // Python treated empty strings as unset (`if step.title:`)
        let title = if let Some(t) = step.title.as_ref().filter(|t| !t.is_empty()) {
            fill(t)
        } else if let Some(t) = taken {
            format!("{}: print {}", f["printer"], t.to_uppercase())
        } else if na.is_some() {
            let verb = if f["next_action"] == "pause" { "pausing" } else { "stopping" };
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
                    m += &format!(" Next: {} in {} unless you respond.", f["next_action"], f["next_action_in"]);
                }
                m
            } else if na.is_some() {
                format!("{head} Tap Keep printing if it's a false alarm. No response = {}.", f["next_action"])
            } else {
                format!("{head} Printer still running (policy '{}' does not act).", inc.policy.name)
            };
            m += &format!(" [{}{}]", inc.policy.name, inc.schedule.as_ref().map(|s| format!(" / {s}")).unwrap_or_default());
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
            return format!("ignored: '{cmd}' not applicable (incident is {})", inc.acted.clone().unwrap_or("waiting".into()));
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
                if core.incident.as_ref().is_some_and(|i| i.acted.as_deref() == Some("paused")) {
                    self.printer.resume(need_job()?)?;
                    core.job.action_taken = None;
                }
                core.counters.vetoes += 1;
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
                let last_state = core.last_status.as_ref().map(|s| s.state.clone()).unwrap_or("PRINTING".into());
                let mut status = PrinterStatus::new(&last_state, inc.job_id);
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.run_step(core, &mut inc, &step, &mut status, now, false)
                }))
                .unwrap_or(false); // incident kept either way
                let action = step.action.clone().unwrap_or_default();
                if !ok {
                    inc.next_idx = idx;
                    inc.retry_idx = Some(idx); // the loop retries it on the next poll, not at its scheduled time
                    core.incident = Some(inc);
                    return Ok(format!("failed: {action} command failed; retrying"));
                }
                let acted = inc.acted.clone();
                core.incident = Some(inc);
                self.maybe_finish(core);
                Ok(acted.unwrap_or_else(|| format!("{action} skipped (printer is {})", status.state)))
            }
            "stop" => {
                let (taken, err) = self.do_action(core, "stop", inc_job);
                if let Some(e) = err {
                    return Err(PrusaLinkError(e));
                }
                if let Some(i) = core.incident.as_mut() {
                    i.acted = taken;
                }
                self.close_incident(core, "stopped by user", false);
                Ok("stopped".into())
            }
            "resume" | "mute" => {
                self.printer.resume(need_job()?)?;
                self.after_resume(core);
                if cmd == "mute" {
                    self.set_muted_locked(core, true);
                    return Ok("resumed (alerts muted for this print)".into());
                }
                Ok("resumed".into())
            }
            other => Ok(format!("ignored: unknown command '{other}'")),
        }
    }

    fn refresh_incident_snapshot(&self, core: &Core, now: f64) {
        let mut v = lock(&self.view);
        v.snap.incident = core.incident.as_ref().map(|i| i.public(now));
        v.incident = core.incident.clone();
    }

    fn refresh_policy_snapshot(&self, core: &Core, now: f64) {
        let (pol, sched) = core.policies.resolve(now);
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
    pub fn resume(&self, mute: bool) -> Result<String, PrusaLinkError> {
        let mut core = lock(&self.core);
        let job_id = core.job.job_id.ok_or_else(|| PrusaLinkError("no active job".into()))?;
        self.printer.resume(job_id)?;
        if core.job.action_taken.as_deref() == Some("paused") {
            self.after_resume(&mut core);
        }
        let out = if mute {
            self.set_muted_locked(&mut core, true);
            "resumed (alerts muted for this print)"
        } else {
            "resumed"
        };
        self.sync_counters(&core);
        Ok(out.into())
    }

    pub fn stop_print(&self) -> Result<String, PrusaLinkError> {
        let job_id = lock(&self.core).job.job_id.ok_or_else(|| PrusaLinkError("no active job".into()))?;
        self.printer.stop(job_id)?;
        Ok("stopped".into())
    }

    pub fn pause(&self) -> Result<String, PrusaLinkError> {
        let job_id = lock(&self.core).job.job_id.ok_or_else(|| PrusaLinkError("no active job".into()))?;
        self.printer.pause(job_id)?;
        Ok("paused".into())
    }

    pub fn set_muted(&self, muted: bool) {
        let mut core = lock(&self.core);
        self.set_muted_locked(&mut core, muted);
    }

    fn set_muted_locked(&self, core: &mut Core, muted: bool) {
        core.job.muted = muted;
        tracing::info!(
            "Monitor: alerts {} for job {}",
            if muted { "muted" } else { "unmuted" },
            core.job.job_id.map(|j| j.to_string()).unwrap_or("None".into())
        );
        lock(&self.view).snap.job = core.job.clone();
    }

    /// Run the model on the current frame without touching decision state (ROI tuning).
    pub fn test_detection(&self) -> Option<(Vec<Detection>, Vec<u8>)> {
        let frame = self.grabber.latest()?;
        let img = crop_roi(&frame.image, self.cfg.camera.roi.as_deref());
        let dets = self.detector.detect(&img, self.cfg.detector.threshold, self.cfg.detector.nms);
        let total: f64 = dets.iter().map(|d| d.confidence).fold(0.0, |a, b| a + b);
        let annotated =
            annotate(&img, &dets, self.cfg.detector.visualization_threshold, Some(&format!("TEST  sum p {total:.2}")));
        Some((dets, encode_jpeg(&annotated, 85)))
    }

    // ------------------------------------------------------------------ read models
    pub fn snapshot(&self) -> Snapshot {
        let v = lock(&self.view);
        let mut snap = v.snap.clone();
        if let Some(inc) = &v.incident {
            // keep the countdown live between ticks
            snap.incident = Some(inc.public(self.now()));
        }
        snap
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
        Some(encode_jpeg(&img, 80))
    }

    pub fn weak(&self) -> Weak<Monitor> {
        self.me.clone()
    }
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
