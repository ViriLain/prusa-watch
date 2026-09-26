"""Entry point.

    prusa-watch run   [-c config.yaml]   # start monitor + dashboard
    prusa-watch check [-c config.yaml]   # verify printer, camera and model, then exit
"""

from __future__ import annotations

import argparse
import logging
import os
import signal
import sys
import time
from pathlib import Path

from .config import Config, load_config

log = logging.getLogger("prusa_watch")


def _setup_logging(level: str) -> None:
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s %(levelname)-7s %(name)s: %(message)s",
    )
    logging.getLogger("httpx").setLevel(logging.WARNING)


def _require_model(cfg: Config) -> None:
    if not Path(cfg.detector.model_path).exists():
        sys.exit(
            f"Model not found at {cfg.detector.model_path}.\n"
            "Download it with:  python scripts/fetch_model.py\n"
            "(the Docker image does this at build time)"
        )


def cmd_check(cfg: Config) -> int:
    from .camera import FrameGrabber
    from .detector import SpaghettiDetector
    from .prusalink import PrusaLink, PrusaLinkError

    ok = True
    print(f"[printer] {cfg.printer.host} (auth={cfg.printer.auth})")
    try:
        pl = PrusaLink(cfg.printer.host, cfg.printer.password, cfg.printer.username, cfg.printer.auth, cfg.printer.scheme)
        st = pl.status()
        print(f"  OK  state={st.state} job_id={st.job_id} nozzle={st.temp_nozzle} bed={st.temp_bed}")
        if st.job_id is not None:
            print(f"  job: {pl.job_name()}")
    except PrusaLinkError as exc:
        ok = False
        print(f"  FAIL {exc}")

    print(f"[camera] {cfg.camera.url}")
    g = FrameGrabber(cfg.camera.url, cfg.camera.transport)
    g.start()
    deadline = time.time() + 20
    while time.time() < deadline and g.latest() is None:
        time.sleep(0.25)
    frame = g.latest()
    g.stop()
    if frame is None:
        ok = False
        print("  FAIL no frame within 20 s. Is RTSP enabled for the camera in the Prusa app? Is the IP right?")
    else:
        h, w = frame.image.shape[:2]
        print(f"  OK  {w}x{h}")
        Path(cfg.state_dir).mkdir(parents=True, exist_ok=True)
        import cv2

        p = Path(cfg.state_dir) / "check_frame.jpg"
        cv2.imwrite(str(p), frame.image)
        print(f"  saved {p}")

    print(f"[model] {cfg.detector.model_path}")
    if not Path(cfg.detector.model_path).exists():
        ok = False
        print("  FAIL missing; run: python scripts/fetch_model.py")
    else:
        try:
            det = SpaghettiDetector(cfg.detector.model_path, use_gpu=cfg.detector.use_gpu)
            if frame is not None:
                from .camera import crop_roi

                dets = det.detect(crop_roi(frame.image, cfg.camera.roi), cfg.detector.threshold, cfg.detector.nms)
                print(f"  OK  {len(dets)} boxes, sum p={sum(d.confidence for d in dets):.3f}, {det.last_inference_ms:.0f} ms")
            else:
                print("  OK  loaded")
        except Exception as exc:
            ok = False
            print(f"  FAIL {exc}")

    print("\nAll checks passed." if ok else "\nSome checks failed.")
    return 0 if ok else 1


def cmd_run(cfg: Config) -> int:
    import uvicorn

    from .monitor import Monitor
    from .web import create_app

    _require_model(cfg)
    monitor = Monitor(cfg)
    monitor.start()

    def _shutdown(*_):
        log.info("Shutting down")
        monitor.stop()
        sys.exit(0)

    signal.signal(signal.SIGTERM, _shutdown)

    if cfg.web.enabled:
        log.info("Dashboard on http://%s:%d", cfg.web.host, cfg.web.port)
        try:
            uvicorn.run(create_app(monitor), host=cfg.web.host, port=cfg.web.port, log_level="warning")
        finally:
            monitor.stop()
    else:
        try:
            while True:
                time.sleep(3600)
        except KeyboardInterrupt:
            monitor.stop()
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="prusa-watch", description="AI spaghetti detection for Prusa printers + Buddy3D camera")
    ap.add_argument("command", nargs="?", default="run", choices=["run", "check"])
    ap.add_argument("-c", "--config", default=os.environ.get("PRUSA_WATCH_CONFIG", "config.yaml"))
    args = ap.parse_args(argv)

    cfg = load_config(args.config)
    _setup_logging(cfg.log_level)
    try:
        cfg.validate()
    except ValueError as exc:
        print(exc, file=sys.stderr)
        return 2
    return cmd_check(cfg) if args.command == "check" else cmd_run(cfg)


if __name__ == "__main__":
    sys.exit(main())
