//! Spaghetti / print-failure detector.
//!
//! Runs Obico's open-source failure-detection model (YOLOv4-family, single class
//! "failure", 416x416 input) with tract (pure-Rust ONNX inference). Pre/post-
//! processing is ported from obico-server `ml_api/lib/onnx.py` (AGPL-3.0) so the
//! per-frame confidences match what Obico's tuned decision thresholds expect.
//!
//! Model weights: <https://tsd-pub-static.s3.amazonaws.com/ml-models/model-weights-5a6b1be1fa.onnx>
//! (fetched by `prusa-watch fetch-model`, or automatically on first run).

use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use image::{Rgb, RgbImage};
use serde_json::json;
use tract_onnx::prelude::*;

use crate::imaging::{draw_rect, draw_text, fill_rect, resize_linear};

#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    pub label: String,
    pub confidence: f64,
    /// center-x, center-y, width, height in pixels of the analyzed image
    pub bbox: [f64; 4],
}

impl Detection {
    pub fn as_list(&self) -> serde_json::Value {
        json!([self.label, self.confidence, self.bbox])
    }
}

/// Anything that turns an image into detections (the real model, or a fake in tests).
pub trait Detect: Send + Sync {
    /// Detections, or an error if inference itself failed (not the same as "nothing seen").
    fn try_detect(&self, image: &RgbImage, thresh: f64, nms: f64) -> Result<Vec<Detection>, String>;
    fn last_inference_ms(&self) -> f64;
}

type Plan = std::sync::Arc<TypedSimplePlan>;

pub struct SpaghettiDetector {
    plan: Plan,
    pub input_w: u32,
    pub input_h: u32,
    pub names: Vec<String>,
    last_ms: Mutex<f64>,
}

impl SpaghettiDetector {
    pub fn load(model_path: impl AsRef<Path>, use_gpu: bool) -> anyhow::Result<Self> {
        let path = model_path.as_ref();
        if use_gpu {
            tracing::warn!("use_gpu requested but prusa-watch runs inference on the CPU (tract); ignoring");
        }
        let mut model = tract_onnx::onnx().model_for_path(path)?;
        anyhow::ensure!(
            model.input_outlets()?.len() == 1,
            "detector model must have exactly one image input"
        );
        // NCHW; Obico's export is static 1x3x416x416. Pin symbolic dims to that.
        let (mut h, mut w) = (416u32, 416u32);
        if let Ok(fact) = model.input_fact(0)
            && let Ok(Some(dims)) = fact.shape.as_concrete_finite()
        {
            anyhow::ensure!(
                dims.len() == 4
                    && dims[0] == 1
                    && dims[1] == 3
                    && (1..=8192).contains(&dims[2])
                    && (1..=8192).contains(&dims[3]),
                "model input must be 1x3xHxW with dimensions in 1..=8192"
            );
            h = dims[2] as u32;
            w = dims[3] as u32;
        }
        model.set_input_fact(0, f32::fact([1, 3, h as usize, w as usize]).into())?;
        let plan = model.into_optimized()?.into_runnable()?;
        anyhow::ensure!(
            plan.model().output_outlets()?.len() == 2,
            "detector model must have boxes and confidences outputs"
        );
        anyhow::ensure!(
            plan.model().output_fact(0)?.datum_type == DatumType::F32
                && plan.model().output_fact(1)?.datum_type == DatumType::F32,
            "detector outputs must be float32"
        );
        let boxes = plan
            .model()
            .output_fact(0)?
            .shape
            .as_concrete()
            .ok_or_else(|| anyhow::anyhow!("boxes output shape must be static"))?;
        let confs = plan
            .model()
            .output_fact(1)?
            .shape
            .as_concrete()
            .ok_or_else(|| anyhow::anyhow!("confidences output shape must be static"))?;
        anyhow::ensure!(
            boxes.len() == 4
                && boxes[0] == 1
                && (1..=100_000).contains(&boxes[1])
                && boxes[2] == 1
                && boxes[3] == 4
                && confs.len() == 3
                && confs[0] == 1
                && confs[1] == boxes[1]
                && confs[2] == 1,
            "detector outputs must be boxes [1,N,1,4] and confidences [1,N,1]"
        );
        tracing::info!("Detector: loaded {} ({w}x{h}) tract CPU", path.display());
        Ok(Self {
            plan,
            input_w: w,
            input_h: h,
            names: vec!["failure".into()],
            last_ms: Mutex::new(0.0),
        })
    }

    /// Raw model outputs (boxes `[1,N,1,4]`, confs `[1,N,C]`) for an already-resized RGB image.
    pub fn run_raw(&self, resized: &RgbImage) -> anyhow::Result<(Vec<f32>, Vec<f32>, usize)> {
        let (w, h) = resized.dimensions();
        let raw = resized.as_raw();
        let input: Tensor = tract_ndarray::Array4::from_shape_fn((1, 3, h as usize, w as usize), |(_, c, y, x)| {
            raw[(y * w as usize + x) * 3 + c] as f32 / 255.0
        })
        .into();
        let out = self.plan.run(tvec!(input.into()))?;
        anyhow::ensure!(out.len() == 2, "detector model must produce two outputs");
        let boxes = out[0].to_plain_array_view::<f32>()?;
        let confs = out[1].to_plain_array_view::<f32>()?;
        anyhow::ensure!(
            boxes.ndim() == 4 && boxes.shape()[0] == 1 && boxes.shape()[2..] == [1, 4],
            "boxes output must be [1,N,1,4]"
        );
        anyhow::ensure!(
            confs.ndim() == 3 && confs.shape()[0] == 1 && confs.shape()[1] == boxes.shape()[1] && confs.shape()[2] == 1,
            "confidence output must be [1,N,1] matching boxes"
        );
        anyhow::ensure!(
            boxes.iter().all(|v| v.is_finite()) && confs.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
            "detector output contains invalid boxes or confidences"
        );
        let num_classes = *confs.shape().last().unwrap_or(&1);
        Ok((
            boxes.iter().copied().collect(),
            confs.iter().copied().collect(),
            num_classes,
        ))
    }
}

