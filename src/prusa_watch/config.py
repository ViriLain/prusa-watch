"""Configuration loading.

Layers, lowest to highest precedence:
  1. Built-in defaults (the dataclass defaults below; escalation.BUILTIN)
  2. Your config.yaml: only the keys you want to change. `${ENV_VAR}` and
     `${ENV_VAR:-default}` are expanded inside it.
  3. Environment overrides: PRUSA_WATCH__<SECTION>__<KEY>=value, e.g.
       PRUSA_WATCH__DECISION__SENSITIVITY=1.25
       PRUSA_WATCH__ESCALATION__DEFAULT_POLICY=watch_only
     Values are parsed as YAML (numbers, true/false, [lists]).

`prusa-watch config` prints the effective result; config.reference.yaml lists
every setting with its default.
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
    open_timeout_s: float = 10.0  # RTSP connect/handshake timeout
    read_timeout_s: float = 10.0  # no frame for this long = reconnect


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
    # Detection only: *whether* a print is failing. What happens next (pause,
    # notify, wait for you, ...) is configured in the top-level `escalation:` section.
    sensitivity: float = 1.0  # >1 = more trigger-happy, <1 = more conservative
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



@dataclass
class NtfyConfig:
    url: str = "https://ntfy.sh"
    topic: str = ""
    token: str = ""  # optional bearer token for protected/self-hosted topics
    # Topic prusa-watch SUBSCRIBES to for button replies (Keep printing / Pause
    # now / Cancel). Buttons post to ntfy, prusa-watch reads them over an
    # outbound connection -> works away from home with no port forwarding.
    # Use a long random name (or an ACL-protected topic on self-hosted ntfy).
    reply_topic: str = ""
    reply_reconnect_s: float = 5.0  # backoff when the reply stream drops


@dataclass
class DiscordConfig:
    webhook_url: str = ""


@dataclass
class WebhookConfig:
    url: str = ""  # receives JSON POST (e.g. Home Assistant webhook trigger)


CHANNELS = ("ntfy", "discord", "webhook")


@dataclass
class EventNotifyConfig:
    """Routing for non-escalation notifications."""

    enabled: bool = True
    channels: list[str] | None = None  # subset of ntfy/discord/webhook; null = every configured channel
    priority: int = 3  # ntfy priority 1 (min) .. 5 (max, bypasses Do Not Disturb on Android)
    cooldown_s: float = 0.0  # min seconds between repeats within one print


@dataclass
class NotifyConfig:
    ntfy: NtfyConfig = field(default_factory=NtfyConfig)
    discord: DiscordConfig = field(default_factory=DiscordConfig)
    webhook: WebhookConfig = field(default_factory=WebhookConfig)
    # "Possible failure" heads-up (warning band, below the failure threshold)
    warning: EventNotifyConfig = field(default_factory=lambda: EventNotifyConfig(priority=4, cooldown_s=300.0))
    # Camera went stale / came back while printing
    camera: EventNotifyConfig = field(default_factory=lambda: EventNotifyConfig(priority=3))
    # Confirmations: "keeping the print running", "resumed", ...
    info: EventNotifyConfig = field(default_factory=lambda: EventNotifyConfig(priority=3))
    timeout_s: float = 15.0  # per-request timeout for notification HTTP calls


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
    history_s: float = 7200.0  # score history kept for the dashboard chart


@dataclass
class RecordingConfig:
    """What the detector saw, kept on disk for tuning (state_dir/history, state_dir/frames)."""

    history: bool = True  # one CSV row per analyzed frame: history/job-<id>.csv
    frames: bool = True  # annotated frames when the model sees something: frames/job-<id>/
    frame_min_p: float = 0.3  # save a frame once its summed confidence reaches this (warning/failure frames always)
    max_frames_per_job: int = 120  # cap on saved frames per print (~100 KB each)
    keep_jobs: int = 50  # history/frames of older prints beyond this many are deleted (0 = keep all)


@dataclass
class Config:
    printer: PrinterConfig = field(default_factory=PrinterConfig)
    camera: CameraConfig = field(default_factory=CameraConfig)
    detector: DetectorConfig = field(default_factory=DetectorConfig)
    decision: DecisionConfig = field(default_factory=DecisionConfig)
    notify: NotifyConfig = field(default_factory=NotifyConfig)
    web: WebConfig = field(default_factory=WebConfig)
    recording: RecordingConfig = field(default_factory=RecordingConfig)
    # What to do once a failure is detected: timed steps (notify / wait / pause /
    # stop), per-step channels, priorities, buttons and message templates, and
    # schedules that pick a policy by time of day. Parsed and validated by
    # prusa_watch.escalation; see config.example.yaml.
    escalation: dict = field(default_factory=dict)
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
        from .escalation import EscalationError, parse_escalation
        from .policy import ScheduleError, resolve_tz

        try:
            parse_escalation(self.escalation)
        except EscalationError as exc:
            errors.append(str(exc))
        try:
            resolve_tz(self.timezone)
        except ScheduleError as exc:
            errors.append(str(exc))
        for name in ("warning", "camera", "info"):
            ev: EventNotifyConfig = getattr(self.notify, name)
            bad = set(ev.channels or []) - set(CHANNELS)
            if bad:
                errors.append(f"notify.{name}.channels: unknown {sorted(bad)} (use {list(CHANNELS)})")
            if not 1 <= int(ev.priority) <= 5:
                errors.append(f"notify.{name}.priority must be 1..5")
        if self.recording.max_frames_per_job < 0 or self.recording.keep_jobs < 0:
            errors.append("recording.max_frames_per_job and recording.keep_jobs must be >= 0")
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


# Keys that existed in earlier versions -> where they live now.
_MOVED = {
    ("decision", "action"): "escalation.policies.<name>.steps[].action",
    ("decision", "veto_window_s"): "escalation.policies.<name>.steps (the step 'at' times)",
    ("decision", "veto_snooze_s"): "escalation.snooze_s",
    ("decision", "schedules"): "escalation.schedules (with `policy:` instead of action/veto_window_s/quiet)",
    ("ntfy", "priority_warning"): "notify.warning.priority",
    ("ntfy", "priority_failure"): "escalation step `priority`",
    ("notify", "cooldown_s"): "notify.warning.cooldown_s",
    ("notify", "notify_camera_down"): "notify.camera.enabled",
}

_DURATION_RE = re.compile(r"^\s*(?:(\d+(?:\.\d+)?)h)?\s*(?:(\d+(?:\.\d+)?)m)?\s*(?:(\d+(?:\.\d+)?)s)?\s*$")


def parse_duration(value: Any) -> float:
    """Seconds from 90, 90.5, "90", "90s", "2m", "1h30m", "1h 5m 10s"."""
    if isinstance(value, bool):
        raise ValueError(f"invalid duration {value!r}")
    if isinstance(value, (int, float)):
        return float(value)
    text = str(value).strip().lower()
    try:
        return float(text)
    except ValueError:
        pass
    m = _DURATION_RE.match(text)
    if not text or not m or not any(m.groups()):
        raise ValueError(f"invalid duration {value!r} (examples: 90, 90s, 2m, 1h30m)")
    h, mi, se = (float(g) if g else 0.0 for g in m.groups())
    return h * 3600 + mi * 60 + se


def _build(cls, data: dict | None, path: str = ""):
    obj = cls()
    if not data:
        return obj
    if not isinstance(data, dict):
        raise ValueError(f"Config section '{path or cls.__name__}' must be a mapping")
    known = {f.name: f for f in fields(cls)}
    section = path.rsplit(".", 1)[-1]
    for key, value in data.items():
        where = f"{path}.{key}" if path else key
        if key not in known:
            moved = _MOVED.get((section, key))
            if moved:
                raise ValueError(f"Config key '{where}' has moved to {moved} (see config.example.yaml)")
            raise ValueError(f"Unknown config key '{where}'")
        current = getattr(obj, key)
        if is_dataclass(current):
            setattr(obj, key, _build(type(current), value, where))
        else:
            try:
                setattr(obj, key, _coerce(current, value, key))
            except ValueError as exc:
                raise ValueError(f"Config key '{where}': {exc}") from exc
    return obj


def _coerce(current: Any, value: Any, key: str = "") -> Any:
    """Coerce env-expanded strings back to the default's type; *_s fields accept durations."""
    if value is None:
        return value
    if isinstance(current, float) and key.endswith("_s"):
        return parse_duration(value)
    if not isinstance(value, str):
        return value
    if isinstance(current, bool):
        return value.strip().lower() in ("1", "true", "yes", "on")
    if isinstance(current, int):
        return int(value)
    if isinstance(current, float):
        return float(value)
    return value


