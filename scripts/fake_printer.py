#!/usr/bin/env python3
"""Tiny PrusaLink simulator for dry runs (no printer needed).

    python scripts/fake_printer.py --port 8081 --password test
    # then set printer.host: 127.0.0.1:8081, printer.auth: apikey, printer.password: test

Starts in PRINTING with job id 1. Honors pause/resume/stop from prusa-watch.
POST /sim/new_job starts a new job.
"""

import argparse

import uvicorn
from fastapi import FastAPI, Header, HTTPException, Response

state = {"state": "PRINTING", "job_id": 1, "progress": 0.0}


def build(password: str) -> FastAPI:
    app = FastAPI()

    def auth(key):
        if key != password:
            raise HTTPException(401)

    @app.get("/api/v1/status")
    def status(x_api_key: str = Header(None)):
        auth(x_api_key)
        if state["state"] == "PRINTING":
            state["progress"] = min(100.0, state["progress"] + 0.2)
        return {
            "printer": {"state": state["state"], "temp_nozzle": 215.0, "temp_bed": 60.0},
            "job": {"id": state["job_id"], "progress": state["progress"], "time_printing": 1234},
        }

    @app.get("/api/v1/job")
    def job(x_api_key: str = Header(None)):
        auth(x_api_key)
        return {"id": state["job_id"], "state": state["state"], "file": {"display_name": f"sim-part-{state['job_id']}.bgcode"}}

    @app.put("/api/v1/job/{jid}/pause")
    def pause(jid: int, x_api_key: str = Header(None)):
        auth(x_api_key)
        state["state"] = "PAUSED"
        print(">>> PAUSED by prusa-watch", flush=True)
        return Response(status_code=204)

    @app.put("/api/v1/job/{jid}/resume")
    def resume(jid: int, x_api_key: str = Header(None)):
        auth(x_api_key)
        state["state"] = "PRINTING"
        return Response(status_code=204)

    @app.delete("/api/v1/job/{jid}")
    def stop(jid: int, x_api_key: str = Header(None)):
        auth(x_api_key)
        state["state"] = "STOPPED"
        return Response(status_code=204)

    @app.post("/sim/new_job")
    def new_job():
        state.update(state="PRINTING", job_id=state["job_id"] + 1, progress=0.0)
        return state

    return app


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8081)
    ap.add_argument("--password", default="test")
    a = ap.parse_args()
    uvicorn.run(build(a.password), host="127.0.0.1", port=a.port, log_level="warning")