impl Detect for SpaghettiDetector {
    fn try_detect(&self, image: &RgbImage, thresh: f64, nms: f64) -> Result<Vec<Detection>, String> {
        let (width, height) = image.dimensions();
        let resized = resize_linear(image, self.input_w, self.input_h);
        let t0 = Instant::now();
        let result = self.run_raw(&resized);
        *self.last_ms.lock().unwrap() = t0.elapsed().as_secs_f64() * 1000.0;
        let (boxes, confs, nc) = result.map_err(|e| e.to_string())?;
        Ok(post_process(
            &boxes,
            &confs,
            nc,
            width,
            height,
            thresh,
            nms,
            &self.names,
        ))
    }

    fn last_inference_ms(&self) -> f64 {
        *self.last_ms.lock().unwrap()
    }
}

/// Greedy NMS in float32, like the numpy original. Returns kept indices.
fn nms_keep(boxes: &[[f32; 4]], confs: &[f32], nms_thresh: f64) -> Vec<usize> {
    let thr = nms_thresh as f32;
    let area = |b: &[f32; 4]| (b[2] - b[0]) * (b[3] - b[1]);
    // numpy: confs.argsort()[::-1] (stable ascending, reversed)
    let mut order: Vec<usize> = (0..confs.len()).collect();
    order.sort_by(|&a, &b| confs[a].partial_cmp(&confs[b]).unwrap_or(std::cmp::Ordering::Equal));
    order.reverse();
    let mut keep = Vec::new();
    while let Some(&i) = order.first() {
        keep.push(i);
        let bi = &boxes[i];
        let ai = area(bi);
        order = order[1..]
            .iter()
            .copied()
            .filter(|&j| {
                let bj = &boxes[j];
                let iw = (bi[2].min(bj[2]) - bi[0].max(bj[0])).max(0.0);
                let ih = (bi[3].min(bj[3]) - bi[1].max(bj[1])).max(0.0);
                let inter = iw * ih;
                let union = ai + area(bj) - inter;
                let over = if union > 0.0 { inter / union } else { 0.0 };
                over <= thr
            })
            .collect();
    }
    keep
}

/// YOLOv4 (pytorch-YOLOv4 ONNX export) post-processing.
///
/// boxes: [1, num, 1, 4] as normalized x1,y1,x2,y2; confs: [1, num, num_classes]
#[allow(clippy::too_many_arguments)]
pub fn post_process(
    boxes: &[f32],
    confs: &[f32],
    num_classes: usize,
    width: u32,
    height: u32,
    conf_thresh: f64,
    nms_thresh: f64,
    names: &[String],
) -> Vec<Detection> {
    let n = boxes.len() / 4;
    let nc = num_classes.max(1);
    let (w, h) = (width as f64, height as f64);
    let mut per_class: Vec<(Vec<[f32; 4]>, Vec<f32>)> = vec![(vec![], vec![]); nc];
    let thr = conf_thresh as f32;
    for i in 0..n {
        let row = &confs[i * nc..(i + 1) * nc];
        let (mut best, mut best_j) = (row[0], 0usize);
        for (j, &c) in row.iter().enumerate().skip(1) {
            if c > best {
                best = c;
                best_j = j;
            }
        }
        if best > thr {
            let b = &boxes[i * 4..i * 4 + 4];
            per_class[best_j].0.push([b[0], b[1], b[2], b[3]]);
            per_class[best_j].1.push(best);
        }
    }
    let mut dets = Vec::new();
    for (j, (b, c)) in per_class.iter().enumerate() {
        if b.is_empty() {
            continue;
        }
        for k in nms_keep(b, c, nms_thresh) {
            let [x1, y1, x2, y2] = b[k].map(|v| v as f64);
            let label = names.get(j).cloned().unwrap_or_else(|| j.to_string());
            dets.push(Detection {
                label,
                confidence: c[k] as f64,
                // Upstream scales h by width (bug); only confidences feed the
                // decision, but boxes are drawn, so use height here.
                bbox: [0.5 * w * (x1 + x2), 0.5 * h * (y1 + y2), w * (x2 - x1), h * (y2 - y1)],
            });
        }
    }
    dets
}

pub fn annotate(image: &RgbImage, detections: &[Detection], min_conf: f64, label: Option<&str>) -> RgbImage {
    let mut out = image.clone();
    for d in detections {
        if d.confidence < min_conf {
            continue;
        }
        let [xc, yc, w, h] = d.bbox;
        let p1 = ((xc - w / 2.0) as i64, (yc - h / 2.0) as i64);
        let p2 = ((xc + w / 2.0) as i64, (yc + h / 2.0) as i64);
        let color = if d.confidence >= 0.5 {
            Rgb([255, 0, 0])
        } else {
            Rgb([255, 165, 0])
        };
        draw_rect(&mut out, p1, p2, color, 2);
        draw_text(
            &mut out,
            p1.0,
            (p1.1 - 4).max(12),
            &format!("{:.2}", d.confidence),
            color,
        );
    }
    if let Some(label) = label.filter(|l| !l.is_empty()) {
        let w = out.width() as i64;
        fill_rect(&mut out, 0, 0, w, 22, Rgb([0, 0, 0]));
        draw_text(&mut out, 6, 16, label, Rgb([255, 255, 255]));
    }
    out
}
