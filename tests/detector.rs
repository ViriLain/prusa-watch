//! Port of tests/test_detector.py.

mod common;

use common::*;
use prusa_watch::detector::{Detect, SpaghettiDetector, annotate, post_process};

// Python's SpaghettiDetector.detect defaults
const THRESH: f64 = 0.08;
const NMS: f64 = 0.45;

fn load() -> SpaghettiDetector {
    SpaghettiDetector::load(fake_model_path(), false).unwrap()
}

#[test]
fn fake_model_io_contract() {
    let det = load();
    assert_eq!((det.input_w, det.input_h), (416, 416));
}

#[test]
fn bright_frame_detects_and_nms_dedupes() {
    let det = load();
    let dets = det.detect(&solid(255), 0.08, 0.45);
    // 3 raw boxes, two overlap heavily -> 2 after NMS
    assert_eq!(dets.len(), 2);
    let mut confs: Vec<f64> = dets.iter().map(|d| d.confidence).collect();
    confs.sort_by(|a, b| b.partial_cmp(a).unwrap());
    assert!((confs[0] - 0.9).abs() < 1e-3 && (confs[1] - 0.3).abs() < 1e-3, "{confs:?}");
    assert!(dets.iter().all(|d| d.label == "failure"));
    assert!(det.last_inference_ms() > 0.0);
}

#[test]
fn dark_frame_is_clean() {
    let det = load();
    assert!(det.detect(&solid(0), THRESH, NMS).is_empty());
}

#[test]
fn threshold_filters() {
    let det = load();
    let dets = det.detect(&solid(128), 0.4, NMS); // mean ~0.5 -> confs .45/.30/.15
    let got: Vec<f64> = dets.iter().map(|d| (d.confidence * 100.0).round() / 100.0).collect();
    assert_eq!(got, vec![0.45]);
}

#[test]
fn boxes_scaled_to_image_pixels() {
    let boxes = [0.1f32, 0.2, 0.3, 0.6]; // [1,1,1,4]
    let confs = [0.8f32]; // [1,1,1]
    let dets = post_process(&boxes, &confs, 1, 1000, 500, 0.1, 0.45, &["failure".to_string()]);
    assert_eq!(dets.len(), 1);
    let [xc, yc, w, h] = dets[0].bbox;
    assert_eq!((xc.round(), yc.round(), w.round(), h.round()), (200.0, 200.0, 200.0, 200.0));
}

#[test]
fn annotate_returns_same_shape() {
    let det = load();
    let img = solid(255);
    let out = annotate(&img, &det.detect(&img, THRESH, NMS), 0.2, Some("label"));
    assert_eq!(out.dimensions(), img.dimensions());
    assert_ne!(out.as_raw(), img.as_raw());
}