ENV_PREFIX = "PRUSA_WATCH__"


def apply_env_overrides(data: dict, environ: dict | None = None) -> dict:
    """PRUSA_WATCH__A__B__C=value  ->  data["a"]["b"]["c"] = yaml(value)."""
    environ = os.environ if environ is None else environ
    for name in sorted(environ):
        if not name.startswith(ENV_PREFIX):
            continue
        path = [p.lower() for p in name[len(ENV_PREFIX):].split("__") if p]
        if not path:
            continue
        raw = environ[name]
        try:
            value = yaml.safe_load(raw) if raw.strip() else ""
        except yaml.YAMLError:
            value = raw
        node = data
        for key in path[:-1]:
            if not isinstance(node.get(key), dict):
                node[key] = {}
            node = node[key]
        node[path[-1]] = value
    return data


def load_config(path: str | os.PathLike | None, environ: dict | None = None) -> Config:
    data: dict = {}
    if path and Path(path).exists():
        with open(path, encoding="utf-8") as fh:
            data = yaml.safe_load(fh) or {}
    elif path:
        raise FileNotFoundError(f"Config file not found: {path}")
    if not isinstance(data, dict):
        raise ValueError(f"{path}: top level must be a mapping")
    data = apply_env_overrides(_expand(data), environ)
    return _build(Config, data)


_SECRET_KEYS = {"password", "token", "webhook_url", "topic", "reply_topic"}


def effective_config(cfg: Config, redact: bool = True) -> dict:
    """Fully-resolved settings (defaults + file + env) as plain data, for `prusa-watch config`."""
    from dataclasses import asdict

    from .escalation import merge_escalation

    d = asdict(cfg)
    d["escalation"] = merge_escalation(cfg.escalation)
    if redact:

        def scrub(node, parent=""):
            if isinstance(node, dict):
                for k, v in node.items():
                    secret = k in _SECRET_KEYS or (parent == "webhook" and k == "url")
                    if secret and isinstance(v, str) and v:
                        node[k] = "***"
                    else:
                        scrub(v, k)
            elif isinstance(node, list):
                for v in node:
                    scrub(v, parent)

        scrub(d)
    return d
