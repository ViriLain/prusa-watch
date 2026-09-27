from fastapi.testclient import TestClient

from conftest import solid
from test_monitor import advance, rig  # noqa: F401  (fixture reuse)
from prusa_watch.web import create_app


def test_dashboard_state_frames_metrics_and_auth(rig):  # noqa: F811
    mon, printer, grabber, clock, sent, cfg = rig
    printer.state, printer.job_id = "PRINTING", 21
    advance(mon, clock, 5)
    c = TestClient(create_app(mon))

    r = c.get("/")
    assert r.status_code == 200 and "prusa-watch" in r.text

    s = c.get("/api/state").json()
    assert s["printer_state"] == "PRINTING" and s["job"]["job_id"] == 21
    assert s["auth_required"] is True and len(s["history"]) >= 1

    assert c.get("/frame.jpg").content[:2] == b"\xff\xd8"
    assert c.get("/raw.jpg").content[:2] == b"\xff\xd8"

    m = c.get("/metrics").text
    assert 'prusa_watch_printer_state{printer="core-one",state="PRINTING"} 1' in m
    assert "prusa_watch_frames_analyzed_total" in m

    # control endpoints require the token
    assert c.post("/api/pause").status_code == 401
    assert c.post("/api/pause?token=wrong").status_code == 401
    assert c.post("/api/pause?token=tok").status_code == 200
    assert printer.state == "PAUSED"
    assert c.post("/api/resume", headers={"X-Token": "tok"}).json()["result"] == "resumed"
    assert c.post("/api/mute?token=tok").status_code == 200 and mon.job.muted

    grabber.image = solid(255)
    t = c.post("/api/test?token=tok").json()
    assert len(t["detections"]) == 2
    assert c.get("/test.jpg").content[:2] == b"\xff\xd8"
    assert c.get("/healthz").status_code == 200


def test_pending_endpoints(rig):  # noqa: F811
    from test_monitor import _arm, _spaghetti_until_pending

    mon, printer, grabber, clock, sent, cfg = rig
    c = TestClient(create_app(mon))
    assert c.post("/api/pending/veto?token=tok").status_code == 409  # nothing pending
    assert c.post("/api/pending/nuke?token=tok").status_code == 404
    _arm(mon, cfg)
    p = _spaghetti_until_pending(mon, printer, grabber, clock, 40)
    s = c.get("/api/state").json()
    assert s["pending"]["id"] == p.id and 0 < s["pending"]["seconds_left"] <= 120
    assert s["policy"]["veto_window_s"] == 120
    assert "prusa_watch_pending_seconds_left" in c.get("/metrics").text
    assert c.post(f"/api/pending/veto?id={p.id}").status_code == 401  # token still required
    r = c.post("/api/pending/veto?token=tok")  # dashboard: no id -> current pending
    assert r.status_code == 200 and r.json()["result"].startswith("vetoed")
    assert c.get("/api/state").json()["pending"] is None
