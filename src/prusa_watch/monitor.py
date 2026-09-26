"""Main watch loop: printer state -> frame -> detection -> decision -> action."""

from __future__ import annotations

import logging
import threading
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path

import cv2
import numpy as np

from .camera import FrameGrabber, crop_roi
from .config import Config
from .decision import FailureDecider, Verdict
from .detector import Detection, SpaghettiDetector, annotate
from .notify import Event, Notifier
from .prusalink import PrinterStatus, PrusaLink, PrusaLinkError

log = logging.getLogger(__name__)


@dataclass
class JobContext:
    job_id: int | None = None
    job_name: str | None = None
    muted: bool = False  # user said "not a failure" -> no more actions this job
    action_taken: str | None = None  # "paused" / "stopped" by us
    last_warning_ts: float = 0.0
    warnings: int = 0
    rearm_at: float = 0.0  # actions disarmed until this time (after a resume)


@dataclass
class Counters:
    frames_analyzed: int = 0
    warnings: int = 0
    failures: int = 0
    pauses: int = 0
    stops: int = 0
    printer_errors: int = 0


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
        self.grabber = grabber or FrameGrabber(cfg.camera.url, cfg.camera.transport, cfg.camera.reconnect_backoff_s)
        self.detector = detector or SpaghettiDetector(cfg.detector.model_path, use_gpu=cfg.detector.use_gpu)
        self.notifier = notifier or Notifier(cfg.notify, public_url=cfg.web.public_url, control_token=cfg.web.token)
        self.decider = FailureDecider(cfg.decision, self.state_dir / "prediction_state.json")

        self.job = JobContext()
        self.counters = Counters()
        self.history: deque[HistoryPoint] = deque(maxlen=720)  # 2 h at 10 s
        self._lock = threading.RLock()
        self._snap = Snapshot()
        self._latest_annotated: bytes | None = None
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._last_status: PrinterStatus | None = None
        self._last_detect_ts = 0.0
        self._camera_down_notified = False
        self._printer_error_logged = False

    # ------------------------------------------------------------------ lifecycle
    def start(self) -> None:
        self.grabber.start()
        self._thread = threading.Thread(target=self._run, name="monitor", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self.grabber.stop()
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
        status = self._poll_printer()
        if status is None:
            return
        self._handle_job_transitions(status)
        self._last_status = status

        frame = self.grabber.latest()
        now = self.clock()
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
        active = status.state in ("PRINTING", "PAUSED", "ATTENTION")

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
            self.job = JobContext(job_id=status.job_id, job_name=name)
            self.decider.reset_for_new_print()
            self.history.clear()

        # User resumed a print we paused -> give it a fresh grace period
        if (
            prev is not None
            and prev.state == "PAUSED"
            and status.state == "PRINTING"
            and self.job.action_taken == "paused"
        ):
            grace = self.cfg.decision.resume_grace_s
            log.info("Monitor: job %s resumed after AI pause - actions re-arm in %.0fs", self.job.job_id, grace)
            self.job.action_taken = None
            self.job.rearm_at = self.clock() + grace
            self.job.last_warning_ts = 0.0

        # Print ended
        if not active and self.job.job_id is not None and prev is not None and prev.state in ("PRINTING", "PAUSED", "ATTENTION"):
            log.info("Monitor: job %s ended with state %s", self.job.job_id, status.state)
            self.decider.reset_for_new_print()

        with self._lock:
            self._snap.job = JobContext(**self.job.__dict__)

    def _handle_camera_health(self, camera_ok: bool) -> None:
        if camera_ok:
            if self._camera_down_notified:
                self._camera_down_notified = False
                if self.cfg.notify.notify_camera_down:
                    self.notifier.send(self._event("camera_up", "Camera back online", "AI monitoring resumed."))
            return
        if not self._camera_down_notified:
            self._camera_down_notified = True
            log.warning("Monitor: printing but camera frame is stale/missing - AI monitoring blind")
            if self.cfg.notify.notify_camera_down:
                self.notifier.send(
                    self._event(
                        "camera_down",
                        f"{self.cfg.printer.name}: camera offline",
                        "Printer is printing but no fresh frames from the Buddy3D camera. AI monitoring is blind. "
                        "Check that RTSP is enabled in the Prusa app.",
                    )
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

        if self.job.muted or self.job.action_taken or now < self.job.rearm_at:
            return
        if verdict == Verdict.FAILURE:
            self._on_failure(status, jpeg, annotated)
        elif verdict == Verdict.WARNING:
            self._on_warning(jpeg)

    # ------------------------------------------------------------------ actions
    def _on_warning(self, jpeg: bytes) -> None:
        now = self.clock()
        if now - self.job.last_warning_ts < self.cfg.notify.cooldown_s:
            return
        self.job.last_warning_ts = now
        self.job.warnings += 1
        self.counters.warnings += 1
        log.warning("Monitor: possible failure on job %s (score %.2f)", self.job.job_id, self.decider.state.normalized_p)
        self.notifier.send(
            self._event(
                "warning",
                f"{self.cfg.printer.name}: possible print failure",
                f"Spaghetti detector is seeing something on '{self.job.job_name or self.job.job_id}'. "
                f"Not paused yet (score {self.decider.state.normalized_p:.2f}).",
                jpeg,
            )
        )

    def _on_failure(self, status: PrinterStatus, jpeg: bytes, annotated: np.ndarray) -> None:
        self.counters.failures += 1
        action = self.cfg.decision.action
        job_id = status.job_id if status.job_id is not None else self.job.job_id
        taken = None
        err = None
        if action in ("pause", "stop") and job_id is not None:
            try:
                if action == "pause":
                    self.printer.pause(job_id)
                    taken = "paused"
                    self.counters.pauses += 1
                else:
                    self.printer.stop(job_id)
                    taken = "stopped"
                    self.counters.stops += 1
            except PrusaLinkError as exc:
                err = str(exc)
                log.error("Monitor: FAILED to %s job %s: %s", action, job_id, exc)
        # Mark handled even for notify-only so we don't spam every 10 s.
        self.job.action_taken = taken or "notified"

        if self.cfg.save_failure_frames:
            out = self.state_dir / "failures"
            out.mkdir(parents=True, exist_ok=True)
            cv2.imwrite(str(out / f"{int(self.clock())}_job{job_id}.jpg"), annotated)

        if taken:
            msg = f"Print '{self.job.job_name or job_id}' was {taken.upper()} - spaghetti detected (score {self.decider.state.normalized_p:.2f})."
        elif err:
            msg = f"Spaghetti detected on '{self.job.job_name or job_id}' but the {action} command FAILED: {err}. Check the printer NOW."
        else:
            msg = f"Spaghetti detected on '{self.job.job_name or job_id}' (notify-only mode, printer still running)."
        log.warning("Monitor: %s", msg)
        self.notifier.send(self._event("failure", f"{self.cfg.printer.name}: print failure detected", msg, jpeg, taken))

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
        self.printer.resume(self.job.job_id)
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
