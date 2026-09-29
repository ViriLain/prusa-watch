"""Per-print recording of what the detector saw, for tuning.

    state_dir/history/job-<id>.csv        one row per analyzed frame
    state_dir/history/job-<id>.json       job name + when recording started
    state_dir/frames/job-<id>/*.jpg       annotated frames where the model saw something

The dashboard's score history lives in memory and is gone once the process
stops; these files are what you look at after a test print (or a false alarm)
to decide whether to change `decision.sensitivity` or `camera.roi`.

Recording must never take the monitor down: every write is best-effort and
logs on failure.
"""

from __future__ import annotations

import csv
import json
import logging
import shutil
from datetime import datetime
from pathlib import Path

from .config import RecordingConfig

log = logging.getLogger(__name__)

COLUMNS = [
    "time", "ts", "frame", "progress", "boxes", "p", "max_conf",
    "ewm", "baseline", "short_mean", "score", "verdict", "inference_ms", "frame_file",
]


def _job_key(job_id) -> str:
    return f"job-{job_id if job_id is not None else 'unknown'}"


class Recorder:
    def __init__(self, cfg: RecordingConfig, state_dir: str | Path):
        self.cfg = cfg
        self.history_dir = Path(state_dir) / "history"
        self.frames_dir = Path(state_dir) / "frames"
        self.key: str | None = None
        self.frames_saved = 0

    @property
    def csv_path(self) -> Path | None:
        return self.history_dir / f"{self.key}.csv" if self.key else None

    @property
    def job_frames_dir(self) -> Path | None:
        return self.frames_dir / self.key if self.key else None

    def start_job(self, job_id, job_name: str | None, now: float) -> None:
        """Called when a new print is detected (also after a restart mid-print: files are appended to)."""
        self.key = _job_key(job_id)
        self.frames_saved = 0
        try:
            if self.cfg.history:
                self.history_dir.mkdir(parents=True, exist_ok=True)
                meta = self.history_dir / f"{self.key}.json"
                if not meta.exists():
                    meta.write_text(json.dumps({"job_id": job_id, "job_name": job_name, "started": datetime.fromtimestamp(now).isoformat(timespec="seconds")}, indent=2))
            if self.cfg.frames and self.job_frames_dir.exists():
                self.frames_saved = sum(1 for _ in self.job_frames_dir.glob("*.jpg"))
            self._prune()
        except OSError as exc:
            log.warning("Recording: could not start %s: %s", self.key, exc)

    def record(
        self,
        now: float,
        frame_num: int,
        progress: float | None,
        confidences: list[float],
        ewm: float,
        baseline: float,
        short_mean: float,
        score: float,
        verdict: str,
        inference_ms: float,
        annotated_jpeg: bytes | None,
    ) -> str | None:
        """Append one analyzed frame. Returns the saved frame's file name, if any."""
        if self.key is None:
            return None
        p = float(sum(confidences))
        frame_file = None
        if (
            self.cfg.frames
            and annotated_jpeg
            and self.frames_saved < self.cfg.max_frames_per_job
            and (verdict != "ok" or p >= self.cfg.frame_min_p)
        ):
            stamp = datetime.fromtimestamp(now).strftime("%Y%m%d-%H%M%S")
            frame_file = f"{stamp}_f{frame_num:05d}_{verdict}_p{p:.2f}.jpg"
            try:
                self.job_frames_dir.mkdir(parents=True, exist_ok=True)
                (self.job_frames_dir / frame_file).write_bytes(annotated_jpeg)
                self.frames_saved += 1
            except OSError as exc:
                log.warning("Recording: could not save frame: %s", exc)
                frame_file = None
        if self.cfg.history:
            row = [
                datetime.fromtimestamp(now).isoformat(timespec="seconds"),
                f"{now:.1f}",
                frame_num,
                "" if progress is None else f"{progress:.1f}",
                len(confidences),
                f"{p:.4f}",
                f"{max(confidences, default=0.0):.4f}",
                f"{ewm:.4f}",
                f"{baseline:.4f}",
                f"{short_mean:.4f}",
                f"{score:.4f}",
                verdict,
                f"{inference_ms:.0f}",
                frame_file or "",
            ]
            try:
                self.history_dir.mkdir(parents=True, exist_ok=True)
                path = self.csv_path
                new = not path.exists()
                with open(path, "a", newline="", encoding="utf-8") as fh:
                    w = csv.writer(fh)
                    if new:
                        w.writerow(COLUMNS)
                    w.writerow(row)
            except OSError as exc:
                log.warning("Recording: could not write history: %s", exc)
        return frame_file

    def _prune(self) -> None:
        keep = self.cfg.keep_jobs
        if keep <= 0:
            return
        jobs: dict[str, float] = {}
        for p in list(self.history_dir.glob("job-*.csv")) + list(self.history_dir.glob("job-*.json")):
            jobs[p.stem] = max(jobs.get(p.stem, 0.0), p.stat().st_mtime)
        if self.frames_dir.exists():
            for d in self.frames_dir.glob("job-*"):
                if d.is_dir():
                    jobs[d.name] = max(jobs.get(d.name, 0.0), d.stat().st_mtime)
        jobs.pop(self.key, None)  # never prune the print we're recording
        old = sorted(jobs, key=jobs.get, reverse=True)[max(0, keep - 1):]
        for key in old:
            for p in (self.history_dir / f"{key}.csv", self.history_dir / f"{key}.json"):
                p.unlink(missing_ok=True)
            shutil.rmtree(self.frames_dir / key, ignore_errors=True)
        if old:
            log.info("Recording: pruned %d old job(s)", len(old))


def summarize(csv_path: str | Path) -> dict:
    """Headline numbers for one recorded print (used by `prusa-watch report`)."""
    with open(csv_path, newline="", encoding="utf-8") as fh:
        rows = list(csv.DictReader(fh))
    if not rows:
        return {"frames": 0}
    ps = [float(r["p"]) for r in rows]
    scores = [float(r["score"]) for r in rows]
    peak = max(range(len(rows)), key=lambda i: ps[i])
    first = {}
    for r in rows:
        first.setdefault(r["verdict"], r["time"])
    return {
        "frames": len(rows),
        "from": rows[0]["time"],
        "to": rows[-1]["time"],
        "peak_p": ps[peak],
        "peak_p_at": rows[peak]["time"],
        "peak_score": max(scores),
        "baseline": float(rows[-1]["baseline"]),
        "verdicts": {v: sum(1 for r in rows if r["verdict"] == v) for v in ("ok", "warning", "failure")},
        "first_warning": first.get("warning"),
        "first_failure": first.get("failure"),
        "frames_saved": sum(1 for r in rows if r["frame_file"]),
    }
