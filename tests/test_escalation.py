from datetime import datetime
from zoneinfo import ZoneInfo

import pytest

from prusa_watch.config import Config, load_config, parse_duration
from prusa_watch.escalation import EscalationError, Incident, PolicyResolver, parse_escalation

NY = ZoneInfo("America/New_York")


def ts(y, mo, d, h, mi):
    return datetime(y, mo, d, h, mi, tzinfo=NY).timestamp()


BASE = {
    "default_policy": "day",
    "policies": {
        "day": {"steps": [{"at": 0, "buttons": ["keep", "act", "stop"]}, {"at": "2m", "action": "pause"}]},
        "night": {"steps": [{"at": 0, "action": "pause", "priority": 2}]},
        "work": {"steps": [{"at": 0}, {"at": "5m", "action": "pause"}]},
        "all": {"steps": [{"at": 0}]},
    },
}


def resolver(schedules):
    return PolicyResolver(parse_escalation(dict(BASE, schedules=schedules)), "America/New_York")


NIGHT = {"name": "night", "start": "22:00", "end": "07:00", "policy": "night"}
WORK = {"name": "work", "days": ["mon", "tue", "wed", "thu", "fri"], "start": "09:00", "end": "17:00", "policy": "work"}


def test_builtin_defaults():
    esc = parse_escalation({})
    assert esc.default_policy == "ask_first" and esc.snooze_s == 1800
    assert set(esc.policies) == {"ask_first", "pause_now", "night", "watch_only"}
    ask = esc.policies["ask_first"]
    assert [(s.at, s.action) for s in ask.steps] == [(0, None), (60, None), (120, "pause")]
    assert not any(s.action == "stop" for p in esc.policies.values() for s in p.steps)  # built-ins never cancel
    assert [(r.name, r.policy) for r in esc.schedules] == [("night", "night")]


def test_user_section_merges_by_policy_name():
    esc = parse_escalation({"policies": {"ask_first": {"steps": [{"at": 0}, {"at": "5m", "action": "pause"}]}, "mine": {"steps": [{"at": 0}]}}})
    assert [s.at for s in esc.policies["ask_first"].steps] == [0, 300]  # replaced wholesale
    assert {"pause_now", "night", "watch_only", "mine"} <= set(esc.policies)  # others kept
    assert [r.name for r in esc.schedules] == ["night"]  # schedules untouched when not given


def test_schedules_replace_and_policies_can_be_removed():
    esc = parse_escalation({"schedules": [], "policies": {"night": None}, "default_policy": "watch_only"})
    assert esc.schedules == [] and "night" not in esc.policies and esc.default_policy == "watch_only"
    with pytest.raises(EscalationError, match="not defined"):  # removed policy still scheduled
        parse_escalation({"policies": {"night": None}})


def test_builtin_false_parses_as_is():
    esc = parse_escalation({"policies": {"p": {"steps": [{}]}}}, builtin=False)
    assert list(esc.policies) == ["p"] and esc.schedules == []


def test_default_policy_when_no_schedule_matches():
    pol, sched = resolver([NIGHT]).resolve(ts(2026, 9, 26, 14, 0))
    assert (pol.name, sched) == ("day", None)


@pytest.mark.parametrize("h,m,expect", [(21, 59, None), (22, 0, "night"), (23, 30, "night"), (0, 0, "night"), (6, 59, "night"), (7, 0, None)])
def test_overnight_window_boundaries(h, m, expect):
    assert resolver([NIGHT]).resolve(ts(2026, 9, 26, h, m))[1] == expect


def test_days_filter_and_overnight_day_attribution():
    fri = {"name": "fri-night", "days": ["fri"], "start": "22:00", "end": "07:00", "policy": "night"}
    r = resolver([fri])
    assert r.resolve(ts(2026, 9, 25, 23, 0))[1] == "fri-night"  # Fri 23:00
    assert r.resolve(ts(2026, 9, 26, 3, 0))[1] == "fri-night"  # Sat 03:00 belongs to Fri's window
    assert r.resolve(ts(2026, 9, 26, 23, 0))[1] is None  # Sat 23:00


def test_first_match_wins_and_full_day_rule():
    r = resolver([WORK, {"name": "weekend", "start": "00:00", "end": "00:00", "policy": "all"}])
    assert r.resolve(ts(2026, 9, 28, 10, 0))[0].name == "work"  # Monday
    assert r.resolve(ts(2026, 9, 26, 10, 0))[0].name == "all"  # Saturday


def test_timezone_is_respected():
    utc3 = datetime(2026, 9, 27, 3, 0, tzinfo=ZoneInfo("UTC")).timestamp()  # 23:00 New York
    assert resolver([NIGHT]).resolve(utc3)[1] == "night"


