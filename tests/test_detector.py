import numpy as np

from conftest import solid
from prusa_watch.detector import SpaghettiDetector, annotate, post_process


def test_fake_model_io_contract(fake_model):
    det = SpaghettiDetector(fake_model)
    assert (det.input_w, det.input_h) == (416, 416)


def test_bright_frame_detects_and_nms_dedupes(fake_model):
    det = SpaghettiDetector(fake_model)
    dets = det.detect(solid(255), thresh=0.08, nms=0.45)
    # 3 raw boxes, two overlap heavily -> 2 after NMS
    assert len(dets) == 2
    confs = sorted((d.confidence for d in dets), reverse=True)
    assert abs(confs[0] - 0.9) < 1e-3 and abs(confs[1] - 0.3) < 1e-3
    assert all(d.label == "failure" for d in dets)
    assert det.last_inference_ms > 0


def test_dark_frame_is_clean(fake_model):
    det = SpaghettiDetector(fake_model)
    assert det.detect(solid(0)) == []


def test_threshold_filters(fake_model):
    det = SpaghettiDetector(fake_model)
    dets = det.detect(solid(128), thresh=0.4)  # mean ~0.5 -> confs .45/.30/.15
    assert [round(d.confidence, 2) for d in dets] == [0.45]


def test_boxes_scaled_to_image_pixels():
    boxes = np.array([[[[0.1, 0.2, 0.3, 0.6]]]], dtype=np.float32)  # [1,1,1,4]
    confs = np.array([[[0.8]]], dtype=np.float32)
    (d,) = post_process([boxes, confs], width=1000, height=500, conf_thresh=0.1, nms_thresh=0.45, names=["failure"])
    xc, yc, w, h = d.box
    assert (round(xc), round(yc), round(w), round(h)) == (200, 200, 200, 200)


def test_annotate_returns_same_shape(fake_model):
    det = SpaghettiDetector(fake_model)
    img = solid(255)
    out = annotate(img, det.detect(img), 0.2, "label")
    assert out.shape == img.shape and not np.array_equal(out, img)
