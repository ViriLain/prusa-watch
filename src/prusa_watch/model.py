"""Download Obico's open-source failure-detection model (ONNX).

Source of truth for the URL: obico-server ml_api/model/model-weights.onnx.url
(AGPL-3.0, https://github.com/TheSpaghettiDetective/obico-server).

Uses httpx, which verifies TLS against certifi's CA bundle. That matters on
macOS: the python.org installer ships without system CA certificates, so
urllib-based downloads fail with CERTIFICATE_VERIFY_FAILED until the user runs
"Install Certificates.command".
"""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path
from typing import Callable

import httpx

DEFAULT_URL = "https://tsd-pub-static.s3.amazonaws.com/ml-models/model-weights-5a6b1be1fa.onnx"
MIN_BYTES = 1_000_000  # the real model is ~250 MB; anything tiny is an error page


class ModelDownloadError(RuntimeError):
    pass


def _progress_stderr(done: int, total: int) -> None:
    if total:
        print(f"\r  {done / 1e6:6.1f} / {total / 1e6:.1f} MB", end="", file=sys.stderr, flush=True)


def download_model(
    out: str | Path,
    url: str = DEFAULT_URL,
    force: bool = False,
    transport: httpx.BaseTransport | None = None,
    progress: Callable[[int, int], None] | None = _progress_stderr,
    verify_onnx: bool = True,
) -> Path:
    """Download the model to `out` (atomically, via a .part file). Returns the path.

    No-op if `out` already exists and force is False.
    """
    out = Path(out)
    if out.exists() and not force:
        return out
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_name(out.name + ".part")

    print(f"Downloading model from {url}", file=sys.stderr)
    h = hashlib.sha256()
    done = 0
    try:
        with httpx.Client(transport=transport, follow_redirects=True, timeout=httpx.Timeout(60.0)) as client:
            with client.stream("GET", url) as r:
                r.raise_for_status()
                total = int(r.headers.get("Content-Length") or 0)
                with open(tmp, "wb") as fh:
                    for chunk in r.iter_bytes(1 << 20):
                        fh.write(chunk)
                        h.update(chunk)
                        done += len(chunk)
                        if progress:
                            progress(done, total)
    except httpx.HTTPError as exc:
        tmp.unlink(missing_ok=True)
        raise ModelDownloadError(f"model download failed: {exc}") from exc
    finally:
        if progress:
            print(file=sys.stderr)

    if done < MIN_BYTES:
        tmp.unlink(missing_ok=True)
        raise ModelDownloadError(f"model download looks wrong ({done} bytes); aborting")

    if verify_onnx:
        try:
            import onnxruntime as ort

            ort.InferenceSession(str(tmp), providers=["CPUExecutionProvider"])
        except ImportError:
            pass
        except Exception as exc:
            tmp.unlink(missing_ok=True)
            raise ModelDownloadError(f"downloaded file is not a loadable ONNX model: {exc}") from exc

    tmp.replace(out)
    print(f"Saved {out} ({done / 1e6:.1f} MB, sha256={h.hexdigest()})", file=sys.stderr)
    return out