@pytest.mark.parametrize(
    "v,sec",
    [(90, 90), ("90", 90), ("90s", 90), ("2m", 120), ("1h30m", 5400), ("1h 5m 10s", 3910), (1.5, 1.5), ("0", 0)],
)
def test_durations(v, sec):
    assert parse_duration(v) == sec


def test_bad_duration():
    with pytest.raises(ValueError):
        parse_duration("soon")


@pytest.mark.parametrize(
    "esc,msg",
    [
        ({"policies": {"ask_first": None, "pause_now": None, "night": None, "watch_only": None}, "schedules": []}, "at least one policy"),
        ({"policies": {"p": {"steps": []}}}, "non-empty"),
        ({"policies": {"p": {"steps": [{"at": "2m"}, {"at": "1m"}]}}}, "ascending"),
        ({"policies": {"p": {"steps": [{"action": "explode"}]}}}, "action"),
        ({"policies": {"p": {"steps": [{"notify": ["sms"]}]}}}, "unknown channels"),
        ({"policies": {"p": {"steps": [{"priority": 9}]}}}, "priority"),
        ({"policies": {"p": {"steps": [{"buttons": ["keep", "act", "stop", "mute"]}]}}}, "at most 3"),
        ({"policies": {"p": {"steps": [{"buttons": ["launch"]}]}}}, "unknown"),
        ({"policies": {"p": {"steps": [{"title": "{nope}"}]}}}, "bad template"),
        ({"policies": {"p": {"steps": [{"at": "later"}]}}}, "invalid duration"),
        ({"policies": {"p": {"steps": [{"when": 0}]}}}, "unknown keys"),
        ({"default_policy": "q", "policies": {"p": {"steps": [{}]}}}, "default_policy"),
        ({"policies": {"p": {"steps": [{}]}}, "schedules": [{"start": "22:00", "end": "07:00", "policy": "zzz"}]}, "not defined"),
        ({"policies": {"p": {"steps": [{}]}}, "schedules": [{"start": "25:99", "end": "07:00", "policy": "p"}]}, "invalid time"),
        ({"policies": {"p": {"steps": [{}]}}, "schedules": [{"start": "1:00", "end": "2:00", "policy": "p", "action": "pause"}]}, "only picks a `policy`"),
        ({"policies": {"p": {"steps": [{}]}}, "retries": 3}, "unknown keys"),
    ],
)
def test_validation(esc, msg):
    with pytest.raises(EscalationError, match=msg):
        parse_escalation(esc)


def test_incident_navigation():
    pol = parse_escalation(BASE).policies["day"]
    inc = Incident(policy=pol, schedule=None, job_id=1, started_ts=1000.0, score=0.7)
    assert [i for i, _ in inc.due(1000)] == [0]
    assert inc.next_action()[0] == 1 and inc.commands() == ["veto", "act", "stop"]
    inc.next_idx = 1
    assert inc.due(1100) == [] and [i for i, _ in inc.due(1120)] == [1]
    f = inc.template_fields(1030, "core-one", "cube.bgcode", 0.7)
    assert f["next_action_in"] == "1:30" and f["elapsed"] == "0:30" and f["next_action"] == "pause"
    inc.acted = "paused"
    assert inc.default_buttons() == ["resume", "mute", "stop"]


def _base_cfg():
    cfg = Config()
    cfg.printer.host, cfg.printer.password, cfg.camera.url = "h", "p", "rtsp://c/live"
    return cfg


def test_config_validate_reports_escalation_tz_and_channel_errors():
    cfg = _base_cfg()
    cfg.escalation = {"policies": {"p": {"steps": [{"at": "nope"}]}}}
    cfg.timezone = "Mars/Olympus_Mons"
    cfg.notify.warning.channels = ["pager"]
    with pytest.raises(ValueError) as e:
        cfg.validate()
    assert "invalid duration" in str(e.value) and "timezone" in str(e.value) and "pager" in str(e.value)


@pytest.mark.parametrize(
    "yaml_text,where",
    [
        ("decision: {action: pause}", "escalation.policies.<name>.steps[].action"),
        ("decision: {veto_window_s: 120}", "step 'at' times"),
        ("decision: {schedules: []}", "escalation.schedules"),
        ("notify: {cooldown_s: 300}", "notify.warning.cooldown_s"),
        ("notify: {ntfy: {priority_failure: 5}}", "escalation step `priority`"),
        ("notify: {notify_camera_down: false}", "notify.camera.enabled"),
    ],
)
def test_moved_keys_explain_where_they_went(tmp_path, yaml_text, where):
    p = tmp_path / "c.yaml"
    p.write_text(yaml_text)
    with pytest.raises(ValueError, match="has moved to") as e:
        load_config(p)
    assert where in str(e.value)


