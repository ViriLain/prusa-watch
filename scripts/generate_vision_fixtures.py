"""Regenerate independent vision goldens; requires tests/vision-requirements.txt.

Inputs remain committed. Pass the SHA-pinned production model as the sole argument.
This measures implementation parity, not labeled detection accuracy.
"""
import hashlib
import json
from pathlib import Path
import sys

import cv2
import numpy as np
import onnxruntime as ort

from reference_detector import SpaghettiDetector

ROOT = Path(__file__).resolve().parents[1]
CORPUS = ROOT / "tests/fixtures/vision"
MODEL_SHA = "0a6ebd8e30dbf6a450c50f9c0a5406f04ba7eb1c99fd5996e888c78bb383b9aa"
model = Path(sys.argv[1])
assert hashlib.sha256(model.read_bytes()).hexdigest() == MODEL_SHA
detector = SpaghettiDetector(str(model))
golden = {}
for path in sorted(CORPUS.glob("*.png")):
    image = cv2.imread(str(path))
    detections = detector.detect(image)
    golden[path.stem] = {
        "file": path.name,
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "p": sum(d.confidence for d in detections),
        "dets": [[d.confidence, list(d.box)] for d in detections],
    }
(CORPUS / "golden.json").write_text(json.dumps(golden, indent=2) + "\n")
metadata = {"model_sha256": MODEL_SHA, "opencv": cv2.__version__,
            "numpy": np.__version__, "onnxruntime": ort.__version__,
            "resize": "INTER_LINEAR_EXACT", "threshold": 0.08, "nms": 0.45}
(CORPUS / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
cases = {}
for w, h in [(1280, 720), (640, 360), (641, 359), (320, 240), (832, 832), (416, 416)]:
    y, x, c = np.indices((h, w, 3), dtype=np.int64)
    image = ((x * (7+c) + y*13 + (x*y) % (97+c) + c*50) % 256).astype(np.uint8)
    out = cv2.resize(image, (416, 416), interpolation=cv2.INTER_LINEAR_EXACT)
    cases[f"{w}x{h}"] = {"first": out[0, 0].tolist(), "center": out[208, 208].tolist(),
                         "last": out[-1, -1].tolist(), "sha256": hashlib.sha256(out.tobytes()).hexdigest()}
(ROOT / "tests/fixtures/resize_opencv.json").write_text(json.dumps({"metadata": metadata, "cases": cases}, indent=2) + "\n")
