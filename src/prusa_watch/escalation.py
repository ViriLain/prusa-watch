"""Escalation policies: what happens after the detector says "failure".

A policy is an ordered list of timed steps, measured from the moment the
failure is detected (an *incident*). Each step can notify (which channels, what
priority, which buttons, what text) and/or act on the printer (pause / stop).
Schedules pick which policy applies by time of day.

Sensible defaults are built in (BUILTIN below): ask first and pause after 2 min
during the day, pause silently at night (22:00-07:00). Your config only needs
the parts you want to change, e.g.

    escalation:
      default_policy: watch_only          # first few prints: never touch the printer
      policies:
        ask_first:                        # replaces just this built-in policy
          steps:
            - {at: 0, buttons: [keep, act, stop]}
            - {at: 5m, action: pause}
      schedules: []                       # no night mode

An incident ends when you answer (keep / act / stop / resume / mute), when you
handle it at the printer (pause/resume/stop there), when the job ends, or when
its steps run out (unless the printer is sitting paused by us, in which case
it stays open so the Resume buttons keep working).
"""

from __future__ import annotations

import secrets
from dataclasses import dataclass, field

from .config import CHANNELS, parse_duration
from .policy import ScheduleError, ScheduleRule, local_now, parse_schedules, resolve_tz

ACTIONS = ("pause", "stop")
# button -> (reply command, default label)
BUTTONS = {
    "keep": ("veto", "Keep printing"),
    "act": ("act", None),  # label depends on the next action: "Pause now" / "Stop now"
    "stop": ("stop", "Cancel print"),
    "resume": ("resume", "Resume"),
    "mute": ("mute", "False alarm: resume + mute"),
    "dashboard": (None, "Dashboard"),  # opens web.public_url
}
TEMPLATE_FIELDS = {
    "printer": "core-one",
    "job": "benchy.bgcode",
    "score": "0.71",
    "policy": "ask_first",
    "schedule": "night",
    "next_action": "pause",
    "next_action_in": "1:30",
    "elapsed": "0:30",
    "action_taken": "paused",
}


class EscalationError(ValueError):
    pass


@dataclass
class Step:
    at: float  # seconds after the incident started
    action: str | None = None  # "pause" | "stop" | None (notify only)
    notify: list[str] | None = None  # channels; None = all configured, [] = silent
    priority: int = 5  # ntfy 1..5
    buttons: list[str] | None = None  # None = automatic for the incident's state
    title: str | None = None  # template, see TEMPLATE_FIELDS
    message: str | None = None
    attach_image: bool = True


@dataclass
class Policy:
    name: str
    steps: list[Step]

    @property
    def has_action(self) -> bool:
        return any(s.action for s in self.steps)


@dataclass
class EscalationConfig:
    policies: dict[str, Policy]
    default_policy: str
    schedules: list[ScheduleRule] = field(default_factory=list)
    snooze_s: float = 1800.0


# Built-in defaults. Your `escalation:` section is merged on top of this:
#   - policies merge BY NAME: define `ask_first` to replace that one policy, or add new names
#   - `schedules`, if you set it, replaces the list ([] turns the night schedule off)
#   - `default_policy` / `snooze_s` override
# Deliberately non-destructive: nothing here ever cancels a print. Add a `stop`
# step yourself if you want that (see config.reference.yaml).
BUILTIN = {
    "default_policy": "ask_first",
    "snooze_s": "30m",
    "policies": {
        # Ask first, remind once, pause after 2 min of silence.
        "ask_first": {
            "steps": [
                {"at": 0, "priority": 5, "buttons": ["keep", "act", "stop"]},
                {
                    "at": "1m",
                    "priority": 5,
                    "title": "{printer}: still failing, {next_action} in {next_action_in}",
                    "attach_image": False,
                },
                {"at": "2m", "action": "pause", "priority": 5},
            ]
        },
        # Pause immediately, loud.
        "pause_now": {"steps": [{"at": 0, "action": "pause", "priority": 5}]},
        # Pause immediately, silent notification (you'll see it in the morning).
        "night": {"steps": [{"at": 0, "action": "pause", "priority": 2}]},
        # Never touch the printer; just tell me.
        "watch_only": {"steps": [{"at": 0, "priority": 4, "buttons": ["stop", "dashboard"]}]},
    },
    "schedules": [{"name": "night", "start": "22:00", "end": "07:00", "policy": "night"}],
}
_TOP_KEYS = {"default_policy", "policies", "schedules", "snooze_s"}


