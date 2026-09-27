"""Time-of-day schedule primitives (which escalation policy applies when)."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, time, timedelta, tzinfo

DAYS = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]


class ScheduleError(ValueError):
    pass


@dataclass
class ScheduleRule:
    name: str
    start: time
    end: time
    policy: str
    days: set[int] | None = None  # weekday() numbers; None = every day

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


def _parse_time(v, where: str) -> time:
    try:
        hh, mm = str(v).strip().split(":")
        return time(int(hh), int(mm))
    except Exception as exc:
        raise ScheduleError(f"{where}: invalid time {v!r} (use \"HH:MM\")") from exc


def parse_schedules(raw: list | None, policies: set[str]) -> list[ScheduleRule]:
    rules = []
    if raw is not None and not isinstance(raw, list):
        raise ScheduleError("escalation.schedules must be a list")
    for i, item in enumerate(raw or []):
        where = f"escalation.schedules[{i}]"
        if not isinstance(item, dict):
            raise ScheduleError(f"{where}: must be a mapping")
        unknown = set(item) - {"name", "start", "end", "days", "policy"}
        if unknown:
            raise ScheduleError(f"{where}: unknown keys {sorted(unknown)} (a schedule only picks a `policy`)")
        for k in ("start", "end", "policy"):
            if k not in item:
                raise ScheduleError(f"{where}: `{k}` is required")
        if item["policy"] not in policies:
            raise ScheduleError(f"{where}: policy {item['policy']!r} is not defined in escalation.policies")
        days = None
        if item.get("days"):
            try:
                days = {DAYS.index(str(d).strip().lower()[:3]) for d in item["days"]}
            except ValueError as exc:
                raise ScheduleError(f"{where}: days must be from {DAYS}") from exc
        rules.append(
            ScheduleRule(
                name=str(item.get("name") or f"schedule-{i}"),
                start=_parse_time(item["start"], where),
                end=_parse_time(item["end"], where),
                policy=str(item["policy"]),
                days=days,
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


def local_now(ts: float, tz: tzinfo | None) -> datetime:
    return datetime.fromtimestamp(ts, tz) if tz else datetime.fromtimestamp(ts)
