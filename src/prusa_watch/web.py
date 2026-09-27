"""Local dashboard + control API + Prometheus metrics."""

from __future__ import annotations

import hmac
import time
from dataclasses import asdict
from importlib import resources

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import HTMLResponse, JSONResponse, PlainTextResponse, Response

from .monitor import Monitor
from .prusalink import PrusaLinkError


def create_app(monitor: Monitor) -> FastAPI:
    app = FastAPI(title="prusa-watch", docs_url=None, redoc_url=None)
    token = monitor.cfg.web.token
    state = {"test_jpeg": None}

    def require_token(request: Request) -> None:
        if not token:
            return
        supplied = request.query_params.get("token") or request.headers.get("X-Token") or ""
        if not hmac.compare_digest(supplied, token):
            raise HTTPException(status_code=401, detail="bad or missing token")

    def jpeg(data: bytes | None) -> Response:
        if not data:
            raise HTTPException(status_code=404, detail="no frame yet")
        return Response(content=data, media_type="image/jpeg", headers={"Cache-Control": "no-store"})

    @app.get("/", response_class=HTMLResponse)
    def index() -> str:
        return resources.files("prusa_watch").joinpath("static/index.html").read_text(encoding="utf-8")

    @app.get("/healthz")
    def healthz():
        snap = monitor.snapshot()
        ok = snap.printer_reachable
        return JSONResponse({"ok": ok}, status_code=200 if ok else 503)

    @app.get("/api/state")
    def api_state():
        snap = monitor.snapshot()
        d = asdict(snap)
        d["printer_name"] = monitor.cfg.printer.name
        d["sensitivity"] = monitor.cfg.decision.sensitivity
        d["auth_required"] = bool(token)
        d["history"] = [asdict(h) for h in monitor.history_points()]
        d["counters"] = asdict(monitor.counters)
        d["server_time"] = time.time()
        return d

    @app.get("/frame.jpg")
    def frame():
        return jpeg(monitor.annotated_jpeg())

    @app.get("/raw.jpg")
    def raw():
        return jpeg(monitor.raw_jpeg())

    @app.get("/test.jpg")
    def test_jpg():
        return jpeg(state["test_jpeg"])

    def control(fn, request: Request):
        require_token(request)
        try:
            return {"ok": True, "result": fn()}
        except PrusaLinkError as exc:
            raise HTTPException(status_code=502, detail=str(exc)) from exc

    @app.post("/api/pause")
    def api_pause(request: Request):
        return control(monitor.pause, request)

    @app.post("/api/resume")
    def api_resume(request: Request):
        mute = request.query_params.get("mute", "").lower() in ("1", "true", "yes")
        return control(lambda: monitor.resume(mute=mute), request)

    @app.post("/api/stop")
    def api_stop(request: Request):
        return control(monitor.stop_print, request)

    @app.post("/api/mute")
    def api_mute(request: Request):
        return control(lambda: monitor.set_muted(True) or "muted", request)

    @app.post("/api/unmute")
    def api_unmute(request: Request):
        return control(lambda: monitor.set_muted(False) or "unmuted", request)

    @app.post("/api/incident/{cmd}")
    def api_incident(cmd: str, request: Request):
        if cmd not in ("veto", "act", "stop", "resume", "mute"):
            raise HTTPException(status_code=404, detail="unknown command")
        require_token(request)
        iid = request.query_params.get("id")
        if not iid:  # dashboard buttons act on whatever incident is open right now
            inc = monitor.incident
            iid = inc.id if inc else ""
        result = monitor.handle_reply(cmd, iid)
        if result.startswith("ignored"):
            raise HTTPException(status_code=409, detail=result)
        if result.startswith("failed"):
            raise HTTPException(status_code=502, detail=result)
        return {"ok": True, "result": result}

    @app.post("/api/test")
    def api_test(request: Request):
        require_token(request)
        res = monitor.test_detection()
        if res is None:
            raise HTTPException(status_code=404, detail="no camera frame yet")
        dets, img = res
        state["test_jpeg"] = img
        return {"detections": [d.as_list() for d in dets], "sum_p": sum(d.confidence for d in dets)}

    @app.get("/metrics", response_class=PlainTextResponse)
    def metrics() -> str:
        s = monitor.snapshot()
        c = monitor.counters
        name = monitor.cfg.printer.name
        lbl = f'printer="{name}"'
        states = ["IDLE", "BUSY", "PRINTING", "PAUSED", "FINISHED", "STOPPED", "ERROR", "ATTENTION", "READY", "UNKNOWN"]
        lines = [
            "# HELP prusa_watch_score Normalized failure score (0-1; >0.33 warning band, >0.66 failure band)",
            "# TYPE prusa_watch_score gauge",
            f"prusa_watch_score{{{lbl}}} {s.score:.4f}",
            "# TYPE prusa_watch_current_p gauge",
            f"prusa_watch_current_p{{{lbl}}} {s.current_p:.4f}",
            "# TYPE prusa_watch_ewm_mean gauge",
            f"prusa_watch_ewm_mean{{{lbl}}} {s.ewm_mean:.4f}",
            "# TYPE prusa_watch_baseline gauge",
            f"prusa_watch_baseline{{{lbl}}} {s.baseline:.4f}",
            "# TYPE prusa_watch_inference_ms gauge",
            f"prusa_watch_inference_ms{{{lbl}}} {s.inference_ms:.1f}",
            "# TYPE prusa_watch_camera_connected gauge",
            f"prusa_watch_camera_connected{{{lbl}}} {int(s.camera_connected)}",
            "# TYPE prusa_watch_frame_age_seconds gauge",
            f"prusa_watch_frame_age_seconds{{{lbl}}} {s.frame_age_s if s.frame_age_s is not None else 'NaN'}",
            "# TYPE prusa_watch_printer_reachable gauge",
            f"prusa_watch_printer_reachable{{{lbl}}} {int(s.printer_reachable)}",
            "# HELP prusa_watch_incident_open 1 while an escalation incident is open",
            "# TYPE prusa_watch_incident_open gauge",
            f"prusa_watch_incident_open{{{lbl}}} {int(s.incident is not None)}",
            "# HELP prusa_watch_next_action_seconds Seconds until the incident's next pause/stop step (-1 = none)",
            "# TYPE prusa_watch_next_action_seconds gauge",
            f"prusa_watch_next_action_seconds{{{lbl}}} "
            f"{s.incident['next_action_in_s'] if s.incident and s.incident['next_action_in_s'] is not None else -1:.0f}",
            "# TYPE prusa_watch_printer_state gauge",
        ]
        lines += [f'prusa_watch_printer_state{{{lbl},state="{st}"}} {int(s.printer_state == st)}' for st in states]
        for field_name, help_text in (
            ("frames_analyzed", "Frames run through the model"),
            ("warnings", "Warning notifications sent"),
            ("failures", "Failure verdicts"),
            ("pauses", "Prints paused by prusa-watch"),
            ("stops", "Prints stopped by prusa-watch"),
            ("printer_errors", "PrusaLink request failures"),
            ("vetoes", "Pending actions cancelled by the user (Keep printing)"),
            ("auto_actions", "Pending actions that fired because nobody responded"),
        ):
            lines += [
                f"# HELP prusa_watch_{field_name}_total {help_text}",
                f"# TYPE prusa_watch_{field_name}_total counter",
                f"prusa_watch_{field_name}_total{{{lbl}}} {getattr(c, field_name)}",
            ]
        return "\n".join(lines) + "\n"

    return app
