"""Configuration loading.

YAML file with ``${ENV_VAR}`` expansion in string values, so secrets can live in
``.env`` / the environment instead of the config file.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field, fields, is_dataclass
from pathlib import Path
from typing import Any

import yaml


@dataclass
class PrinterConfig:
    name: str = "core-one"
    host: str = ""  # IP/hostname of the printer (PrusaLink)
    scheme: str = "http"
    username: str = "maker"  # Buddy firmware PrusaLink user is always "maker"
    password: str = ""  # Settings > Network > PrusaLink on the printer
    auth: str = "digest"  # "digest" (user/password) or "apikey" (X-Api-Key header)
    poll_interval_s: float = 5.0
    timeout_s: float = 5.0


@dataclass
class CameraConfig:
    url: str = ""  # rtsp://<camera-ip>/live  (or a file path / http URL for testing)
    transport: str = "tcp"  # RTSP transport; TCP avoids smeared frames on lossy Wi-Fi
    # Optional crop in normalized coords [x1, y1, x2, y2] (0..1). Cropping to the
    # bed improves detection a lot when the camera also sees the frame/door.
    roi: list[float] | None = None
    stale_after_s: float = 30.0  # frame older than this = camera considered down
    reconnect_backoff_s: float = 5.0


@dataclass
class DetectorConfig:
    model_path: str = "models/model-weights.onnx"
    threshold: float = 0.08  # per-box confidence floor (Obico default)
    nms: float = 0.45
    interval_s: float = 10.0  # Obico's hyperparameters are tuned for 10 s
    use_gpu: bool = False
    visualization_threshold: float = 0.2


@dataclass
class DecisionConfig:
    # User-facing knobs
    sensitivity: float = 1.0  # >1 = more trigger-happy, <1 = more conservative
    action: str = "pause"  # "pause" | "stop" | "notify"
    # After you resume a print that prusa-watch paused, actions stay disarmed this
    # long. Detection keeps running; if spaghetti is still there afterwards it
    # pauses again. Use "Mute this print" if it was a false positive.
    resume_grace_s: float = 120.0
    # Obico 1st-gen hyperparameters (backend/config/settings.py: FD_1ST_GEN_PARAMS)
    ewm_span: int = 12
    rolling_win_short: int = 310
    rolling_win_long: int = 7200
    threshold_low: float = 0.38
    threshold_high: float = 0.78
    init_safe_frame_num: int = 30  # 30 frames * 10 s = 5 min grace at print start
    rolling_mean_short_multiple: float = 3.8
    escalating_factor: float = 1.75
    # Fresh-install prior: seed the long-run baseline as if we'd already watched
    # this many clean (p=0) frames (360 = 1 h). Pure Obico starts the baseline at
    # zero history, so on a new install the baseline is the average of the
    # *current* print and spaghetti present from the first layer is absorbed as
    # "normal" and never pauses. With the prior, a fresh install behaves like an
    # established one (which is what Obico's thresholds were tuned on).
    # Only used when no saved state exists. Set 0 for exact Obico behavior.
    baseline_prior_frames: int = 360

    # --- Veto window ---------------------------------------------------------
    # On a failure verdict, wait this long before pausing/stopping and push an
    # alert with "Keep printing" / "Pause now" / "Cancel print" buttons. No
    # response = the action happens (fail-safe). 0 = act immediately.
    veto_window_s: float = 0.0
    # "Keep printing" disarms actions for this long (detection keeps running).
    # 0 = for the rest of the print.
    veto_snooze_s: float = 1800.0
    # Time-of-day overrides, first match wins. Each item:
    #   {name, start: "HH:MM", end: "HH:MM", days: [mon..sun] (optional),
    #    action: pause|stop|notify (optional), veto_window_s (optional),
    #    quiet: bool (optional; low-priority, silent notifications)}
    # Windows may cross midnight (start > end); `days` refers to the day the
    # window starts.
    schedules: list = field(default_factory=list)


@dataclass
class NtfyConfig:
    url: str = "https://ntfy.sh"
    topic: str = ""
    token: str = ""  # optional bearer token for protected/self-hosted topics
    priority_warning: int = 4
    priority_failure: int = 5
    # Topic prusa-watch SUBSCRIBES to for button replies (Keep printing / Pause
    # now / Cancel). Buttons post to ntfy, prusa-watch reads them over an
    # outbound connection -> works away from home with no port forwarding.
    # Use a long random name (or an ACL-protected topic on self-hosted ntfy).
    reply_topic: str = ""


@dataclass
class DiscordConfig:
    webhook_url: str = ""


@dataclass
class WebhookConfig:
    url: str = ""  # receives JSON POST (e.g. Home Assistant webhook trigger)


@dataclass
class NotifyConfig:
    ntfy: NtfyConfig = field(default_factory=NtfyConfig)
    discord: DiscordConfig = field(default_factory=DiscordConfig)
    webhook: WebhookConfig = field(default_factory=WebhookConfig)
    cooldown_s: float = 300.0  # min seconds between repeated warnings for one print
    notify_camera_down: bool = True


@dataclass
class WebConfig:
    enabled: bool = True
    host: str = "0.0.0.0"
    port: int = 8484
    public_url: str = ""  # used for links in notifications, e.g. http://192.168.1.10:8484
    # Optional shared secret for control endpoints (pause/resume/stop/mute).
    # Sent as ?token=... or X-Token header. Strongly recommended if the
    # dashboard is reachable from anything but your own LAN.
    token: str = ""


@dataclass
class Config:
    printer: PrinterConfig = field(default_factory=PrinterConfig)
    camera: CameraConfig = field(default_factory=CameraConfig)
    detector: DetectorConfig = field(default_factory=DetectorConfig)
    decision: DecisionConfig = field(default_factory=DecisionConfig)
    notify: NotifyConfig = field(default_factory=NotifyConfig)
    web: WebConfig = field(default_factory=WebConfig)
    state_dir: str = "data"
    timezone: str = ""  # IANA name for schedules, e.g. America/New_York; empty = system local time
    save_failure_frames: bool = True
    log_level: str = "INFO"

    def validate(self) -> None:
        errors = []
        if not self.printer.host:
            errors.append("printer.host is required")
        if not self.printer.password:
            errors.append("printer.password is required (Settings > Network > PrusaLink on the printer)")
        if self.printer.auth not in ("digest", "apikey"):
            errors.append("printer.auth must be 'digest' or 'apikey'")
        if not self.camera.url:
            errors.append("camera.url is required (rtsp://<camera-ip>/live)")
        if self.decision.action not in ("pause", "stop", "notify"):
            errors.append("decision.action must be 'pause', 'stop' or 'notify'")
        if self.decision.veto_window_s < 0 or self.decision.veto_snooze_s < 0:
            errors.append("decision.veto_window_s / veto_snooze_s must be >= 0")
        from .policy import ScheduleError, parse_schedules, resolve_tz

        try:
            parse_schedules(self.decision.schedules)
        except ScheduleError as exc:
            errors.append(str(exc))
        try:
            resolve_tz(self.timezone)
        except ScheduleError as exc:
            errors.append(str(exc))
        if self.camera.roi is not None:
            r = self.camera.roi
            if len(r) != 4 or not (0 <= r[0] < r[2] <= 1 and 0 <= r[1] < r[3] <= 1):
                errors.append("camera.roi must be [x1, y1, x2, y2] with 0 <= x1 < x2 <= 1 and 0 <= y1 < y2 <= 1")
        if errors:
            raise ValueError("Invalid config:\n  - " + "\n  - ".join(errors))


_ENV_RE = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}")


def _expand(value: Any) -> Any:
    if isinstance(value, str):
        return _ENV_RE.sub(lambda m: os.environ.get(m.group(1), m.group(2) or ""), value)
    if isinstance(value, dict):
        return {k: _expand(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_expand(v) for v in value]
    return value


def _build(cls, data: dict | None):
    obj = cls()
    if not data:
        return obj
    known = {f.name: f for f in fields(cls)}
    for key, value in data.items():
        if key not in known:
            raise ValueError(f"Unknown config key '{key}' in section {cls.__name__}")
        current = getattr(obj, key)
        if is_dataclass(current):
            setattr(obj, key, _build(type(current), value))
        else:
            setattr(obj, key, _coerce(current, value))
    return obj


def _coerce(current: Any, value: Any) -> Any:
    """Coerce env-expanded strings back to the default's type."""
    if value is None or not isinstance(value, str):
        return value
    if isinstance(current, bool):
        return value.strip().lower() in ("1", "true", "yes", "on")
    if isinstance(current, int):
        return int(value)
    if isinstance(current, float):
        return float(value)
    return value


def load_config(path: str | os.PathLike | None) -> Config:
    data: dict = {}
    if path and Path(path).exists():
        with open(path, encoding="utf-8") as fh:
            data = yaml.safe_load(fh) or {}
    elif path:
        raise FileNotFoundError(f"Config file not found: {path}")
    cfg = _build(Config, _expand(data))
    return cfg
