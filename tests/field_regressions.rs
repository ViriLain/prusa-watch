//! Regressions from real prints.
//!
//! Job 419 (Core One, 2026-09-28): 88 clean frames, then dust on the sheet's top-right
//! corner caught the chamber light for about a minute at one bed height. The model put
//! 5-12 boxes there (p 0.51 -> 2.45), the print was paused, and after the resume grace
//! it was paused *again* on a frame with p = 0 while the moving average was still
//! decaying. The p / box-count sequence below is the recorded one; box positions are
//! where the saved frames show them.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use common::*;
use image::RgbImage;
use prusa_watch::config::Config;
use prusa_watch::detector::{Detect, Detection};
use prusa_watch::monitor::{drop_ignored, ignore_zones_in_crop};

/// Recorded (p, boxes) for job 419 frames 86..97 (frames 1..85 were p = 0).
const BURST: [(f64, usize); 12] = [
    (0.0, 0),
    (0.0, 0),
    (0.0, 0),
    (0.51, 5),
    (1.148, 8),
    (2.4482, 10),
    (2.2612, 12),
    (1.8077, 10),
    (1.246, 7),
    (0.0, 0),
    (0.1215, 1),
    (0.0, 0),
];

/// User's ROI and the sheet-corner zone, in full-frame normalized coordinates.
const ROI: [f64; 4] = [0.14, 0.0, 0.88, 1.0];
const CORNER: [f64; 4] = [0.62, 0.05, 0.88, 0.48];

/// Replays a scripted p sequence: each frame's p is split over n boxes centred in the
/// glinting corner of the ROI crop (where job 419's saved frames show them).
struct Replay {
    frames: Mutex<VecDeque<(f64, usize)>>,
}

impl Replay {
    fn job419() -> Arc<Self> {
        let mut v: VecDeque<(f64, usize)> = std::iter::repeat_n((0.0, 0), 85).collect();
        v.extend(BURST);
        Arc::new(Self { frames: Mutex::new(v) })
    }
}

impl Detect for Replay {
    fn try_detect(&self, img: &RgbImage, _: f64, _: f64) -> Result<Vec<Detection>, String> {
        let (p, n) = self.frames.lock().unwrap().pop_front().unwrap_or((0.0, 0));
        let (w, h) = (img.width() as f64, img.height() as f64);
        Ok((0..n)
            .map(|i| Detection {
                label: "failure".into(),
                confidence: p / n as f64,
                // spread over x 0.70-0.92, y 0.10-0.42 of the crop (the top-right sheet corner)
                bbox: [w * (0.70 + 0.02 * i as f64), h * (0.10 + 0.03 * i as f64), 60.0, 60.0],
            })
            .collect())
    }
    fn last_inference_ms(&self) -> f64 {
        100.0
    }
}

fn job419_rig(tweak: impl FnOnce(&mut Config)) -> Rig {
    let mut r = rig_with(|c| {
        c.camera.roi = Some(ROI.to_vec());
        c.escalation = serde_yaml::from_str("{default_policy: pause_now, schedules: []}").unwrap();
        c.decision.sensitivity = 1.25;
        tweak(c);
    });
    // swap in the replaying detector (same rig otherwise)
    let cfg = r.mon.cfg.clone();
    let mut n = prusa_watch::notify::Notifier::with_transport(cfg.notify.clone(), "", "", r.transport.clone());
    n.blocking = true;
    r.mon = prusa_watch::monitor::Monitor::new(
        cfg,
        prusa_watch::monitor::Parts {
            printer: Some(r.printer.clone()),
            grabber: Some(r.grabber.clone()),
            detector: Some(Replay::job419()),
            notifier: Some(Arc::new(n)),
            clock: Some(r.clock.as_fn()),
            ..Default::default()
        },
    )
    .unwrap();
    r.grabber.set_image(Some(RgbImage::new(1280, 720)));
    r.printer.set("PRINTING", Some(419));
    r
}

#[test]
fn without_mitigation_the_glint_pauses_the_print() {
    // What happened in the field: pure Obico gating, no ignore zone.
    let r = job419_rig(|c| c.decision.min_frame_p = 0.0);
    r.advance(97);
    assert_eq!(r.printer.count("pause", 419), 1, "reproduces the job-419 false pause");
}

