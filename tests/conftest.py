"""Shared fixtures.

The real Obico weights are ~250 MB and fetched at deploy time, so tests use a
tiny synthetic ONNX model with the *same I/O contract* as Obico's export:

    input  : float32 [1, 3, 416, 416]  (RGB, 0..1)
    output0: boxes   [1, N, 1, 4]      normalized x1, y1, x2, y2
    output1: confs   [1, N, 1]         per-box, single class "failure"

Confidence = mean pixel brightness * per-box scale, so a white frame looks
like "lots of spaghetti" and a black frame looks clean.
"""

from __future__ import annotations

import numpy as np
import pytest

BOX_SCALES = [0.9, 0.6, 0.3]
BOXES = [
    [0.10, 0.10, 0.40, 0.40],
    [0.12, 0.12, 0.41, 0.41],  # overlaps box 0 -> removed by NMS
    [0.60, 0.60, 0.90, 0.80],
]


def build_fake_model(path):
    import onnx
    from onnx import TensorProto, helper, numpy_helper

    n = len(BOX_SCALES)
    x = helper.make_tensor_value_info("input", TensorProto.FLOAT, [1, 3, 416, 416])
    boxes_out = helper.make_tensor_value_info("boxes", TensorProto.FLOAT, [1, n, 1, 4])
    confs_out = helper.make_tensor_value_info("confs", TensorProto.FLOAT, [1, n, 1])

    axes = numpy_helper.from_array(np.array([1, 2, 3], dtype=np.int64), "axes")
    shape = numpy_helper.from_array(np.array([1, 1, 1], dtype=np.int64), "shape")
    scales = numpy_helper.from_array(np.array(BOX_SCALES, dtype=np.float32).reshape(1, n, 1), "scales")
    boxes = numpy_helper.from_array(np.array(BOXES, dtype=np.float32).reshape(1, n, 1, 4), "boxes_const")
    zero = numpy_helper.from_array(np.zeros((1, n, 1, 4), dtype=np.float32), "zero")

    nodes = [
        helper.make_node("ReduceMean", ["input", "axes"], ["mean"], keepdims=1),
        helper.make_node("Reshape", ["mean", "shape"], ["mean3"]),
        helper.make_node("Mul", ["mean3", "scales"], ["confs"]),
        helper.make_node("Add", ["boxes_const", "zero"], ["boxes"]),
    ]
    graph = helper.make_graph(nodes, "fake_obico", [x], [boxes_out, confs_out], [axes, shape, scales, boxes, zero])
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 18)])
    model.ir_version = 8
    onnx.checker.check_model(model)
    onnx.save(model, str(path))
    return str(path)


@pytest.fixture(scope="session")
def fake_model(tmp_path_factory):
    return build_fake_model(tmp_path_factory.mktemp("model") / "fake.onnx")


def solid(value: int, w: int = 640, h: int = 360) -> np.ndarray:
    return np.full((h, w, 3), value, dtype=np.uint8)
