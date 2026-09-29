"""Per-print recording: history CSV, saved frames, restart append, pruning, failure isolation."""

import csv
import json
import os
from pathlib import Path

from conftest import solid
from test_monitor import advance, rig  # noqa: F401  (fixture)

from prusa_watch.config import RecordingConfig
from prusa_watch.recording import COLUMNS, Recorder


def _rows(path):
    with open(path, newline="") as fh:
        return list(csv.DictReader(fh))


def test_print_is_recorded_frame_by_frame(rig):  # noqa: F811
    mon, printer, grabber, clock, sent, cfg = rig
    printer.state, printer.job_id = "PRINTING", 417
    advance(mon, clock, 40)  # clean
    grabber.image = solid(255)  # spaghetti
    advance(mon, clock, 20)

    hist = Path(cfg.state_dir) / "history"
    rows = _rows(hist / "job-417.csv")
    assert list(rows[0].keys()) == COLUMNS
    assert len(rows) == mon.counters.frames_analyzed
    assert [int(r["frame"]) for r in rows] == list(range(1, len(rows) + 1))
    assert all(float(r["p"]) < 0.3 for r in rows[:40]), "clean frames"
    assert max(float(r["p"]) for r in rows) > 1.0
    assert {"warning", "failure"} & {r["verdict"] for r in rows}
    assert json.loads((hist / "job-417.json").read_text())["job_name"] == "part-417.bgcode"

    frames_dir = Path(cfg.state_dir) / "frames" / "job-417"
    saved = sorted(p.name for p in frames_dir.glob("*.jpg"))
    referenced = sorted(r["frame_file"] for r in rows if r["frame_file"])
    assert saved == referenced and saved, "every saved frame is referenced by its CSV row"
    for r in rows:
        if r["frame_file"]:
            assert float(r["p"]) >= 0.3 or r["verdict"] != "ok"
    assert (frames_dir / saved[0]).read_bytes()[:2] == b"\xff\xd8"


def test_frame_cap_and_disabled_outputs(rig):  # noqa: F811
    mon, printer, grabber, clock, sent, cfg = rig
    cfg.recording.max_frames_per_job = 3
    printer.state, printer.job_id = "PRINTING", 9
    grabber.image = solid(255)
    advance(mon, clock, 40)
    assert len(list((Path(cfg.state_dir) / "frames" / "job-9").glob("*.jpg"))) == 3

    cfg.recording.history = False
    cfg.recording.frames = False
    printer.job_id = 10
    advance(mon, clock, 10)
    assert not (Path(cfg.state_dir) / "history" / "job-10.csv").exists()
    assert not (Path(cfg.state_dir) / "frames" / "job-10").exists()


def test_restart_mid_print_appends(tmp_path):
    cfg = RecordingConfig()
    jpeg = b"\xff\xd8fake"
    r1 = Recorder(cfg, tmp_path)
    r1.start_job(5, "a.bgcode", 1000.0)
    r1.record(1000.0, 1, 10.0, [0.5], 0.1, 0.0, 0.0, 0.2, "ok", 90, jpeg)
    r2 = Recorder(cfg, tmp_path)  # process restarted, same print
    r2.start_job(5, "a.bgcode", 2000.0)
    assert r2.frames_saved == 1
    r2.record(2000.0, 1, 11.0, [], 0.1, 0.0, 0.0, 0.0, "ok", 90, jpeg)
    rows = _rows(tmp_path / "history" / "job-5.csv")
    assert len(rows) == 2  # one header, both rows
    from datetime import datetime

    started = json.loads((tmp_path / "history" / "job-5.json").read_text())["started"]
    assert started == datetime.fromtimestamp(1000.0).isoformat(timespec="seconds"), "restart keeps the original start"


def test_old_jobs_are_pruned(tmp_path):
    cfg = RecordingConfig(keep_jobs=2)
    r = Recorder(cfg, tmp_path)
    for i, job in enumerate([1, 2, 3, 4]):
        r.start_job(job, None, 1000.0 + i)
        r.record(1000.0 + i, 1, None, [0.9], 0, 0, 0, 0.5, "warning", 50, b"\xff\xd8x")
        for p in list((tmp_path / "history").glob(f"job-{job}.*")) + [tmp_path / "frames" / f"job-{job}"]:
            os.utime(p, (1000 + i, 1000 + i))
    r.start_job(5, None, 2000.0)
    left = sorted(p.name for p in (tmp_path / "history").glob("*.csv"))
    assert left == ["job-4.csv"], "keep_jobs=2 -> the current print plus the newest older one"
    assert sorted(p.name for p in (tmp_path / "frames").iterdir()) == ["job-4"]


def test_recording_failure_never_stops_monitoring(rig, caplog):  # noqa: F811
    mon, printer, grabber, clock, sent, cfg = rig
    (Path(cfg.state_dir) / "history").write_text("not a directory")
    (Path(cfg.state_dir) / "frames").write_text("not a directory")
    printer.state, printer.job_id = "PRINTING", 7
    advance(mon, clock, 60)
    grabber.image = solid(255)
    advance(mon, clock, 30)
    assert ("pause", 7) in printer.calls, "detection and escalation still work"
    assert any("Recording:" in r.message for r in caplog.records)


def test_report_command_summarizes_latest_print(rig, capsys, monkeypatch):  # noqa: F811
    from prusa_watch.__main__ import main

    mon, printer, grabber, clock, sent, cfg = rig
    printer.state, printer.job_id = "PRINTING", 417
    advance(mon, clock, 40)
    grabber.image = solid(255)
    advance(mon, clock, 20)

    conf = Path(cfg.state_dir).parent / "c.yaml"
    conf.write_text(f"state_dir: '{cfg.state_dir}'\n")
    assert main(["report", "-c", str(conf)]) == 0
    out = capsys.readouterr().out
    assert "job-417  part-417.bgcode" in out
    assert "peak p" in out and "first warn" in out
    assert main(["report", "999", "-c", str(conf)]) == 1
