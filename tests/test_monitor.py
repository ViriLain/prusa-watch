"""End-to-end loop tests with a fake printer, fake camera, real detector (fake weights), real notifier."""

import httpx
import pytest

from conftest import solid
from prusa_watch.camera import Frame
from prusa_watch.config import Config
from prusa_watch.detector import SpaghettiDetector
from prusa_watch.monitor import Monitor
from prusa_watch.notify import Notifier
from prusa_watch.prusalink import PrinterStatus, PrusaLinkError


class Clock:
    def __init__(self):
        self.t = 1_000_000.0

    def __call__(self):
        return self.t


class FakePrinter:
    def __init__(self):
        self.state = "IDLE"
        self.job_id = None
        self.calls = []
        self.reachable = True
        self.fail_pause = False

    def status(self):
        if not self.reachable:
            raise PrusaLinkError("GET /api/v1/status: timed out")
        return PrinterStatus(self.state, self.job_id, 10.0, 100, 215.0, 60.0, {})

    def job(self):
        return {"id": self.job_id} if self.job_id else None

    def job_name(self):
        return f"part-{self.job_id}.bgcode"

    def pause(self, job_id):
        self.calls.append(("pause", job_id))
        if self.fail_pause:
            raise PrusaLinkError("PUT pause: HTTP 409")
        self.state = "PAUSED"

    def resume(self, job_id):
        self.calls.append(("resume", job_id))
        self.state = "PRINTING"

    def stop(self, job_id):
        self.calls.append(("stop", job_id))
        self.state = "STOPPED"


class FakeGrabber:
    def __init__(self, clock):
        self.clock = clock
        self.image = solid(0)
        self.connected = True
        self.stale = False

    def start(self):
        pass

    def stop(self):
        pass

    def latest(self):
        if self.image is None:
            return None
        return Frame(self.image, self.clock() - (120 if self.stale else 0.1))


@pytest.fixture
def rig(fake_model, tmp_path):
    cfg = Config()
    cfg.printer.host = "printer"
    cfg.printer.password = "x"
    cfg.camera.url = "rtsp://cam/live"
    cfg.state_dir = str(tmp_path / "data")
    cfg.notify.ntfy.topic = "prusa-test"
    cfg.notify.ntfy.url = "https://ntfy.example"
    cfg.web.public_url = "http://watch.lan:8484"
    cfg.web.token = "tok"
    # Most tests exercise the simplest policy; escalation tests below override this.
    cfg.escalation = {"default_policy": "pause_now", "schedules": []}

    sent = []

    def ntfy(request: httpx.Request):
        sent.append(request)
        return httpx.Response(200, json={"id": "x"})

    clock = Clock()
    printer = FakePrinter()
    grabber = FakeGrabber(clock)
    notifier = Notifier(cfg.notify, cfg.web.public_url, cfg.web.token, transport=httpx.MockTransport(ntfy))
    notifier.send = lambda e, channels=None, blocking=False: Notifier.send(notifier, e, channels, blocking=True)
    mon = Monitor(cfg, printer=printer, grabber=grabber, detector=SpaghettiDetector(fake_model), notifier=notifier, clock=clock)
    return mon, printer, grabber, clock, sent, cfg


def advance(mon, clock, n, step=10.0):
    for _ in range(n):
        clock.t += step
        mon.tick()


def test_idle_printer_is_not_analyzed(rig):
    mon, printer, grabber, clock, sent, _ = rig
    grabber.image = solid(255)
    advance(mon, clock, 50)
    assert mon.counters.frames_analyzed == 0
    assert printer.calls == [] and sent == []


