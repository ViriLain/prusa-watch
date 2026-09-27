#!/usr/bin/env python3
"""Download Obico's open-source failure-detection model (ONNX).

Kept for the Dockerfile and old instructions; `prusa-watch fetch-model` does the
same thing, and `prusa-watch run` / `check` download the model if it's missing.
Source of truth for the URL: obico-server ml_api/model/model-weights.onnx.url.
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

from prusa_watch.model import DEFAULT_URL, ModelDownloadError, download_model


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=os.environ.get("MODEL_URL", DEFAULT_URL))
    ap.add_argument("--out", default=os.environ.get("MODEL_PATH", "models/model-weights.onnx"))
    ap.add_argument("--force", action="store_true")
    args = ap.parse_args()

    out = Path(args.out)
    if out.exists() and not args.force:
        print(f"{out} already exists ({out.stat().st_size / 1e6:.1f} MB); use --force to re-download")
        return 0
    try:
        download_model(out, url=args.url, force=args.force)
    except ModelDownloadError as exc:
        print(exc, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
