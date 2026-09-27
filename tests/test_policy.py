from datetime import datetime
from zoneinfo import ZoneInfo

import pytest

from prusa_watch.config import Config, DecisionConfig
from prusa_watch.policy import PolicyResolver, ScheduleError, parse_schedules

NY = ZoneInfo("America/New_York")


def ts(y, mo, d, h, mi):
    return datetime(y, mo, d, h, mi, tzinfo=NY).timestamp()


def resolver(schedules, **kw):
    return PolicyResolver(DecisionConfig(action="pause", veto_window_s=120, schedules=schedules, **kw), "America/New_York")


NIGHT = {"name": "night", "start": "22:00", "end": "07:00", "veto_window_s": 0, "quiet": True}
WORK = {"name": "work", "days": ["mon", "tue", "wed", "thu", "fri"], "start": "09:00", "end": "17:00", "veto_window_s": 300}


def test_defaults_when_no_rule_matches():
    p = resolver([NIGHT]).resolve(ts(2026, 9, 26, 14, 0))
    assert (p.action, p.veto_window_s, p.quiet, p.rule) == ("pause", 120, False, None)
    assert p.uses_veto


@pytest.mark.parametrize("h,m,expect", [(21, 59, None), (22, 0, "night"), (23, 30, "night"), (0, 0, "night"), (6, 59, "night"), (7, 0, None)])
def test_overnight_window_boundaries(h, m, expect):
    assert resolver([NIGHT]).resolve(ts(2026, 9, 26, h, m)).rule == expect


def test_night_rule_is_immediate_and_quiet():
    p = resolver([NIGHT]).resolve(ts(2026, 9, 27, 3, 0))
    assert p.veto_window_s == 0 and not p.uses_veto and p.quiet


def test_days_filter_and_overnight_day_attribution():
    fri_night = {"name": "fri-night", "days": ["fri"], "start": "22:00", "end": "07:00", "veto_window_s": 0}
    r = resolver([fri_night])
    assert r.resolve(ts(2026, 9, 25, 23, 0)).rule == "fri-night"  # Fri 23:00
    assert r.resolve(ts(2026, 9, 26, 3, 0)).rule == "fri-night"  # Sat 03:00 belongs to Fri's window
    assert r.resolve(ts(2026, 9, 26, 23, 0)).rule is None  # Sat 23:00


def test_weekday_work_window_and_first_match_wins():
    r = resolver([WORK, {"name": "all-day", "start": "00:00", "end": "00:00", "action": "notify"}])
    assert r.resolve(ts(2026, 9, 28, 10, 0)).rule == "work"  # Monday
    sat = r.resolve(ts(2026, 9, 26, 10, 0))
    assert sat.rule == "all-day" and sat.action == "notify" and not sat.uses_veto


def test_timezone_is_respected():
    # 03:00 UTC is 23:00 in New York -> night
    utc3 = datetime(2026, 9, 27, 3, 0, tzinfo=ZoneInfo("UTC")).timestamp()
    assert resolver([NIGHT]).resolve(utc3).rule == "night"


@pytest.mark.parametrize(
    "bad,msg",
    [
        ({"start": "22:00"}, "start and end"),
        ({"start": "25:99", "end": "07:00"}, "invalid time"),
        ({"start": "22:00", "end": "07:00", "days": ["funday"]}, "days"),
        ({"start": "22:00", "end": "07:00", "action": "explode"}, "action"),
        ({"start": "22:00", "end": "07:00", "veto": 1}, "unknown keys"),
    ],
)
def test_schedule_validation(bad, msg):
    with pytest.raises(ScheduleError, match=msg):
        parse_schedules([bad])


def test_config_validate_reports_schedule_and_tz_errors():
    cfg = Config()
    cfg.printer.host, cfg.printer.password, cfg.camera.url = "h", "p", "rtsp://c/live"
    cfg.decision.schedules = [{"start": "nope", "end": "07:00"}]
    cfg.timezone = "Mars/Olympus_Mons"
    with pytest.raises(ValueError) as e:
        cfg.validate()
    assert "invalid time" in str(e.value) and "timezone" in str(e.value)