def test_spaghetti_pauses_print_and_notifies_with_image(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    printer.state, printer.job_id = "PRINTING", 7
    advance(mon, clock, 60)  # 10 clean minutes
    assert mon.job.job_name == "part-7.bgcode"
    assert printer.calls == []

    grabber.image = solid(255)  # spaghetti everywhere (fake model p ~1.2/frame)
    advance(mon, clock, 30)

    assert ("pause", 7) in printer.calls
    assert printer.calls.count(("pause", 7)) == 1  # exactly once
    assert printer.state == "PAUSED"
    assert mon.counters.pauses == 1

    failure = [r for r in sent if r.headers.get("Priority") == "5"]
    assert failure, "failure notification not sent"
    r = failure[0]
    assert r.method == "PUT" and r.url.path == "/prusa-test"
    assert r.content[:2] == b"\xff\xd8"  # JPEG attached
    assert "PAUSED" in r.headers["Message"]
    assert "/api/incident/resume?token=tok&id=" in r.headers["Actions"]
    assert "False alarm" in r.headers["Actions"] and "Cancel print" in r.headers["Actions"]

    # snapshot of the failure saved to disk
    saved = list((mon.state_dir / "failures").glob("*.jpg"))
    assert len(saved) == 1

    # while paused, no further analysis or actions
    before = mon.counters.frames_analyzed
    advance(mon, clock, 10)
    assert mon.counters.frames_analyzed == before


def test_resume_after_ai_pause_rearms_after_grace(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    printer.state, printer.job_id = "PRINTING", 8
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 30)
    assert printer.state == "PAUSED"

    # user looks, decides to resume anyway (mess still visible)
    mon.resume()
    warn_count = lambda: sum(1 for r in sent if r.headers.get("Priority") == "4")
    before = warn_count()
    advance(mon, clock, 11)  # inside resume_grace_s (120 s): no actions, no alerts
    assert printer.calls.count(("pause", 8)) == 1
    assert warn_count() == before
    assert mon.job.action_taken is None
    # Obico semantics: after a resume the per-print short mean has absorbed the
    # mess, so it only re-pauses if things get *worse*. Crank sensitivity to emulate.
    cfg.decision.sensitivity = 3.0
    advance(mon, clock, 5)
    assert printer.calls.count(("pause", 8)) == 2


def test_resume_with_mute_stops_further_actions(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    printer.state, printer.job_id = "PRINTING", 10
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 30)
    assert "/api/incident/mute?" in [r for r in sent if r.headers.get("Priority") == "5"][0].headers["Actions"]
    assert "muted" in mon.resume(mute=True)
    advance(mon, clock, 60)
    assert printer.calls.count(("pause", 10)) == 1


def test_mute_prevents_action(rig):
    mon, printer, grabber, clock, sent, _ = rig
    printer.state, printer.job_id = "PRINTING", 9
    advance(mon, clock, 1)
    mon.set_muted(True)
    advance(mon, clock, 59)
    grabber.image = solid(255)
    advance(mon, clock, 40)
    assert printer.calls == []


def test_new_job_resets_state_and_mute(rig):
    mon, printer, grabber, clock, sent, _ = rig
    printer.state, printer.job_id = "PRINTING", 1
    advance(mon, clock, 40)
    mon.set_muted(True)
    printer.state = "FINISHED"
    advance(mon, clock, 2)
    printer.state, printer.job_id = "PRINTING", 2
    advance(mon, clock, 1)
    assert mon.job.job_id == 2 and not mon.job.muted
    assert mon.decider.state.current_frame_num == 1
    assert mon.decider.state.lifetime_frame_num > 40  # baseline history kept


def test_notify_only_mode_does_not_touch_printer(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    cfg.escalation = {"default_policy": "watch", "policies": {"watch": {"steps": [{"at": 0}]}}}
    mon.reload_escalation()
    printer.state, printer.job_id = "PRINTING", 3
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 30)
    assert printer.calls == []
    assert any(r.headers.get("Priority") == "5" for r in sent)
    assert sum(1 for r in sent if r.headers.get("Priority") == "5") == 1  # not spammed


def test_stop_mode(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    cfg.escalation = {"default_policy": "kill", "policies": {"kill": {"steps": [{"at": 0, "action": "stop"}]}}}
    mon.reload_escalation()
    printer.state, printer.job_id = "PRINTING", 4
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 30)
    assert ("stop", 4) in printer.calls and printer.state == "STOPPED"


def test_failed_pause_is_reported_loudly(rig):
    mon, printer, grabber, clock, sent, _ = rig
    printer.fail_pause = True
    printer.state, printer.job_id = "PRINTING", 5
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 30)
    titles = [r.headers.get("Title", "") for r in sent if r.headers.get("Priority") == "5"]
    assert any("pause FAILED" in t for t in titles)


def test_camera_down_while_printing_notifies_once(rig):
    mon, printer, grabber, clock, sent, _ = rig
    printer.state, printer.job_id = "PRINTING", 6
    advance(mon, clock, 3)
    grabber.stale = True
    advance(mon, clock, 10)
    down = [r for r in sent if "camera offline" in r.headers.get("Title", "")]
    assert len(down) == 1
    grabber.stale = False
    advance(mon, clock, 1)
    assert any("back online" in r.headers.get("Title", "") for r in sent)


def test_printer_unreachable_is_survivable(rig):
    mon, printer, grabber, clock, sent, _ = rig
    printer.reachable = False
    advance(mon, clock, 5)
    snap = mon.snapshot()
    assert not snap.printer_reachable and "timed out" in snap.printer_error
    printer.reachable = True
    printer.state, printer.job_id = "PRINTING", 11
    advance(mon, clock, 2)
    assert mon.snapshot().printer_reachable and mon.counters.frames_analyzed >= 1


