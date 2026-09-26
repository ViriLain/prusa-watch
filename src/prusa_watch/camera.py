"""Buddy3D camera frame source.

The Buddy3D exposes an RTSP stream at ``rtsp://<camera-ip>/live`` once "RTSP"
is enabled for the camera in the Prusa app / Prusa Connect. There is no
snapshot endpoint, so we hold one persistent RTSP session open in a background
thread and always keep only the most recent decoded frame. Opening a fresh
RTSP session per inference would cost 1-3 s of handshake + keyframe wait and
hammer the camera's small SoC.

Any source OpenCV/FFmpeg can open works (file path, http MJPEG, other RTSP
cameras), which is how the tests drive this without hardware.
"""

from __future__ import annotations

import logging
import os
import threading
import time
from dataclasses import dataclass

import cv2
import numpy as np

log = logging.getLogger(__name__)


@dataclass
class Frame:
    image: np.ndarray  # BGR
    ts: float  # time.time() when decoded


class FrameGrabber:
    def __init__(self, url: str, transport: str = "tcp", reconnect_backoff_s: float = 5.0):
        self.url = url
        self.transport = transport
        self.reconnect_backoff_s = reconnect_backoff_s
        self._lock = threading.Lock()
        self._latest: Frame | None = None
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self.connected = False
        self.reconnects = 0
        self.frames_decoded = 0

    # -- public API -------------------------------------------------------
    def start(self) -> None:
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, name="frame-grabber", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=5)

    def latest(self) -> Frame | None:
        with self._lock:
            return self._latest

    # -- internals --------------------------------------------------------
    def _is_live(self) -> bool:
        return self.url.lower().startswith(("rtsp://", "rtsps://", "http://", "https://"))

    def _open(self) -> cv2.VideoCapture:
        if self.url.lower().startswith("rtsp") and self.transport:
            # Must be set before the capture is created; applies to the FFmpeg backend.
            os.environ["OPENCV_FFMPEG_CAPTURE_OPTIONS"] = f"rtsp_transport;{self.transport}"
        params = [cv2.CAP_PROP_OPEN_TIMEOUT_MSEC, 10000, cv2.CAP_PROP_READ_TIMEOUT_MSEC, 10000]
        cap = cv2.VideoCapture(self.url, cv2.CAP_FFMPEG, params)
        # Keep the internal buffer tiny so we don't analyze stale frames.
        cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)
        return cap

    def _run(self) -> None:
        live = self._is_live()
        while not self._stop.is_set():
            cap = self._open()
            if not cap.isOpened():
                self.connected = False
                log.warning("Camera: could not open %s; retrying in %.0fs", _redact(self.url), self.reconnect_backoff_s)
                cap.release()
                self._stop.wait(self.reconnect_backoff_s)
                self.reconnects += 1
                continue

            log.info("Camera: connected to %s", _redact(self.url))
            self.connected = True
            fps = cap.get(cv2.CAP_PROP_FPS) or 25.0
            file_delay = 1.0 / fps if not live and fps > 0 else 0.0

            while not self._stop.is_set():
                ok, img = cap.read()
                if not ok or img is None:
                    if not live:
                        # File source: loop it (useful for demos/tests).
                        cap.set(cv2.CAP_PROP_POS_FRAMES, 0)
                        ok, img = cap.read()
                        if not ok:
                            break
                    else:
                        log.warning("Camera: stream read failed, reconnecting")
                        break
                with self._lock:
                    self._latest = Frame(image=img, ts=time.time())
                self.frames_decoded += 1
                if file_delay:
                    time.sleep(file_delay)

            cap.release()
            self.connected = False
            if not self._stop.is_set():
                self.reconnects += 1
                self._stop.wait(self.reconnect_backoff_s)


def crop_roi(image: np.ndarray, roi: list[float] | None) -> np.ndarray:
    if not roi:
        return image
    h, w = image.shape[:2]
    x1, y1, x2, y2 = int(roi[0] * w), int(roi[1] * h), int(roi[2] * w), int(roi[3] * h)
    return image[y1:y2, x1:x2]


def _redact(url: str) -> str:
    # rtsp://user:pass@host/... -> rtsp://***@host/...
    if "@" in url and "://" in url:
        scheme, rest = url.split("://", 1)
        return f"{scheme}://***@{rest.split('@', 1)[1]}"
    return url
