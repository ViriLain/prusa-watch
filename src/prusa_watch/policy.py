"""What to do when a failure is detected, depending on time of day.

Example (config.yaml):

    decision:
      action: pause
      veto_window_s: 120          # daytime: ask first, pause after 2 min of silence
      schedules:
        - name: night
          start: "22:00"
          end: "07:00"
          veto_window_s: 0        # asleep: pause immediately
          quiet: true             # ...and don't wake me up
        - name: work
          days: [mon, tue, wed, thu, fri]
          start: "09:00"
          end: "17:00"
          veto_window_s: 300      # at work: longer window to look at the camera
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, time, timedelta, tzinfo

from .config import DecisionConfig

DAYS = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
ACTIONS = ("pause", "stop", "notify")


class ScheduleError(ValueError):
    pass


@dataclass
class ScheduleRule:
    name: str
    start: time
    end: time
    days: set[int] | None = None  # weekday() numbers; None = every day
    action: str | None = None
    veto_window_s: float | None = None
    quiet: bool | None = None

    def matches(self, now: datetime) -> bool:
        t = now.time().replace(tzinfo=None)
        if self.start == self.end:  # full day
            return self._day_ok(now)
        if self.start < self.end:
            return self.start <= t < self.end and self._day_ok(now)
        # crosses midnight: the part after midnight belongs to the previous day's window
        if t >= self.start:
            return self._day_ok(now)
        if t < self.end:
            return self._day_ok(now - timedelta(days=1))
        return False

    def _day_ok(self, dt: datetime) -> bool:
        return self.days is None or dt.weekday() in self.days


@dataclass
class Policy:
    action: str
    veto_window_s: float
    quiet: bool
    rule: str | None  # schedule name that applied, None = defaults

    @property
    def uses_veto(self) -> bool:
        return self.action in ("pause", "stop") and self.veto_window_s > 0


def _parse_time(v, where: str) -> time:
    try:
        hh, mm = str(v).strip().split(":")
        return time(int(hh), int(mm))
    except Exception as exc:
        raise ScheduleError(f"{where}: invalid time {v!r} (use \"HH:MM\")") from exc


def parse_schedules(raw: list | None) -> list[ScheduleRule]:
    rules = []
    for i, item in enumerate(raw or []):
        where = f"decision.schedules[{i}]"
        if not isinstance(item, dict):
            raise ScheduleError(f"{where}: must be a mapping")
        unknown = set(item) - {"name", "start", "end", "days", "action", "veto_window_s", "quiet"}
        if unknown:
            raise ScheduleError(f"{where}: unknown keys {sorted(unknown)}")
        if "start" not in item or "end" not in item:
            raise ScheduleError(f"{where}: start and end are required")
        days = None
        if item.get("days"):
            try:
                days = {DAYS.index(str(d).strip().lower()[:3]) for d in item["days"]}
            except ValueError as exc:
                raise ScheduleError(f"{where}: days must be from {DAYS}") from exc
        action = item.get("action")
        if action is not None and action not in ACTIONS:
            raise ScheduleError(f"{where}: action must be one of {ACTIONS}")
        veto = item.get("veto_window_s")
        if veto is not None:
            veto = float(veto)
            if veto < 0:
                raise ScheduleError(f"{where}: veto_window_s must be >= 0")
        quiet = item.get("quiet")
        if isinstance(quiet, str):
            quiet = quiet.strip().lower() in ("1", "true", "yes", "on")
        rules.append(
            ScheduleRule(
                name=str(item.get("name") or f"schedule-{i}"),
                start=_parse_time(item["start"], where),
                end=_parse_time(item["end"], where),
                days=days,
                action=action,
                veto_window_s=veto,
                quiet=quiet,
            )
        )
    return rules


def resolve_tz(name: str) -> tzinfo | None:
    if not name:
        return None
    try:
        from zoneinfo import ZoneInfo

        return ZoneInfo(name)
    except Exception as exc:
        raise ScheduleError(f"timezone: unknown IANA zone {name!r}") from exc


class PolicyResolver:
    def __init__(self, cfg: DecisionConfig, tz: str = ""):
        self.cfg = cfg
        self.rules = parse_schedules(cfg.schedules)
        self.tz = resolve_tz(tz)

    def now(self, ts: float) -> datetime:
        return datetime.fromtimestamp(ts, self.tz) if self.tz else datetime.fromtimestamp(ts)

    def resolve(self, ts: float) -> Policy:
        now = self.now(ts)
        for r in self.rules:
            if r.matches(now):
                return Policy(
                    action=r.action or self.cfg.action,
                    veto_window_s=self.cfg.veto_window_s if r.veto_window_s is None else r.veto_window_s,
                    quiet=bool(r.quiet),
                    rule=r.name,
                )
        return Policy(action=self.cfg.action, veto_window_s=self.cfg.veto_window_s, quiet=False, rule=None)