def merge_escalation(user: dict | None) -> dict:
    """Built-in escalation with the user's section layered on top (see BUILTIN)."""
    import copy

    user = user or {}
    if not isinstance(user, dict):
        raise EscalationError("escalation must be a mapping")
    unknown = set(user) - _TOP_KEYS
    if unknown:
        raise EscalationError(f"escalation: unknown keys {sorted(unknown)}")
    merged = copy.deepcopy(BUILTIN)
    for key in ("default_policy", "snooze_s", "schedules"):
        if key in user:
            merged[key] = copy.deepcopy(user[key]) if user[key] is not None else ([] if key == "schedules" else merged[key])
    pols = user.get("policies") or {}
    if not isinstance(pols, dict):
        raise EscalationError("escalation.policies must be a mapping of name -> {steps: [...]}")
    for name, body in pols.items():
        if body is None:  # `name: null` removes a built-in policy
            merged["policies"].pop(str(name), None)
        else:
            merged["policies"][str(name)] = copy.deepcopy(body)
    return merged


def _check_template(t, where: str) -> str | None:
    if t is None:
        return None
    t = str(t)
    try:
        t.format(**TEMPLATE_FIELDS)
    except (KeyError, IndexError, ValueError) as exc:
        raise EscalationError(f"{where}: bad template {t!r} ({exc}); fields: {sorted(TEMPLATE_FIELDS)}") from exc
    return t


def _parse_step(raw, where: str) -> Step:
    if not isinstance(raw, dict):
        raise EscalationError(f"{where}: must be a mapping")
    unknown = set(raw) - {"at", "action", "notify", "priority", "buttons", "title", "message", "attach_image"}
    if unknown:
        raise EscalationError(f"{where}: unknown keys {sorted(unknown)}")
    try:
        at = parse_duration(raw.get("at", 0))
    except ValueError as exc:
        raise EscalationError(f"{where}.at: {exc}") from exc
    if at < 0:
        raise EscalationError(f"{where}.at must be >= 0")
    action = raw.get("action")
    if action in (None, "none", "notify", False):
        action = None
    elif action not in ACTIONS:
        raise EscalationError(f"{where}.action must be one of {list(ACTIONS)} or none")
    notify = raw.get("notify", None)
    if notify is False:
        notify = []
    elif notify is True:
        notify = None
    elif isinstance(notify, str):
        notify = [notify]
    if notify is not None:
        bad = set(notify) - set(CHANNELS)
        if bad:
            raise EscalationError(f"{where}.notify: unknown channels {sorted(bad)} (use {list(CHANNELS)})")
    try:
        priority = int(raw.get("priority", 5))
    except (TypeError, ValueError) as exc:
        raise EscalationError(f"{where}.priority must be 1..5") from exc
    if not 1 <= priority <= 5:
        raise EscalationError(f"{where}.priority must be 1..5")
    buttons = raw.get("buttons")
    if buttons is not None:
        if isinstance(buttons, str):
            buttons = [buttons]
        bad = set(buttons) - set(BUTTONS)
        if bad:
            raise EscalationError(f"{where}.buttons: unknown {sorted(bad)} (use {list(BUTTONS)})")
        if len(buttons) > 3:
            raise EscalationError(f"{where}.buttons: ntfy allows at most 3")
    return Step(
        at=at,
        action=action,
        notify=list(notify) if notify is not None else None,
        priority=priority,
        buttons=list(buttons) if buttons is not None else None,
        title=_check_template(raw.get("title"), f"{where}.title"),
        message=_check_template(raw.get("message"), f"{where}.message"),
        attach_image=bool(raw.get("attach_image", True)),
    )


def parse_escalation(user: dict | None, builtin: bool = True) -> EscalationConfig:
    """Parse the user's escalation section merged over BUILTIN (builtin=False: parse as-is)."""
    raw = merge_escalation(user) if builtin else (user or {})
    if not isinstance(raw, dict):
        raise EscalationError("escalation must be a mapping")
    unknown = set(raw) - _TOP_KEYS
    if unknown:
        raise EscalationError(f"escalation: unknown keys {sorted(unknown)}")
    pols_raw = raw.get("policies") or {}
    if not isinstance(pols_raw, dict) or not pols_raw:
        raise EscalationError("escalation.policies must define at least one policy")
    policies: dict[str, Policy] = {}
    for name, body in pols_raw.items():
        where = f"escalation.policies.{name}"
        steps_raw = body.get("steps") if isinstance(body, dict) else body
        if not isinstance(steps_raw, list) or not steps_raw:
            raise EscalationError(f"{where}.steps must be a non-empty list")
        if isinstance(body, dict) and set(body) - {"steps"}:
            raise EscalationError(f"{where}: unknown keys {sorted(set(body) - {'steps'})}")
        steps = [_parse_step(s, f"{where}.steps[{i}]") for i, s in enumerate(steps_raw)]
        for a, b in zip(steps, steps[1:]):
            if b.at < a.at:
                raise EscalationError(f"{where}.steps must be in ascending `at` order")
        policies[str(name)] = Policy(str(name), steps)
    default = str(raw.get("default_policy") or next(iter(policies)))
    if default not in policies:
        raise EscalationError(f"escalation.default_policy {default!r} is not defined in escalation.policies")
    try:
        schedules = parse_schedules(raw.get("schedules"), set(policies))
        snooze = parse_duration(raw.get("snooze_s", 1800))
    except (ScheduleError, ValueError) as exc:
        raise EscalationError(str(exc)) from exc
    if snooze < 0:
        raise EscalationError("escalation.snooze_s must be >= 0")
    return EscalationConfig(policies=policies, default_policy=default, schedules=schedules, snooze_s=snooze)


