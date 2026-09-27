"""Spaghetti / print-failure detector.

Runs Obico's open-source failure-detection model (YOLOv4-family, single class
"failure", 416x416 input) through ONNX Runtime. Pre/post-processing is ported
from obico-server ``ml_api/lib/onnx.py`` (AGPL-3.0) so the per-frame
confidences match what Obico's tuned decision thresholds expect.

Model weights: https://tsd-pub-static.s3.amazonaws.com/ml-models/model-weights-5a6b1be1fa.onnx
(fetched by ``prusa-watch fetch-model``, or automatically on first run).
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass

import cv2
import numpy as np

log = logging.getLogger(__name__)


@dataclass
class Detection:
    label: str
    confidence: float
    # center-x, center-y, width, height in pixels of the analyzed image
    box: tuple[float, float, float, float]

    def as_list(self):
        return [self.label, self.confidence, list(self.box)]


class SpaghettiDetector:
    def __init__(self, model_path: str, use_gpu: bool = False, names: list[str] | None = None):
        import onnxruntime as ort

        available = ort.get_available_providers()
        providers = ["CPUExecutionProvider"]
        if use_gpu:
            for p in ("CUDAExecutionProvider", "DmlExecutionProvider"):
                if p in available:
                    providers.insert(0, p)
                    break
            else:
                log.warning("use_gpu requested but no GPU execution provider available (%s); using CPU", available)
        self.session = ort.InferenceSession(model_path, providers=providers)
        inp = self.session.get_inputs()[0]
        self.input_name = inp.name
        # NCHW; Obico's export is static 1x3x416x416
        self.input_h = int(inp.shape[2]) if isinstance(inp.shape[2], int) else 416
        self.input_w = int(inp.shape[3]) if isinstance(inp.shape[3], int) else 416
        self.names = names or ["failure"]
        self.last_inference_ms = 0.0
        log.info(
            "Detector: loaded %s (%dx%d) providers=%s",
            model_path, self.input_w, self.input_h, self.session.get_providers(),
        )

    def detect(self, image: np.ndarray, thresh: float = 0.08, nms: float = 0.45) -> list[Detection]:
        height, width = image.shape[:2]
        resized = cv2.resize(image, (self.input_w, self.input_h), interpolation=cv2.INTER_LINEAR)
        img_in = cv2.cvtColor(resized, cv2.COLOR_BGR2RGB)
        img_in = np.transpose(img_in, (2, 0, 1)).astype(np.float32)[None, ...] / 255.0

        t0 = time.perf_counter()
        outputs = self.session.run(None, {self.input_name: img_in})
        self.last_inference_ms = (time.perf_counter() - t0) * 1000
        return post_process(outputs, width, height, thresh, nms, self.names)


def _nms(boxes: np.ndarray, confs: np.ndarray, nms_thresh: float) -> np.ndarray:
    x1, y1, x2, y2 = boxes[:, 0], boxes[:, 1], boxes[:, 2], boxes[:, 3]
    areas = (x2 - x1) * (y2 - y1)
    order = confs.argsort()[::-1]
    keep = []
    while order.size > 0:
        i = order[0]
        keep.append(i)
        xx1 = np.maximum(x1[i], x1[order[1:]])
        yy1 = np.maximum(y1[i], y1[order[1:]])
        xx2 = np.minimum(x2[i], x2[order[1:]])
        yy2 = np.minimum(y2[i], y2[order[1:]])
        inter = np.maximum(0.0, xx2 - xx1) * np.maximum(0.0, yy2 - yy1)
        union = areas[i] + areas[order[1:]] - inter
        over = np.divide(inter, union, out=np.zeros_like(inter), where=union > 0)
        order = order[np.where(over <= nms_thresh)[0] + 1]
    return np.array(keep, dtype=int)


def post_process(outputs, width: int, height: int, conf_thresh: float, nms_thresh: float, names: list[str]) -> list[Detection]:
    """YOLOv4 (pytorch-YOLOv4 ONNX export) post-processing.

    outputs[0]: boxes [batch, num, 1, 4] as normalized x1,y1,x2,y2
    outputs[1]: confs [batch, num, num_classes]
    """
    box_array = np.asarray(outputs[0])[:, :, 0]
    confs = np.asarray(outputs[1])
    num_classes = confs.shape[2]
    max_conf = confs.max(axis=2)
    max_id = confs.argmax(axis=2)

    dets: list[Detection] = []
    i = 0  # batch of 1
    mask = max_conf[i] > conf_thresh
    l_boxes, l_conf, l_id = box_array[i, mask, :], max_conf[i, mask], max_id[i, mask]
    for j in range(num_classes):
        cls = l_id == j
        b, c = l_boxes[cls], l_conf[cls]
        if b.size == 0:
            continue
        for k in _nms(b, c, nms_thresh):
            x1, y1, x2, y2 = (float(v) for v in b[k])
            label = names[j] if j < len(names) else str(j)
            dets.append(
                Detection(
                    label=label,
                    confidence=float(c[k]),
                    # Upstream scales h by width (bug); only confidences feed the
                    # decision, but boxes are drawn, so use height here.
                    box=(0.5 * width * (x1 + x2), 0.5 * height * (y1 + y2), width * (x2 - x1), height * (y2 - y1)),
                )
            )
    return dets


def annotate(image: np.ndarray, detections: list[Detection], min_conf: float = 0.2, label: str | None = None) -> np.ndarray:
    out = image.copy()
    for d in detections:
        if d.confidence < min_conf:
            continue
        xc, yc, w, h = d.box
        p1 = (int(xc - w / 2), int(yc - h / 2))
        p2 = (int(xc + w / 2), int(yc + h / 2))
        color = (0, 0, 255) if d.confidence >= 0.5 else (0, 165, 255)
        cv2.rectangle(out, p1, p2, color, 2)
        cv2.putText(out, f"{d.confidence:.2f}", (p1[0], max(12, p1[1] - 4)), cv2.FONT_HERSHEY_SIMPLEX, 0.5, color, 1, cv2.LINE_AA)
    if label:
        cv2.rectangle(out, (0, 0), (out.shape[1], 22), (0, 0, 0), -1)
        cv2.putText(out, label, (6, 16), cv2.FONT_HERSHEY_SIMPLEX, 0.5, (255, 255, 255), 1, cv2.LINE_AA)
    return out