#[test]
fn ignore_zone_suppresses_the_glint() {
    let r = job419_rig(|c| c.camera.ignore = vec![CORNER.to_vec()]);
    r.advance(97);
    assert!(r.printer.calls().is_empty(), "no pause");
    assert_eq!(r.mon.counters().warnings, 0, "not even a warning");
    assert_eq!(
        r.mon.snapshot().current_p,
        0.0,
        "the corner's boxes never reached the score"
    );
}

#[test]
fn clean_frame_never_opens_an_incident() {
    // Reproduce the second pause (10:06:30): the burst happens while incidents are
    // disarmed (resume grace), and the grace ends while the average is still high but
    // the frames are clean again.
    let run = |min_frame_p: f64| {
        let r = job419_rig(|c| c.decision.min_frame_p = min_frame_p);
        r.advance(85);
        // disarmed through the whole burst (frames 86-94); the first armed frame is 95 (p = 0)
        r.mon.core().job.rearm_at = r.clock.t() + 10.0 * 9.5;
        r.advance(12);
        let s = r.mon.core().decider.state.clone();
        (r.printer.count("pause", 419), s.current_p, s.ewm_mean)
    };
    let (pauses, p, ewm) = run(0.0);
    assert_eq!(
        pauses, 1,
        "pure Obico pauses on the decaying average (p={p}, ewm={ewm:.2})"
    );
    let (pauses, _, _) = run(0.3);
    assert_eq!(pauses, 0, "with the gate a clean frame can't open an incident");
}

#[test]
fn gate_does_not_delay_a_real_failure() {
    // Sustained real spaghetti keeps p high on every frame, so the gate never bites.
    let r = rig_with(|c| c.decision.min_frame_p = 0.3);
    r.printer.set("PRINTING", Some(7));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    let mut frames = 0;
    while r.printer.count("pause", 7) == 0 && frames < 40 {
        r.advance(1);
        frames += 1;
    }
    let r0 = rig_with(|c| c.decision.min_frame_p = 0.0);
    r0.printer.set("PRINTING", Some(7));
    r0.advance(60);
    r0.grabber.set_image(Some(solid(255)));
    let mut frames0 = 0;
    while r0.printer.count("pause", 7) == 0 && frames0 < 40 {
        r0.advance(1);
        frames0 += 1;
    }
    assert!(frames < 40, "paused");
    assert_eq!(frames, frames0, "same time to pause with and without the gate");
}

#[test]
fn ignore_zone_mapping_into_the_roi_crop() {
    let mut cfg = Config::default();
    cfg.camera.roi = Some(ROI.to_vec());
    cfg.camera.ignore = vec![CORNER.to_vec(), vec![0.0, 0.0, 0.1, 0.1]];
    let zones = ignore_zones_in_crop(&cfg, 947, 720);
    assert_eq!(zones.len(), 1, "a zone entirely outside the ROI maps to nothing");
    let z = zones[0];
    assert!((z[0] - (0.62 - 0.14) / 0.74 * 947.0).abs() < 1e-6 && (z[2] - 947.0).abs() < 1e-6);
    assert!((z[1] - 0.05 * 720.0).abs() < 1e-6 && (z[3] - 0.48 * 720.0).abs() < 1e-6);

    let det = |x: f64, y: f64| Detection {
        label: "failure".into(),
        confidence: 0.4,
        bbox: [x, y, 50.0, 50.0],
    };
    let (kept, ignored) = drop_ignored(vec![det(800.0, 100.0), det(300.0, 400.0), det(800.0, 600.0)], &zones);
    assert_eq!(ignored.len(), 1);
    assert_eq!(kept.len(), 2, "outside the zone (left, and below it) is kept");
}

#[test]
fn ignore_zones_are_validated() {
    let mut cfg = base_config(std::path::Path::new("/tmp"));
    cfg.camera.ignore = vec![vec![0.5, 0.5, 0.4, 0.9]];
    let e = cfg.validate().unwrap_err().to_string();
    assert!(e.contains("camera.ignore[0]"), "{e}");
    cfg.camera.ignore = vec![vec![0.1, 0.2, 0.3]];
    assert!(cfg.validate().is_err());
    cfg.camera.ignore = vec![CORNER.to_vec()];
    cfg.decision.min_frame_p = -1.0;
    assert!(cfg.validate().unwrap_err().to_string().contains("min_frame_p"));
}
