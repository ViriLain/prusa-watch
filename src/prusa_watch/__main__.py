"""Entry point.

    prusa-watch run    [-c config.yaml]   # start monitor + dashboard
    prusa-watch check  [-c config.yaml]   # verify printer, camera, model and escalation, then exit
    prusa-watch config [-c config.yaml]   # print the effective config (defaults + file + env), secrets masked
    prusa-watch config --defaults         # print the built-in defaults only
    prusa-watch fetch-model [--force]     # download the detection model (run/check do this if it's missing)

A `.env` next to the config file (or in the current directory) is loaded
automatically; variables already set in the shell win.
"""

from __future__ import annotations

import argparse
import logging
import os
import signal
import sys
import time
from pathlib import Path

from .config import Config, effective_config, load_config

log = logging.getLogger("prusa_watch")


def _setup_logging(level: str) -> None:
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s %(levelname)-7s %(name)s: %(message)s",
    )
    logging.getLogger("httpx").setLevel(logging.WARNING)


def _ensure_model(path: str, force: bool = False) -> bool:
    """Download the model if it's missing (or force). Returns False on failure."""
    from .model import ModelDownloadError, download_model

    if Path(path).exists() and not force:
        return True
    try:
        download_model(path, force=force)
        return True
    except ModelDownloadError as exc:
        print(f"{exc}\nRetry with:  prusa-watch fetch-model", file=sys.stderr)
        return False


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
    c = cfg.camera
    g = FrameGrabber(c.url, c.transport, c.reconnect_backoff_s, c.open_timeout_s, c.read_timeout_s)
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

    print("[escalation]")
    from .escalation import PolicyResolver, parse_escalation

    esc = parse_escalation(cfg.escalation)
    pol, sched = PolicyResolver(esc, cfg.timezone).resolve(time.time())
    for name, p in esc.policies.items():
        marks = " (default)" if name == esc.default_policy else ""
        marks += " <- active now" + (f" via schedule '{sched}'" if sched else "") if name == pol.name else ""
        print(f"  policy {name}{marks}")
        for st in p.steps:
            print(f"    at {st.at:>6.0f}s  action={st.action or '-':5}  notify={st.notify if st.notify is not None else 'all'}  prio={st.priority}")
    if cfg.notify.ntfy.topic and not cfg.notify.ntfy.reply_topic:
        print("  note: notify.ntfy.reply_topic not set - buttons only work on your LAN (via web.public_url)")
    channels = [c for c, on in (("ntfy", cfg.notify.ntfy.topic), ("discord", cfg.notify.discord.webhook_url), ("webhook", cfg.notify.webhook.url)) if on]
    if not channels:
        print("  WARNING: no notification channel configured - policies that wait for your answer will just act late")

    print(f"[model] {cfg.detector.model_path}")
    if not _ensure_model(cfg.detector.model_path):
        ok = False
        print("  FAIL model missing and download failed")
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

    if not _ensure_model(cfg.detector.model_path):
        return 1
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


def cmd_config(args) -> int:
    import yaml

    if args.defaults:
        cfg = Config()
    else:
        try:
            cfg = load_config(args.config)
        except (ValueError, FileNotFoundError) as exc:
            print(exc, file=sys.stderr)
            return 2
    print(yaml.safe_dump(effective_config(cfg, redact=not args.show_secrets), sort_keys=False, allow_unicode=True), end="")
    if not args.defaults:
        try:
            cfg.validate()
        except ValueError as exc:
            print(f"\n# {exc}".replace("\n", "\n# "), file=sys.stderr)
            return 2
    return 0


def cmd_fetch_model(args) -> int:
    path = Config().detector.model_path
    if Path(args.config).exists():
        try:
            path = load_config(args.config).detector.model_path
        except ValueError as exc:
            print(exc, file=sys.stderr)
            return 2
    if Path(path).exists() and not args.force:
        print(f"{path} already exists ({Path(path).stat().st_size / 1e6:.1f} MB); use --force to re-download")
        return 0
    return 0 if _ensure_model(path, force=args.force) else 1


def _load_env_files(args) -> None:
    from .dotenv import load_dotenv

    if args.env_file:
        if not Path(args.env_file).is_file():
            print(f"--env-file {args.env_file}: not found", file=sys.stderr)
            sys.exit(2)
        candidates = [Path(args.env_file)]
    else:
        candidates = [Path(args.config).resolve().parent / ".env", Path.cwd() / ".env"]
    for p in load_dotenv(candidates):
        print(f"prusa-watch: loaded {p}", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="prusa-watch", description="AI spaghetti detection for Prusa printers + Buddy3D camera")
    ap.add_argument("command", nargs="?", default="run", choices=["run", "check", "config", "fetch-model"])
    ap.add_argument("-c", "--config", default=os.environ.get("PRUSA_WATCH_CONFIG", "config.yaml"))
    ap.add_argument("--env-file", default=None, help="load this .env instead of looking next to the config / in the cwd")
    ap.add_argument("--defaults", action="store_true", help="config: show built-in defaults only")
    ap.add_argument("--show-secrets", action="store_true", help="config: don't mask passwords/tokens/topics")
    ap.add_argument("--force", action="store_true", help="fetch-model: re-download even if present")
    args = ap.parse_args(argv)

    _load_env_files(args)
    if args.command == "config":
        return cmd_config(args)
    if args.command == "fetch-model":
        return cmd_fetch_model(args)
    try:
        cfg = load_config(args.config)
    except FileNotFoundError as exc:
        print(f"{exc}\nCreate one with:  cp config.example.yaml {args.config}", file=sys.stderr)
        return 2
    except ValueError as exc:
        print(exc, file=sys.stderr)
        return 2
    _setup_logging(cfg.log_level)
    try:
        cfg.validate()
    except ValueError as exc:
        print(exc, file=sys.stderr)
        return 2
    return cmd_check(cfg) if args.command == "check" else cmd_run(cfg)


if __name__ == "__main__":
    sys.exit(main())
