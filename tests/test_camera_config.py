import time

import cv2
import numpy as np
import pytest

from prusa_watch.camera import FrameGrabber, crop_roi
from prusa_watch.config import load_config


def test_grabber_reads_video_file(tmp_path):
    path = str(tmp_path / "clip.avi")
    vw = cv2.VideoWriter(path, cv2.VideoWriter_fourcc(*"MJPG"), 25, (320, 240))
    for i in range(25):
        vw.write(np.full((240, 320, 3), i * 10 % 255, dtype=np.uint8))
    vw.release()

    g = FrameGrabber(path)
    g.start()
    deadline = time.time() + 5
    while g.latest() is None and time.time() < deadline:
        time.sleep(0.05)
    f = g.latest()
    g.stop()
    assert f is not None and f.image.shape == (240, 320, 3)
    assert g.frames_decoded > 0


def test_grabber_survives_bad_url():
    g = FrameGrabber("/nonexistent/file.mp4", reconnect_backoff_s=0.1)
    g.start()
    time.sleep(0.5)
    g.stop()
    assert g.latest() is None and g.reconnects >= 1 and not g.connected


def test_crop_roi():
    img = np.zeros((100, 200, 3), dtype=np.uint8)
    assert crop_roi(img, [0.25, 0.1, 0.75, 0.9]).shape == (80, 100, 3)
    assert crop_roi(img, None) is img


def test_config_env_expansion_and_validation(tmp_path, monkeypatch):
    monkeypatch.setenv("PL_PASS", "hunter2")
    monkeypatch.setenv("SENS", "1.3")
    p = tmp_path / "c.yaml"
    p.write_text(
        """
printer: {host: 192.168.1.50, password: "${PL_PASS}"}
camera: {url: "rtsp://192.168.1.51/live", roi: [0.1, 0.1, 0.9, 0.9]}
decision: {sensitivity: "${SENS}", action: pause}
notify: {ntfy: {topic: "${NTFY_TOPIC:-default-topic}"}}
"""
    )
    cfg = load_config(p)
    cfg.validate()
    assert cfg.printer.password == "hunter2"
    assert cfg.decision.sensitivity == 1.3
    assert cfg.notify.ntfy.topic == "default-topic"


def test_config_rejects_unknown_keys_and_bad_values(tmp_path):
    p = tmp_path / "c.yaml"
    p.write_text("printer: {hots: x}\n")
    with pytest.raises(ValueError, match="Unknown config key 'hots'"):
        load_config(p)
    p.write_text("printer: {host: x, password: y}\ncamera: {url: rtsp://a/live}\ndecision: {action: explode}\n")
    with pytest.raises(ValueError, match="decision.action"):
        load_config(p).validate()