class PolicyResolver:
    def __init__(self, esc: EscalationConfig, tz: str = ""):
        self.esc = esc
        self.tz = resolve_tz(tz)

    def resolve(self, ts: float) -> tuple[Policy, str | None]:
        now = local_now(ts, self.tz)
        for rule in self.esc.schedules:
            if rule.matches(now):
                return self.esc.policies[rule.policy], rule.name
        return self.esc.policies[self.esc.default_policy], None


def fmt_duration(seconds: float) -> str:
    seconds = max(0, int(round(seconds)))
    h, rem = divmod(seconds, 3600)
    m, s = divmod(rem, 60)
    return f"{h}:{m:02d}:{s:02d}" if h else f"{m}:{s:02d}"


@dataclass
class Incident:
    policy: Policy
    schedule: str | None
    job_id: int | None
    started_ts: float
    score: float
    jpeg: bytes = b""
    id: str = field(default_factory=lambda: secrets.token_urlsafe(6))
    next_idx: int = 0  # first step not yet executed
    acted: str | None = None  # "paused" | "stopped" once we acted on the printer

    def due(self, now: float) -> list[tuple[int, Step]]:
        out = []
        for i in range(self.next_idx, len(self.policy.steps)):
            if self.started_ts + self.policy.steps[i].at <= now:
                out.append((i, self.policy.steps[i]))
            else:
                break
        return out

    def next_action(self) -> tuple[int, Step] | None:
        for i in range(self.next_idx, len(self.policy.steps)):
            if self.policy.steps[i].action:
                return i, self.policy.steps[i]
        return None

    @property
    def steps_done(self) -> bool:
        return self.next_idx >= len(self.policy.steps)

    def commands(self) -> list[str]:
        """Reply commands that make sense right now."""
        if self.acted == "paused":
            return ["resume", "mute", "stop", "veto"]
        if self.acted:
            return []
        cmds = ["veto", "stop"]
        if self.next_action():
            cmds.insert(1, "act")
        return cmds

    def default_buttons(self) -> list[str]:
        if self.acted == "paused":
            return ["resume", "mute", "stop"]
        if self.acted:
            return ["dashboard"]
        if self.next_action():
            return ["keep", "act", "stop"]
        return ["keep", "stop", "dashboard"]

    def template_fields(self, now: float, printer: str, job: str | None, score: float) -> dict:
        na = self.next_action()
        return {
            "printer": printer,
            "job": job or (str(self.job_id) if self.job_id is not None else "print"),
            "score": f"{score:.2f}",
            "policy": self.policy.name,
            "schedule": self.schedule or "",
            "next_action": na[1].action if na else "",
            "next_action_in": fmt_duration(self.started_ts + na[1].at - now) if na else "",
            "elapsed": fmt_duration(now - self.started_ts),
            "action_taken": self.acted or "",
        }

    def public(self, now: float) -> dict:
        na = self.next_action()
        return {
            "id": self.id,
            "policy": self.policy.name,
            "schedule": self.schedule,
            "job_id": self.job_id,
            "started_ts": self.started_ts,
            "elapsed_s": now - self.started_ts,
            "score": self.score,
            "acted": self.acted,
            "next_action": na[1].action if na else None,
            "next_action_ts": self.started_ts + na[1].at if na else None,
            "next_action_in_s": max(0.0, self.started_ts + na[1].at - now) if na else None,
            "commands": self.commands(),
            "steps": [
                {"at": s.at, "action": s.action, "notify": s.notify, "done": i < self.next_idx}
                for i, s in enumerate(self.policy.steps)
            ],
        }
