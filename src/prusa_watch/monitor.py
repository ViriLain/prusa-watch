"""Main watch loop: printer state -> frame -> detection -> decision -> escalation."""

from __future__ import annotations

import logging
import secrets
import threading
import time
from collections import deque
from dataclasses import dataclass, field, replace
from pathlib import Path

import cv2
import numpy as np

from .camera import FrameGrabber, crop_roi
from .config import Config, EventNotifyConfig
from .decision import FailureDecider, Verdict
from .detector import Detection, SpaghettiDetector, annotate
from .escalation import Incident, PolicyResolver, Step, fmt_duration, parse_escalation
from .notify import Event, Notifier
from .prusalink import PrinterStatus, PrusaLink, PrusaLinkError

log = logging.getLogger(__name__)

ACTIVE = ("PRINTING", "PAUSED", "ATTENTION")


@dataclass
class JobContext:
    job_id: int | None = None
    job_name: str | None = None
    muted: bool = False  # user said "not a failure" -> no more incidents this job
    action_taken: str | None = None  # "paused" / "stopped" by us
    last_warning_ts: float = 0.0
    warnings: int = 0
    rearm_at: float = 0.0  # no new incidents until this time (after resume / keep printing)
    printer_handled: bool = False  # you paused at the printer during an incident -> grace on resume


@dataclass
class Counters:
    frames_analyzed: int = 0
    warnings: int = 0
    failures: int = 0  # incidents opened
    pauses: int = 0
    stops: int = 0
    printer_errors: int = 0
    vetoes: int = 0  # "Keep printing"
    auto_actions: int = 0  # actions taken because a step's time came with no answer


@dataclass
class HistoryPoint:
    ts: float
    p: float
    score: float
    verdict: str


@dataclass
class Snapshot:
    printer_state: str = "UNKNOWN"
    printer_reachable: bool = False
    printer_error: str | None = None
    job: JobContext = field(default_factory=JobContext)
    verdict: str = Verdict.OK.value
    score: float = 0.0
    current_p: float = 0.0
    ewm_mean: float = 0.0
    baseline: float = 0.0
    frame_num: int = 0
    grace_frames_left: int = 0
    camera_connected: bool = False
    frame_age_s: float | None = None
    inference_ms: float = 0.0
    last_detections: list = field(default_factory=list)
    last_analysis_ts: float | None = None
    progress: float | None = None
    incident: dict | None = None
    policy: dict = field(default_factory=dict)


def _jpeg(img: np.ndarray, quality: int = 85) -> bytes:
    ok, buf = cv2.imencode(".jpg", img, [cv2.IMWRITE_JPEG_QUALITY, quality])
    return buf.tobytes() if ok else b""


