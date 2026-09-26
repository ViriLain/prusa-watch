#!/usr/bin/env python3
"""Download Obico's open-source failure-detection model (ONNX) into ./models.

Source of truth for the URL: obico-server ml_api/model/model-weights.onnx.url
(AGPL-3.0, https://github.com/TheSpaghettiDetective/obico-server).
"""

from __future__ import annotations

import argparse
import hashlib
import os
import sys
import urllib.request
from pathlib import Path

DEFAULT_URL = "https://tsd-pub-static.s3.amazonaws.com/ml-models/model-weights-5a6b1be1fa.onnx"


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
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_suffix(".part")

    print(f"Downloading {args.url}")
    h = hashlib.sha256()
    with urllib.request.urlopen(args.url, timeout=60) as r, open(tmp, "wb") as fh:
        total = int(r.headers.get("Content-Length") or 0)
        done = 0
        while chunk := r.read(1 << 20):
            fh.write(chunk)
            h.update(chunk)
            done += len(chunk)
            if total:
                print(f"\r  {done / 1e6:6.1f} / {total / 1e6:.1f} MB", end="", flush=True)
    print()
    if done < 1_000_000:
        tmp.unlink(missing_ok=True)
        print("Download looks wrong (under 1 MB). Aborting.", file=sys.stderr)
        return 1

    try:
        import onnxruntime as ort

        sess = ort.InferenceSession(str(tmp), providers=["CPUExecutionProvider"])
        shapes = [i.shape for i in sess.get_inputs()]
        print(f"  ONNX OK, inputs {shapes}")
    except ImportError:
        print("  (onnxruntime not installed; skipping load check)")
    tmp.replace(out)
    print(f"Saved {out}  sha256={h.hexdigest()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