def test_detection_interval_respected(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    printer.state, printer.job_id = "PRINTING", 12
    advance(mon, clock, 20, step=5.0)  # poll every 5 s, detect every 10 s
    assert 9 <= mon.counters.frames_analyzed <= 11



# ---------------------------------------------------------------- escalation policies
ASK_FIRST = {
    "default_policy": "ask_first",
    "snooze_s": "10m",
    "policies": {
        "ask_first": {
            "steps": [
                {"at": 0, "notify": ["ntfy"], "priority": 5, "buttons": ["keep", "act", "stop"]},
                {"at": "1m", "notify": ["ntfy"], "priority": 4, "title": "{printer}: reminder, {next_action} in {next_action_in}", "attach_image": False},
                {"at": "2m", "action": "pause", "notify": ["ntfy", "webhook"]},
                {"at": "32m", "action": "stop"},
            ]
        },
        "night": {"steps": [{"at": 0, "action": "pause", "priority": 2}]},
        "watch": {"steps": [{"at": 0, "notify": ["ntfy"], "priority": 3}]},
    },
}


def _arm(mon, cfg, esc=None, reply_topic="reply-xyz", tz="UTC", webhook=True):
    cfg.escalation = esc if esc is not None else ASK_FIRST
    cfg.notify.ntfy.reply_topic = reply_topic
    if webhook:
        cfg.notify.webhook.url = "https://ha.example/api/webhook/pw"
    cfg.timezone = tz
    mon.reload_escalation()


def _spaghetti_until_incident(mon, printer, grabber, clock, job):
    printer.state, printer.job_id = "PRINTING", job
    advance(mon, clock, 60)
    grabber.image = solid(255)
    for _ in range(40):
        advance(mon, clock, 1)
        if mon.incident:
            return mon.incident
    raise AssertionError("no incident opened")


def _ntfy(sent):
    return [r for r in sent if r.url.host == "ntfy.example"]


def _hooks(sent):
    import json as _j

    return [_j.loads(r.content) for r in sent if r.url.host == "ha.example"]


def test_policy_steps_run_on_schedule_with_per_step_routing(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg)
    inc = _spaghetti_until_incident(mon, printer, grabber, clock, 30)
    assert printer.calls == []
    (first,) = _ntfy(sent)[-1:]
    assert first.headers["Priority"] == "5" and "pausing in 2:00" in first.headers["Title"]
    acts = first.headers["Actions"]
    assert f"https://ntfy.example/reply-xyz, method=POST, body=veto {inc.id}" in acts
    assert f"body=act {inc.id}" in acts and f"body=stop {inc.id}" in acts
    assert [h for h in _hooks(sent) if h["kind"] != "warning"] == []  # step 0 is ntfy-only

    advance(mon, clock, 6)  # 1 min: reminder, custom template, no image, priority 4
    rem = _ntfy(sent)[-1]
    assert rem.headers["Title"] == "core-one: reminder, pause in 1:00" and rem.headers["Priority"] == "4"
    assert rem.method == "POST"  # no attachment
    assert printer.calls == []

    advance(mon, clock, 6)  # 2 min: pause, ntfy + webhook, default resume buttons
    assert printer.calls == [("pause", 30)] and mon.counters.auto_actions == 1
    paused = _ntfy(sent)[-1]
    assert "PAUSED" in paused.headers["Title"]
    assert f"body=resume {inc.id}" in paused.headers["Actions"] and f"body=mute {inc.id}" in paused.headers["Actions"]
    hook = _hooks(sent)[-1]
    assert hook["action_taken"] == "paused" and hook["incident_id"] == inc.id and hook["next_action"] == "stop"
    assert hook["command_urls"]["resume"].endswith(f"id={inc.id}")
    assert mon.incident is inc  # stays open while paused so Resume works

    advance(mon, clock, 179)  # nobody answered for 30 min -> stop
    assert ("stop", 30) not in printer.calls
    advance(mon, clock, 2)
    assert ("stop", 30) in printer.calls and mon.counters.auto_actions == 2
    advance(mon, clock, 1)
    assert mon.incident is None


def test_keep_printing_closes_incident_and_snoozes(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg)
    inc = _spaghetti_until_incident(mon, printer, grabber, clock, 31)
    assert mon.handle_reply("veto", inc.id).startswith("vetoed (no new alerts for 10:00)")
    assert mon.incident is None and mon.counters.vetoes == 1
    advance(mon, clock, 55)  # 550 s inside the 10 min snooze
    assert printer.calls == [] and mon.incident is None
    cfg.decision.sensitivity = 3.0  # "worse" (Obico's short mean absorbed the mess meanwhile)
    advance(mon, clock, 10)
    assert mon.incident is not None and mon.incident.id != inc.id
    assert printer.calls == []  # asks again, doesn't pause straight away


def test_keep_printing_after_pause_resumes(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg)
    inc = _spaghetti_until_incident(mon, printer, grabber, clock, 32)
    advance(mon, clock, 13)
    assert printer.state == "PAUSED"
    # the paused alert doesn't offer "keep", but the command still means "false alarm, carry on"
    assert "vetoed" in mon.handle_reply("veto", inc.id)
    assert printer.calls[-1] == ("resume", 32) and mon.incident is None


def test_act_now_skips_reminders_and_stop_and_bad_ids(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg)
    inc = _spaghetti_until_incident(mon, printer, grabber, clock, 33)
    assert mon.handle_reply("veto", "wrong").startswith("ignored")
    assert mon.handle_reply("resume", inc.id).startswith("ignored")  # not paused yet
    assert mon.handle_reply("act", inc.id) == "paused"
    assert printer.calls == [("pause", 33)]
    before = len(_ntfy(sent))
    advance(mon, clock, 12)  # the 1-min reminder was skipped by "act"
    assert len(_ntfy(sent)) == before
    assert mon.handle_reply("stop", inc.id) == "stopped"
    assert ("stop", 33) in printer.calls and mon.incident is None
    assert mon.handle_reply("resume", inc.id).startswith("ignored")  # one-time id


def test_resume_reply_rearms_after_grace(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg)
    inc = _spaghetti_until_incident(mon, printer, grabber, clock, 34)
    mon.handle_reply("act", inc.id)
    assert mon.handle_reply("resume", inc.id) == "resumed"
    assert printer.state == "PRINTING" and mon.incident is None
    assert mon.job.rearm_at == clock.t + cfg.decision.resume_grace_s


def test_incident_closed_when_you_pause_at_the_printer(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg)
    _spaghetti_until_incident(mon, printer, grabber, clock, 35)
    printer.state = "PAUSED"  # knob on the printer / Prusa app
    advance(mon, clock, 1)
    assert mon.incident is None
    advance(mon, clock, 30)
    assert printer.calls == []


def test_schedule_picks_policy(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    esc = dict(ASK_FIRST, schedules=[{"name": "nap", "start": "13:00", "end": "16:00", "policy": "night"}])
    _arm(mon, cfg, esc)  # rig clock starts at 1970-01-12 13:46 UTC
    printer.state, printer.job_id = "PRINTING", 36
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 30)
    assert printer.calls == [("pause", 36)]
    first = _ntfy(sent)[-1]
    assert first.headers["Priority"] == "2" and "[night / nap]" in first.headers["Message"]
    assert mon.snapshot().policy == mon.snapshot().policy | {"policy": "night", "schedule": "nap"}


def test_notify_only_policy_cools_down(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg, dict(ASK_FIRST, default_policy="watch", snooze_s=300))
    printer.state, printer.job_id = "PRINTING", 37
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 12)
    assert mon.counters.failures == 1 and mon.incident is None  # notify-only closes right away
    assert printer.calls == []
    assert "does not act" in _ntfy(sent)[-1].headers["Message"]
    assert mon.job.rearm_at > clock.t  # cooldown = escalation.snooze_s
    advance(mon, clock, 20)
    assert mon.counters.failures == 1


def test_silent_step_and_dashboard_fallback_links(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    esc = {"default_policy": "p", "policies": {"p": {"steps": [{"at": 0, "notify": []}, {"at": 30, "buttons": ["keep", "act", "dashboard"]}, {"at": 60, "action": "pause", "notify": False}]}}}
    _arm(mon, cfg, esc, reply_topic="", webhook=False)
    inc = _spaghetti_until_incident(mon, printer, grabber, clock, 38)
    assert _ntfy(sent) == [] or all("hourglass" not in r.headers.get("Tags", "") for r in _ntfy(sent))
    advance(mon, clock, 3)
    alert = [r for r in _ntfy(sent) if "hourglass" in r.headers.get("Tags", "")][-1]
    acts = alert.headers["Actions"]
    assert f"http://watch.lan:8484/api/incident/veto?token=tok&id={inc.id}" in acts
    assert "view, Dashboard, http://watch.lan:8484" in acts
    n = len(sent)
    advance(mon, clock, 4)
    assert printer.calls == [("pause", 38)]
    assert len(sent) == n  # pause step was silent