class Monitor:
    def __init__(
        self,
        cfg: Config,
        printer: PrusaLink | None = None,
        grabber: FrameGrabber | None = None,
        detector: SpaghettiDetector | None = None,
        notifier: Notifier | None = None,
        clock=time.time,
    ):
        self.cfg = cfg
        self.clock = clock
        self.state_dir = Path(cfg.state_dir)
        self.state_dir.mkdir(parents=True, exist_ok=True)

        self.printer = printer or PrusaLink(
            cfg.printer.host,
            cfg.printer.password,
            username=cfg.printer.username,
            auth=cfg.printer.auth,
            scheme=cfg.printer.scheme,
            timeout_s=cfg.printer.timeout_s,
        )
        c = cfg.camera
        self.grabber = grabber or FrameGrabber(c.url, c.transport, c.reconnect_backoff_s, c.open_timeout_s, c.read_timeout_s)
        self.detector = detector or SpaghettiDetector(cfg.detector.model_path, use_gpu=cfg.detector.use_gpu)
        self.notifier = notifier or Notifier(cfg.notify, public_url=cfg.web.public_url, control_token=cfg.web.token)
        self.decider = FailureDecider(cfg.decision, self.state_dir / "prediction_state.json")
        self.reload_escalation()

        self.incident: Incident | None = None
        self.replies = None  # NtfyReplyListener, started in start()
        self.job = JobContext()
        self.counters = Counters()
        maxlen = max(10, int(cfg.web.history_s / max(cfg.detector.interval_s, 1.0)) + 1)
        self.history: deque[HistoryPoint] = deque(maxlen=maxlen)
        self._lock = threading.RLock()  # snapshot/read-model lock
        self._ctl = threading.RLock()  # serializes the loop with replies from ntfy / web
        self._snap = Snapshot()
        self._latest_annotated: bytes | None = None
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._last_status: PrinterStatus | None = None
        self._last_detect_ts = 0.0
        self._camera_down_notified = False
        self._printer_error_logged = False
        self._last_routed: dict[str, float] = {}

    def reload_escalation(self) -> None:
        """(Re)parse cfg.escalation + cfg.timezone."""
        self.escalation = parse_escalation(self.cfg.escalation)
        self.policies = PolicyResolver(self.escalation, self.cfg.timezone)

    # ------------------------------------------------------------------ lifecycle
    def start(self) -> None:
        self.grabber.start()
        ntfy = self.cfg.notify.ntfy
        if ntfy.reply_topic and self.replies is None:
            from .replies import NtfyReplyListener

            self.replies = NtfyReplyListener(ntfy, self.handle_reply)
            self.replies.start()
        self._thread = threading.Thread(target=self._run, name="monitor", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self.grabber.stop()
        if self.replies:
            self.replies.stop()
        if self._thread:
            self._thread.join(timeout=10)
        self.decider.save()

    def _run(self) -> None:
        log.info("Monitor: watching %s (%s)", self.cfg.printer.name, self.cfg.printer.host)
        while not self._stop.is_set():
            try:
                self.tick()
            except Exception:
                log.exception("Monitor: unexpected error in tick")
            self._stop.wait(self.cfg.printer.poll_interval_s)

    # ------------------------------------------------------------------ core loop
    def tick(self) -> None:
        """One iteration. Public so tests can drive it deterministically."""
        with self._ctl:
            self._tick()

    def _tick(self) -> None:
        status = self._poll_printer()
        if status is None:
            return
        self._handle_job_transitions(status)
        # Keep what the printer actually reported. _run_step marks `status` PAUSED/STOPPED
        # for the rest of this tick; if that leaked into _last_status, a pause still in
        # flight (next poll reads PRINTING) would look like a resume at the printer.
        self._last_status = replace(status)

        now = self.clock()
        self._process_incident(status, now)
        self._refresh_policy_snapshot(now)

        frame = self.grabber.latest()
        frame_age = (now - frame.ts) if frame else None
        with self._lock:
            self._snap.camera_connected = self.grabber.connected
            self._snap.frame_age_s = frame_age

        if status.state != "PRINTING":
            return

        camera_ok = frame is not None and frame_age is not None and frame_age <= self.cfg.camera.stale_after_s
        self._handle_camera_health(camera_ok)
        if not camera_ok:
            return

        if now - self._last_detect_ts < self.cfg.detector.interval_s:
            return
        self._last_detect_ts = now
        self._analyze(frame.image, status)

    def _poll_printer(self) -> PrinterStatus | None:
        try:
            status = self.printer.status()
        except PrusaLinkError as exc:
            self.counters.printer_errors += 1
            if not self._printer_error_logged:
                log.error("Monitor: printer unreachable: %s", exc)
                self._printer_error_logged = True
            with self._lock:
                self._snap.printer_reachable = False
                self._snap.printer_error = str(exc)
            return None
        if self._printer_error_logged:
            log.info("Monitor: printer reachable again")
        self._printer_error_logged = False
        with self._lock:
            self._snap.printer_reachable = True
            self._snap.printer_error = None
            self._snap.printer_state = status.state
            self._snap.progress = status.progress
        return status

    def _handle_job_transitions(self, status: PrinterStatus) -> None:
        prev = self._last_status
        active = status.state in ACTIVE

        if active and status.job_id is None:
            # Some firmware builds omit job.id from /status; fall back to /job.
            try:
                j = self.printer.job()
                status.job_id = j.get("id") if j else None
            except PrusaLinkError:
                pass

        # New job started (id changed while printing)
        if active and status.job_id is not None and status.job_id != self.job.job_id:
            name = self.printer.job_name()
            log.info("Monitor: new job %s (%s) - resetting per-print state", status.job_id, name)
            if self.incident:
                self._close_incident("a new job started")
            self.job = JobContext(job_id=status.job_id, job_name=name)
            self.decider.reset_for_new_print()
            self.history.clear()

        # Resumed (anywhere: printer knob, Prusa app, dashboard) after we paused it
        if prev is not None and prev.state == "PAUSED" and status.state == "PRINTING" and self.job.action_taken == "paused":
            self._after_resume()
        # Back to printing after you paused at the printer during an incident: same grace as our resume,
        # so a mess you're still clearing doesn't open a new incident the moment it resumes.
        elif prev is not None and prev.state in ACTIVE and prev.state != "PRINTING" and status.state == "PRINTING" and self.job.printer_handled:
            grace = self.cfg.decision.resume_grace_s
            log.info("Monitor: job %s resumed after a pause at the printer - incidents re-arm in %.0fs", self.job.job_id, grace)
            self.job.printer_handled = False
            self.job.rearm_at = max(self.job.rearm_at, self.clock() + grace)

        # Print ended
        if not active and self.job.job_id is not None and prev is not None and prev.state in ACTIVE:
            log.info("Monitor: job %s ended with state %s", self.job.job_id, status.state)
            if self.incident:
                self._close_incident(f"job ended ({status.state})")
            self.decider.reset_for_new_print()

        with self._lock:
            self._snap.job = JobContext(**self.job.__dict__)

    def _after_resume(self) -> None:
        grace = self.cfg.decision.resume_grace_s
        log.info("Monitor: job %s resumed after AI pause - incidents re-arm in %.0fs", self.job.job_id, grace)
        self.job.action_taken = None
        self.job.rearm_at = self.clock() + grace
        self.job.last_warning_ts = 0.0
        if self.incident:
            self._close_incident("resumed")

    def _handle_camera_health(self, camera_ok: bool) -> None:
        if camera_ok:
            if self._camera_down_notified:
                self._camera_down_notified = False
                self._notify_routed(self.cfg.notify.camera, "camera_up", f"{self.cfg.printer.name}: camera back online", "AI monitoring resumed.")
            return
        if not self._camera_down_notified:
            self._camera_down_notified = True
            log.warning("Monitor: printing but camera frame is stale/missing - AI monitoring blind")
            self._notify_routed(
                self.cfg.notify.camera,
                "camera_down",
                f"{self.cfg.printer.name}: camera offline",
                "Printer is printing but no fresh frames from the Buddy3D camera. AI monitoring is blind. "
                "Check that RTSP is enabled in the Prusa app.",
            )

    def _analyze(self, image: np.ndarray, status: PrinterStatus) -> None:
        cfg = self.cfg
        roi_img = crop_roi(image, cfg.camera.roi)
        detections = self.detector.detect(roi_img, thresh=cfg.detector.threshold, nms=cfg.detector.nms)
        verdict = self.decider.update([d.confidence for d in detections])
        s = self.decider.state
        self.counters.frames_analyzed += 1

        label = f"{verdict.value.upper()}  score {s.normalized_p:.2f}  p {s.current_p:.2f}"
        annotated = annotate(roi_img, detections, cfg.detector.visualization_threshold, label)
        jpeg = _jpeg(annotated)
        now = self.clock()

        with self._lock:
            self._latest_annotated = jpeg
            self.history.append(HistoryPoint(now, s.current_p, s.normalized_p, verdict.value))
            self._snap.verdict = verdict.value
            self._snap.score = s.normalized_p
            self._snap.current_p = s.current_p
            self._snap.ewm_mean = s.ewm_mean
            self._snap.baseline = s.rolling_mean_long
            self._snap.frame_num = s.current_frame_num
            self._snap.grace_frames_left = max(0, cfg.decision.init_safe_frame_num - s.current_frame_num)
            self._snap.inference_ms = self.detector.last_inference_ms
            self._snap.last_detections = [d.as_list() for d in detections if d.confidence >= cfg.detector.visualization_threshold]
            self._snap.last_analysis_ts = now

        log.debug("Analyze: %d dets p=%.3f ewm=%.3f base=%.3f -> %s", len(detections), s.current_p, s.ewm_mean, s.rolling_mean_long, verdict.value)

        if self.job.muted or self.job.action_taken or self.incident or now < self.job.rearm_at:
            return
        if verdict == Verdict.FAILURE:
            self._open_incident(status, jpeg, annotated, now)
        elif verdict == Verdict.WARNING:
            self._on_warning(jpeg)

    # ------------------------------------------------------------------ warnings / routed notifications
    def _notify_routed(self, route: EventNotifyConfig, kind: str, title: str, message: str, jpeg: bytes | None = None) -> None:
        if not route.enabled:
            log.info("Notify (%s disabled): %s", kind, title)
            return
        if kind != "warning":  # warnings use a per-print cooldown in _on_warning
            now = self.clock()
            if now - self._last_routed.get(kind, float("-inf")) < route.cooldown_s:
                return
            self._last_routed[kind] = now
        ev = self._event(kind, title, message, jpeg)
        ev.priority = route.priority
        self.notifier.send(ev, channels=route.channels)

    def _on_warning(self, jpeg: bytes) -> None:
        now = self.clock()
        route = self.cfg.notify.warning
        if now - self.job.last_warning_ts < route.cooldown_s:
            return
        self.job.last_warning_ts = now
        self.job.warnings += 1
        self.counters.warnings += 1
        log.warning("Monitor: possible failure on job %s (score %.2f)", self.job.job_id, self.decider.state.normalized_p)
        self._notify_routed(
            route,
            "warning",
            f"{self.cfg.printer.name}: possible print failure",
            f"Spaghetti detector is seeing something on '{self.job.job_name or self.job.job_id}'. "
            f"Not acting yet (score {self.decider.state.normalized_p:.2f}).",
            jpeg,
        )

    # ------------------------------------------------------------------ escalation
    def _open_incident(self, status: PrinterStatus, jpeg: bytes, annotated: np.ndarray, now: float) -> None:
        self.counters.failures += 1
        job_id = status.job_id if status.job_id is not None else self.job.job_id
        if self.cfg.save_failure_frames:
            out = self.state_dir / "failures"
            out.mkdir(parents=True, exist_ok=True)
            cv2.imwrite(str(out / f"{int(now)}_job{job_id}.jpg"), annotated)
        policy, schedule = self.policies.resolve(now)
        inc = Incident(
            policy=policy,
            schedule=schedule,
            job_id=job_id,
            started_ts=now,
            score=self.decider.state.normalized_p,
            jpeg=jpeg,
        )
        self.incident = inc
        log.warning(
            "Monitor: failure on job %s (score %.2f) - incident %s, policy '%s'%s",
            job_id, inc.score, inc.id, policy.name, f" (schedule '{schedule}')" if schedule else "",
        )
        self._run_due_steps(inc, status, now)
        self._refresh_incident_snapshot(now)

    def _process_incident(self, status: PrinterStatus, now: float) -> None:
        inc = self.incident
        if inc is None:
            return
        if status.job_id is not None and inc.job_id is not None and status.job_id != inc.job_id:
            self._close_incident("job changed")
        elif status.state not in ACTIVE:
            self._close_incident(f"printer is {status.state}")
        elif status.state != "PRINTING" and inc.acted is None:
            self.job.printer_handled = True
            self._close_incident(f"printer is {status.state} (handled at the printer)")
        else:
            self._run_due_steps(inc, status, now, auto=True)
        self._refresh_incident_snapshot(now)

    def _run_due_steps(self, inc: Incident, status: PrinterStatus, now: float, auto: bool = False) -> None:
        for idx, step in inc.due(now):
            inc.next_idx = idx + 1
            ok = self._run_step(inc, step, status, now, auto=auto and step.at > 0)
            if self.incident is not inc:
                return
            if not ok:
                # The pause/stop didn't go through: keep the incident open and retry this
                # step on the next poll instead of moving on (and snoozing) as if it had.
                inc.next_idx = idx
                return
        self._maybe_finish(inc)

    def _maybe_finish(self, inc: Incident) -> None:
        if self.incident is inc and inc.steps_done and inc.acted != "paused":
            # Paused incidents stay open so Resume / False alarm buttons keep working.
            self._close_incident("all steps done", cooldown=inc.acted is None)

    def _run_step(self, inc: Incident, step: Step, status: PrinterStatus, now: float, auto: bool = False) -> bool:
        """Run one step. Returns False if its pause/stop failed (caller retries)."""
        taken, err = None, None
        if step.action == "pause" and inc.acted is None and status.state == "PRINTING":
            taken, err = self._do("pause", inc.job_id)
        elif step.action == "stop" and inc.acted != "stopped" and status.state in ACTIVE:
            taken, err = self._do("stop", inc.job_id)
        if taken:
            inc.acted = taken
            status.state = "PAUSED" if taken == "paused" else "STOPPED"
            if auto:
                self.counters.auto_actions += 1
        if err:
            inc.action_errors += 1
            if inc.action_errors == 1:
                # Never let a failed pause/stop go quietly: every channel, max priority. Once per
                # incident; retries follow every poll and the normal alert goes out if one succeeds.
                name = self.job.job_name or inc.job_id
                ev = self._event(
                    "failure",
                    f"{self.cfg.printer.name}: {step.action} FAILED",
                    f"Spaghetti detected on '{name}' but the {step.action} command failed: {err}. "
                    "Retrying every few seconds. Check the printer NOW.",
                    inc.jpeg,
                )
                ev.priority, ev.incident_id, ev.policy = 5, inc.id, inc.policy.name
                self.notifier.send(ev, channels=None)
            else:
                log.warning("Monitor: %s retry %d for incident %s failed: %s", step.action, inc.action_errors, inc.id, err)
            return False
        self._notify_step(inc, step, now, taken)
        return True

    def _do(self, action: str, job_id: int | None) -> tuple[str | None, str | None]:
        if job_id is None:
            return None, "no job id"
        try:
            if action == "pause":
                self.printer.pause(job_id)
                self.counters.pauses += 1
                self.job.action_taken = "paused"
                return "paused", None
            self.printer.stop(job_id)
            self.counters.stops += 1
            self.job.action_taken = "stopped"
            return "stopped", None
        except PrusaLinkError as exc:
            log.error("Monitor: FAILED to %s job %s: %s", action, job_id, exc)
            return None, str(exc)

    def _notify_step(self, inc: Incident, step: Step, now: float, taken: str | None) -> None:
        if step.notify == []:
            return
        f = inc.template_fields(now, self.cfg.printer.name, self.job.job_name, self.decider.state.normalized_p)
        na = inc.next_action()
        if step.title:
            title = step.title.format(**f)
        elif taken:
            title = f"{f['printer']}: print {taken.upper()}"
        elif na:
            verb = "pausing" if f["next_action"] == "pause" else "stopping"
            title = f"{f['printer']}: {verb} in {f['next_action_in']} unless you respond"
        else:
            title = f"{f['printer']}: print failure detected"
        if step.message:
            message = step.message.format(**f)
        else:
            head = f"Spaghetti detected on '{f['job']}' (score {f['score']})."
            if taken:
                message = f"{head} Print was {taken.upper()}."
                if na:
                    message += f" Next: {f['next_action']} in {f['next_action_in']} unless you respond."
            elif na:
                message = f"{head} Tap Keep printing if it's a false alarm. No response = {f['next_action']}."
            else:
                message = f"{head} Printer still running (policy '{inc.policy.name}' does not act)."
            message += f" [{inc.policy.name}{' / ' + inc.schedule if inc.schedule else ''}]"
        ev = self._event("failure" if taken else "incident", title, message, inc.jpeg if step.attach_image else None, taken)
        ev.priority = step.priority
        ev.buttons = step.buttons if step.buttons is not None else inc.default_buttons()
        ev.incident_id = inc.id
        ev.policy = inc.policy.name
        if na:
            ev.next_action, ev.next_action_ts = na[1].action, inc.started_ts + na[1].at
        self.notifier.send(ev, channels=step.notify)

    def _close_incident(self, reason: str, cooldown: bool = False) -> None:
        inc = self.incident
        if inc is None:
            return
        self.incident = None
        if cooldown:
            snooze = self.escalation.snooze_s
            if snooze > 0:
                self.job.rearm_at = self.clock() + snooze
            else:
                self.job.muted = True
        log.info("Monitor: incident %s closed - %s", inc.id, reason)
        self._refresh_incident_snapshot(self.clock())

    def handle_reply(self, cmd: str, incident_id: str) -> str:
        """Button / dashboard command for the current incident: veto|act|stop|resume|mute."""
        with self._ctl:
            now = self.clock()
            inc = self.incident
            if inc is None or not secrets.compare_digest(str(incident_id), inc.id):
                log.info("Monitor: ignoring '%s' for unknown/closed incident", cmd)
                return "ignored: no matching open incident"
            if cmd not in inc.commands():
                return f"ignored: '{cmd}' not applicable (incident is {inc.acted or 'waiting'})"
            try:
                result = self._apply_reply(inc, cmd, now)
            except PrusaLinkError as exc:
                return f"failed: {exc}"
            self._refresh_incident_snapshot(now)
            with self._lock:
                self._snap.job = JobContext(**self.job.__dict__)
            return result

    def _apply_reply(self, inc: Incident, cmd: str, now: float) -> str:
        snooze = self.escalation.snooze_s
        snooze_txt = f"no new alerts for {fmt_duration(snooze)}" if snooze > 0 else "alerts muted for the rest of this print"
        if cmd == "veto":
            if inc.acted == "paused":
                self.printer.resume(inc.job_id)
                self.job.action_taken = None
            self.counters.vetoes += 1
            self._close_incident("kept printing by user", cooldown=True)
            self._notify_routed(self.cfg.notify.info, "info", f"{self.cfg.printer.name}: keeping the print running", f"Got it: {snooze_txt}.")
            return f"vetoed ({snooze_txt})"
        if cmd == "act":
            idx, step = inc.next_action()  # commands() guarantees one exists
            inc.next_idx = idx + 1  # skip any reminders before it
            last = self._last_status
            status = PrinterStatus(last.state if last else "PRINTING", inc.job_id, None, None, None, None, {})
            if not self._run_step(inc, step, status, now):
                inc.next_idx = idx
                inc.retry_idx = idx  # the loop retries it on the next poll, not at its scheduled time
                return f"failed: {step.action} command failed; retrying"
            self._maybe_finish(inc)
            return inc.acted or f"{step.action} skipped (printer is {status.state})"
        if cmd == "stop":
            taken, err = self._do("stop", inc.job_id)
            if err:
                raise PrusaLinkError(err)
            inc.acted = taken
            self._close_incident("stopped by user")
            return "stopped"
        if cmd in ("resume", "mute"):
            self.printer.resume(inc.job_id)
            self._after_resume()
            if cmd == "mute":
                self.set_muted(True)
                return "resumed (alerts muted for this print)"
            return "resumed"
        return f"ignored: unknown command {cmd!r}"

    def _refresh_incident_snapshot(self, now: float) -> None:
        with self._lock:
            self._snap.incident = self.incident.public(now) if self.incident else None

    def _refresh_policy_snapshot(self, now: float) -> None:
        pol, sched = self.policies.resolve(now)
        with self._lock:
            self._snap.policy = {
                "policy": pol.name,
                "schedule": sched,
                "steps": [{"at": s.at, "action": s.action, "notify": s.notify, "priority": s.priority} for s in pol.steps],
                "replies_connected": bool(self.replies and self.replies.connected),
            }

    def _event(self, kind, title, message, jpeg=None, action_taken=None) -> Event:
        return Event(
            kind=kind,
            title=title,
            message=message,
            printer=self.cfg.printer.name,
            job_id=self.job.job_id,
            job_name=self.job.job_name,
            score=self.decider.state.normalized_p,
            action_taken=action_taken,
            image_jpeg=jpeg,
        )

    # ------------------------------------------------------------------ controls (web UI)
    def resume(self, mute: bool = False) -> str:
        if self.job.job_id is None:
            raise PrusaLinkError("no active job")
        with self._ctl:
            self.printer.resume(self.job.job_id)
            if self.job.action_taken == "paused":
                self._after_resume()
            if mute:
                self.set_muted(True)
                return "resumed (alerts muted for this print)"
        return "resumed"

    def stop_print(self) -> str:
        if self.job.job_id is None:
            raise PrusaLinkError("no active job")
        self.printer.stop(self.job.job_id)
        return "stopped"

    def pause(self) -> str:
        if self.job.job_id is None:
            raise PrusaLinkError("no active job")
        self.printer.pause(self.job.job_id)
        return "paused"

    def set_muted(self, muted: bool) -> None:
        self.job.muted = muted
        log.info("Monitor: alerts %s for job %s", "muted" if muted else "unmuted", self.job.job_id)
        with self._lock:
            self._snap.job = JobContext(**self.job.__dict__)

    def test_detection(self) -> tuple[list[Detection], bytes] | None:
        """Run the model on the current frame without touching decision state (ROI tuning)."""
        frame = self.grabber.latest()
        if frame is None:
            return None
        img = crop_roi(frame.image, self.cfg.camera.roi)
        dets = self.detector.detect(img, thresh=self.cfg.detector.threshold, nms=self.cfg.detector.nms)
        total = sum(d.confidence for d in dets)
        return dets, _jpeg(annotate(img, dets, self.cfg.detector.visualization_threshold, f"TEST  sum p {total:.2f}"))

    # ------------------------------------------------------------------ read models
    def snapshot(self) -> Snapshot:
        with self._lock:
            snap = Snapshot(**self._snap.__dict__)
            if self.incident:  # keep the countdown live between ticks
                snap.incident = self.incident.public(self.clock())
        return snap

    def history_points(self) -> list[HistoryPoint]:
        with self._lock:
            return list(self.history)

    def annotated_jpeg(self) -> bytes | None:
        with self._lock:
            return self._latest_annotated

    def raw_jpeg(self) -> bytes | None:
        frame = self.grabber.latest()
        if frame is None:
            return None
        img = frame.image
        if self.cfg.camera.roi:
            img = img.copy()
            h, w = img.shape[:2]
            r = self.cfg.camera.roi
            cv2.rectangle(img, (int(r[0] * w), int(r[1] * h)), (int(r[2] * w), int(r[3] * h)), (255, 200, 0), 2)
        return _jpeg(img, 80)
