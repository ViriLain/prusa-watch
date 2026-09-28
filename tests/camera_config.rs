//! Port of tests/test_camera_config.py: frame grabber, ROI crop, config env
//! expansion and validation.
//!
//! Adaptations: the Python grabber decoded through OpenCV and the test wrote its
//! clip with cv2.VideoWriter; the Rust grabber shells out to ffmpeg, so the
//! same 320x240 MJPEG AVI is generated with ffmpeg instead. Images are
//! `RgbImage` (width x height) rather than numpy (h, w, 3) arrays.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use image::RgbImage;
use prusa_watch::camera::{FrameSource, Grabber};
use prusa_watch::config::load_config;

#[test]
fn grabber_reads_video_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("clip.avi");
    // 25 frames at 25 fps, 320x240, MJPEG (what the Python test wrote via cv2.VideoWriter)
    let st = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=gray:size=320x240:rate=25",
            "-frames:v",
            "25",
            "-c:v",
            "mjpeg",
        ])
        .arg(&path)
        .stdin(Stdio::null())
        .status()
        .expect("ffmpeg");
    assert!(st.success());

    let g = Grabber::new(path.to_str().unwrap(), "tcp", 5.0, 10.0, 10.0);
    g.start();
    let deadline = Instant::now() + Duration::from_secs(5);
    while g.latest().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let f = g.latest();
    g.stop();
    let f = f.expect("no frame within 5 s");
    assert_eq!(f.image.dimensions(), (320, 240));
    assert!(g.0.frames_decoded() > 0);
}

#[test]
fn grabber_survives_bad_url() {
    let g = Grabber::new("/nonexistent/file.mp4", "tcp", 0.1, 10.0, 10.0);
    g.start();
    // Python's in-process OpenCV failed instantly; here ffmpeg has to start and exit first,
    // which on a cold CI runner can take longer than the original fixed 0.5 s.
    let deadline = Instant::now() + Duration::from_secs(5);
    while g.0.reconnects() < 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    g.stop();
    assert!(g.latest().is_none());
    assert!(g.0.reconnects() >= 1, "reconnects={}", g.0.reconnects());
    assert!(!g.connected());
}

#[test]
fn crop_roi() {
    let img = RgbImage::new(200, 100);
    assert_eq!(prusa_watch::imaging::crop_roi(&img, Some(&[0.25, 0.1, 0.75, 0.9])).dimensions(), (100, 80));
    // Python asserts identity (`is img`); Rust returns an owned copy, so compare by value.
    assert_eq!(prusa_watch::imaging::crop_roi(&img, None), img);
}

#[test]
fn config_env_expansion_and_validation() {
    // SAFETY: std serializes its own env access; the only other env readers in
    // this binary are std::process::Command spawns, which take the same lock.
    unsafe {
        std::env::set_var("PL_PASS", "hunter2");
        std::env::set_var("SENS", "1.3");
        std::env::remove_var("NTFY_TOPIC"); // so the ${NTFY_TOPIC:-default-topic} fallback applies
    }
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("c.yaml");
    std::fs::write(
        &p,
        r#"
printer: {host: 192.168.1.50, password: "${PL_PASS}"}
camera: {url: "rtsp://192.168.1.51/live", roi: [0.1, 0.1, 0.9, 0.9]}
decision: {sensitivity: "${SENS}"}
notify: {ntfy: {topic: "${NTFY_TOPIC:-default-topic}"}}
"#,
    )
    .unwrap();
    let cfg = load_config(Some(&p), &BTreeMap::new()).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.printer.password, "hunter2");
    assert_eq!(cfg.decision.sensitivity, 1.3);
    assert_eq!(cfg.notify.ntfy.topic, "default-topic");
}

#[test]
fn config_rejects_unknown_keys_and_bad_values() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("c.yaml");
    std::fs::write(&p, "printer: {hots: x}\n").unwrap();
    let e = load_config(Some(&p), &BTreeMap::new()).unwrap_err().to_string();
    assert!(e.contains("Unknown config key 'printer.hots'"), "{e}");
    std::fs::write(
        &p,
        "printer: {host: x, password: y}\ncamera: {url: rtsp://a/live}\nescalation: {policies: {p: {steps: [{action: explode}]}}}\n",
    )
    .unwrap();
    let e = load_config(Some(&p), &BTreeMap::new()).unwrap().validate().unwrap_err().to_string();
    assert!(e.contains("action must be one of"), "{e}");
}