def test_duration_strings_in_regular_settings(tmp_path):
    p = tmp_path / "c.yaml"
    p.write_text("decision: {resume_grace_s: 3m}\nnotify: {warning: {cooldown_s: 10m, channels: [ntfy], priority: 2}}\nweb: {history_s: 4h}\n")
    cfg = load_config(p)
    assert cfg.decision.resume_grace_s == 180 and cfg.notify.warning.cooldown_s == 600 and cfg.web.history_s == 14400
    assert cfg.notify.warning.channels == ["ntfy"] and cfg.notify.warning.priority == 2


def test_example_config_is_minimal_and_valid():
    env = {"PRUSALINK_PASSWORD": "x", "NTFY_TOPIC": "t"}
    import os

    old = {k: os.environ.get(k) for k in env}
    os.environ.update(env)
    try:
        cfg = load_config("config.example.yaml", environ={})
    finally:
        for k, v in old.items():
            if v is None:
                os.environ.pop(k)
            else:
                os.environ[k] = v
    cfg.validate()
    assert cfg.escalation == {}  # uses built-in escalation
    lines = [ln for ln in open("config.example.yaml") if ln.strip() and not ln.lstrip().startswith("#")]
    assert len(lines) <= 15, "the starter config should stay small"


def test_reference_file_equals_builtin_defaults():
    """config.reference.yaml must be exactly the built-in defaults (docs can't drift)."""
    ref = load_config("config.reference.yaml", environ={})
    assert parse_escalation(ref.escalation, builtin=False) == parse_escalation({})
    ref.escalation = {}
    assert ref == Config()


def test_env_overrides_beat_the_file(tmp_path):
    p = tmp_path / "c.yaml"
    p.write_text("decision: {sensitivity: 1.1}\nescalation: {default_policy: night}\n")
    env = {
        "PRUSA_WATCH__DECISION__SENSITIVITY": "1.4",
        "PRUSA_WATCH__ESCALATION__DEFAULT_POLICY": "watch_only",
        "PRUSA_WATCH__NOTIFY__WARNING__CHANNELS": "[ntfy]",
        "PRUSA_WATCH__NOTIFY__WARNING__ENABLED": "false",
        "PRUSA_WATCH__CAMERA__STALE_AFTER_S": "2m",
        "PRUSA_WATCH_CONFIG": "ignored.yaml",  # single underscore: not an override
    }
    cfg = load_config(p, environ=env)
    assert cfg.decision.sensitivity == 1.4
    assert parse_escalation(cfg.escalation).default_policy == "watch_only"
    assert cfg.notify.warning.channels == ["ntfy"] and cfg.notify.warning.enabled is False
    assert cfg.camera.stale_after_s == 120
    with pytest.raises(ValueError, match="Unknown config key 'decision.sensitivty'"):
        load_config(p, environ={"PRUSA_WATCH__DECISION__SENSITIVTY": "1"})


def test_config_command_prints_effective_config_with_secrets_masked(tmp_path, capsys):
    import yaml

    from prusa_watch.__main__ import main

    p = tmp_path / "c.yaml"
    p.write_text("printer: {host: 10.0.0.5, password: hunter2}\ncamera: {url: rtsp://c/live}\n"
                 "notify: {ntfy: {topic: secret-topic}}\nescalation: {default_policy: watch_only}\n")
    assert main(["config", "-c", str(p)]) == 0
    out = yaml.safe_load(capsys.readouterr().out)
    assert out["printer"]["host"] == "10.0.0.5" and out["printer"]["password"] == "***"
    assert out["notify"]["ntfy"]["topic"] == "***"
    assert out["escalation"]["default_policy"] == "watch_only"
    assert set(out["escalation"]["policies"]) >= {"ask_first", "night"}  # merged built-ins shown
    assert main(["config", "--defaults"]) == 0


def test_example_config_documents_every_setting():
    """Guard: a new setting must be added to config.example.yaml too."""
    from dataclasses import fields, is_dataclass

    import yaml

    raw = yaml.safe_load(open("config.reference.yaml"))
    missing = []

    def walk(obj, data, path):
        for f in fields(obj):
            where = f"{path}{f.name}"
            if not isinstance(data, dict) or f.name not in data:
                missing.append(where)
                continue
            val = getattr(obj, f.name)
            if is_dataclass(val):
                walk(val, data[f.name], where + ".")

    walk(Config(), raw, "")
    assert missing == [], f"undocumented settings: {missing}"
