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

    sent = []

    def ntfy(request: httpx.Request):
        sent.append(request)
        return httpx.Response(200, json={"id": "x"})

    clock = Clock()
    printer = FakePrinter()
    grabber = FakeGrabber(clock)
    notifier = Notifier(cfg.notify, cfg.web.public_url, cfg.web.token, transport=httpx.MockTransport(ntfy))
    notifier.send = lambda e, blocking=False: Notifier.send(notifier, e, blocking=True)
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
    assert "/api/resume?token=tok" in r.headers["Actions"]

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
    assert "mute=1" in [r for r in sent if r.headers.get("Priority") == "5"][0].headers["Actions"]
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
    cfg.decision.action = "notify"
    printer.state, printer.job_id = "PRINTING", 3
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 30)
    assert printer.calls == []
    assert any(r.headers.get("Priority") == "5" for r in sent)
    assert sum(1 for r in sent if r.headers.get("Priority") == "5") == 1  # not spammed


def test_stop_mode(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    cfg.decision.action = "stop"
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
    msgs = [r.headers.get("Message", "") for r in sent if r.headers.get("Priority") == "5"]
    assert any("FAILED" in m for m in msgs)


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


# ---------------------------------------------------------------- veto window
from prusa_watch.policy import PolicyResolver  # noqa: E402


def _arm(mon, cfg, veto=120, reply_topic="reply-xyz", schedules=None, tz="UTC"):
    cfg.decision.veto_window_s = veto
    cfg.decision.schedules = schedules or []
    cfg.notify.ntfy.reply_topic = reply_topic
    cfg.timezone = tz
    mon.policy = PolicyResolver(cfg.decision, tz)


def _spaghetti_until_pending(mon, printer, grabber, clock, job):
    printer.state, printer.job_id = "PRINTING", job
    advance(mon, clock, 60)
    grabber.image = solid(255)
    for _ in range(40):
        advance(mon, clock, 1)
        if mon.pending:
            return mon.pending
    raise AssertionError("no pending action created")


def _by_kind(sent, tag):
    return [r for r in sent if tag in r.headers.get("Tags", "")]


def test_veto_window_fires_after_silence(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg, veto=120)
    p = _spaghetti_until_pending(mon, printer, grabber, clock, 30)
    assert printer.calls == []  # nothing yet: waiting for the human

    (alert,) = _by_kind(sent, "hourglass")
    assert alert.headers["Priority"] == "5"
    assert "pausing in 2:00" in alert.headers["Title"]
    acts = alert.headers["Actions"]
    assert f"https://ntfy.example/reply-xyz, method=POST, body=veto {p.id}" in acts
    assert f"body=act {p.id}" in acts and f"body=stop {p.id}" in acts
    assert alert.content[:2] == b"\xff\xd8"

    advance(mon, clock, 11)  # 110 s: still inside the window
    assert printer.calls == [] and mon.snapshot().pending["seconds_left"] <= 10
    advance(mon, clock, 2)
    assert printer.calls == [("pause", 30)] and mon.pending is None
    assert mon.counters.auto_actions == 1
    fail = [r for r in sent if r.headers.get("Tags", "").startswith("rotating_light")]
    assert "no response within 120 s" in fail[-1].headers["Message"]


def test_keep_printing_snoozes_then_rearms(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg, veto=120)
    cfg.decision.veto_snooze_s = 600
    p = _spaghetti_until_pending(mon, printer, grabber, clock, 31)
    assert mon.handle_reply("veto", p.id).startswith("vetoed")
    assert mon.counters.vetoes == 1
    advance(mon, clock, 50)  # 500 s of continued "spaghetti" inside the snooze
    assert printer.calls == [] and mon.pending is None
    warnings_before = len(_by_kind(sent, "warning,printer"))
    advance(mon, clock, 15)  # snooze over -> re-armed: warns again
    assert len(_by_kind(sent, "warning,printer")) == warnings_before + 1
    # Obico semantics: the per-print mean absorbed the mess while snoozed, so it
    # only asks again if things get worse. Emulate "worse" with sensitivity.
    cfg.decision.sensitivity = 3.0
    advance(mon, clock, 5)
    assert mon.pending is not None and mon.pending.id != p.id
    assert printer.calls == []  # asks again rather than pausing straight away
    assert len(_by_kind(sent, "hourglass")) == 2


def test_veto_with_zero_snooze_mutes_print(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg)
    cfg.decision.veto_snooze_s = 0
    p = _spaghetti_until_pending(mon, printer, grabber, clock, 32)
    assert "muted" in mon.handle_reply("veto", p.id)
    advance(mon, clock, 200)
    assert printer.calls == [] and mon.pending is None


def test_act_now_and_stop_and_bad_ids(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg)
    p = _spaghetti_until_pending(mon, printer, grabber, clock, 33)
    assert mon.handle_reply("veto", "wrong-id").startswith("ignored")
    assert mon.handle_reply("act", "wrong-id").startswith("ignored")
    assert mon.pending is p
    assert mon.handle_reply("act", p.id) == "paused"
    assert printer.calls == [("pause", 33)]
    assert mon.handle_reply("veto", p.id).startswith("ignored")  # one-time id

    printer.resume(33)
    printer.state, printer.job_id = "FINISHED", 33
    advance(mon, clock, 2)
    grabber.image = solid(0)
    p2 = _spaghetti_until_pending(mon, printer, grabber, clock, 34)
    assert mon.handle_reply("stop", p2.id) == "stopped"
    assert ("stop", 34) in printer.calls


def test_pending_cancelled_if_you_pause_at_the_printer(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg)
    _spaghetti_until_pending(mon, printer, grabber, clock, 35)
    printer.state = "PAUSED"  # knob on the printer / Prusa app
    advance(mon, clock, 1)
    assert mon.pending is None
    advance(mon, clock, 20)
    assert printer.calls == []


def test_night_schedule_pauses_immediately_and_quietly(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    # rig clock starts at 1970-01-12 13:46 UTC
    _arm(mon, cfg, veto=120, schedules=[{"name": "nap", "start": "13:00", "end": "16:00", "veto_window_s": 0, "quiet": True}])
    printer.state, printer.job_id = "PRINTING", 36
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 30)
    assert printer.calls == [("pause", 36)]
    assert _by_kind(sent, "hourglass") == []
    fail = [r for r in sent if r.headers.get("Tags", "").startswith("rotating_light")]
    assert fail[0].headers["Priority"] == "2" and "schedule 'nap'" in fail[0].headers["Message"]
    assert mon.snapshot().policy["rule"] == "nap"


def test_pending_without_reply_topic_uses_dashboard_links(rig):
    mon, printer, grabber, clock, sent, cfg = rig
    _arm(mon, cfg, reply_topic="")
    mon.notifier.cfg.ntfy.reply_topic = ""
    p = _spaghetti_until_pending(mon, printer, grabber, clock, 37)
    (alert,) = _by_kind(sent, "hourglass")
    assert f"http://watch.lan:8484/api/pending/veto?token=tok&id={p.id}" in alert.headers["Actions"]
