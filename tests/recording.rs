//! Port of tests/test_recording.py: per-print recording (history CSV, saved frames,
//! restart append, pruning, failure isolation) and the `report` command.

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use common::*;
use prusa_watch::config::RecordingConfig;
use prusa_watch::recording::{COLUMNS, FrameRecord, Recorder};

struct Rows {
    headers: Vec<String>,
    rows: Vec<HashMap<String, String>>,
}

fn read_rows(path: &Path) -> Rows {
    let mut rdr = csv::Reader::from_path(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let headers: Vec<String> = rdr.headers().unwrap().iter().map(str::to_string).collect();
    let rows = rdr
        .records()
        .map(|r| {
            let r = r.unwrap();
            headers
                .iter()
                .cloned()
                .zip(r.iter().map(str::to_string))
                .collect::<HashMap<_, _>>()
        })
        .collect();
    Rows { headers, rows }
}

fn f(r: &HashMap<String, String>, k: &str) -> f64 {
    r[k].parse().unwrap()
}

fn jpgs(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".jpg"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[allow(clippy::too_many_arguments)]
fn rec(
    r: &mut Recorder,
    now: f64,
    frame_num: i64,
    progress: Option<f64>,
    confs: &[f64],
    ewm: f64,
    baseline: f64,
    short_mean: f64,
    score: f64,
    verdict: &str,
    inference_ms: f64,
    jpeg: &[u8],
) -> Option<String> {
    r.record(&FrameRecord {
        now,
        frame_num,
        progress,
        confidences: confs,
        ewm,
        baseline,
        short_mean,
        score,
        verdict,
        inference_ms,
        annotated_jpeg: Some(jpeg),
    })
}

#[test]
fn print_is_recorded_frame_by_frame() {
    let r = rig();
    r.printer.set("PRINTING", Some(417));
    r.advance(40); // clean
    r.grabber.set_image(Some(solid(255))); // spaghetti
    r.advance(20);

    let hist = r.state_dir().join("history");
    let Rows { headers, rows } = read_rows(&hist.join("job-417.csv"));
    assert_eq!(headers, COLUMNS.to_vec());
    assert_eq!(rows.len() as u64, r.mon.counters().frames_analyzed);
    assert_eq!(
        rows.iter()
            .map(|r| r["frame"].parse::<usize>().unwrap())
            .collect::<Vec<_>>(),
        (1..=rows.len()).collect::<Vec<_>>()
    );
    assert!(
        rows[..40.min(rows.len())].iter().all(|r| f(r, "p") < 0.3),
        "clean frames"
    );
    assert!(rows.iter().map(|r| f(r, "p")).fold(f64::MIN, f64::max) > 1.0);
    assert!(
        rows.iter()
            .any(|r| r["verdict"] == "warning" || r["verdict"] == "failure")
    );
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(hist.join("job-417.json")).unwrap()).unwrap();
    assert_eq!(meta["job_name"], "part-417.bgcode");

    let frames_dir = r.state_dir().join("frames").join("job-417");
    let saved = jpgs(&frames_dir);
    let mut referenced: Vec<String> = rows
        .iter()
        .filter(|r| !r["frame_file"].is_empty())
        .map(|r| r["frame_file"].clone())
        .collect();
    referenced.sort();
    assert!(
        saved == referenced && !saved.is_empty(),
        "every saved frame is referenced by its CSV row"
    );
    for row in &rows {
        if !row["frame_file"].is_empty() {
            assert!(f(row, "p") >= 0.3 || row["verdict"] != "ok");
        }
    }
    assert_eq!(&std::fs::read(frames_dir.join(&saved[0])).unwrap()[..2], b"\xff\xd8");
}

#[test]
fn frame_cap_and_disabled_outputs() {
    let r = rig();
    r.mon.core().recorder.cfg.max_frames_per_job = 3;
    r.printer.set("PRINTING", Some(9));
    r.grabber.set_image(Some(solid(255)));
    r.advance(40);
    assert_eq!(jpgs(&r.state_dir().join("frames").join("job-9")).len(), 3);

    {
        let mut core = r.mon.core();
        core.recorder.cfg.history = false;
        core.recorder.cfg.frames = false;
    }
    r.printer.s().job_id = Some(10);
    r.advance(10);
    assert!(!r.state_dir().join("history").join("job-10.csv").exists());
    assert!(!r.state_dir().join("frames").join("job-10").exists());
}

#[test]
fn restart_mid_print_appends() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = RecordingConfig::default();
    let jpeg = b"\xff\xd8fake";
    let mut r1 = Recorder::new(cfg.clone(), tmp.path());
    r1.start_job(Some(5), Some("a.bgcode"), 1000.0);
    rec(
        &mut r1,
        1000.0,
        1,
        Some(10.0),
        &[0.5],
        0.1,
        0.0,
        0.0,
        0.2,
        "ok",
        90.0,
        jpeg,
    );
    let mut r2 = Recorder::new(cfg, tmp.path()); // process restarted, same print
    r2.start_job(Some(5), Some("a.bgcode"), 2000.0);
    assert_eq!(r2.frames_saved, 1);
    rec(
        &mut r2,
        2000.0,
        1,
        Some(11.0),
        &[],
        0.1,
        0.0,
        0.0,
        0.0,
        "ok",
        90.0,
        jpeg,
    );
    let rows = read_rows(&tmp.path().join("history").join("job-5.csv")).rows;
    assert_eq!(rows.len(), 2); // one header, both rows

    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join("history").join("job-5.json")).unwrap()).unwrap();
    // Python: datetime.fromtimestamp(1000.0).isoformat(timespec="seconds") (local time)
    use chrono::TimeZone;
    let expected = chrono::Local
        .timestamp_opt(1000, 0)
        .single()
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    assert_eq!(meta["started"], expected.as_str(), "restart keeps the original start");
}

fn set_mtime(p: &Path, secs: u64) {
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    let f = std::fs::File::open(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
}

#[test]
fn old_jobs_are_pruned() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = RecordingConfig {
        keep_jobs: 2,
        ..Default::default()
    };
    let mut r = Recorder::new(cfg, tmp.path());
    for (i, job) in [1i64, 2, 3, 4].into_iter().enumerate() {
        let t = 1000.0 + i as f64;
        r.start_job(Some(job), None, t);
        rec(
            &mut r,
            t,
            1,
            None,
            &[0.9],
            0.0,
            0.0,
            0.0,
            0.5,
            "warning",
            50.0,
            b"\xff\xd8x",
        );
        let hist = tmp.path().join("history");
        let mut paths: Vec<_> = std::fs::read_dir(&hist)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&format!("job-{job}."))
            })
            .collect();
        paths.push(tmp.path().join("frames").join(format!("job-{job}")));
        for p in paths {
            set_mtime(&p, 1000 + i as u64);
        }
    }
    r.start_job(Some(5), None, 2000.0);
    let mut left: Vec<String> = std::fs::read_dir(tmp.path().join("history"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".csv"))
        .collect();
    left.sort();
    assert_eq!(
        left,
        vec!["job-4.csv"],
        "keep_jobs=2 -> the current print plus the newest older one"
    );
    let mut frames: Vec<String> = std::fs::read_dir(tmp.path().join("frames"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    frames.sort();
    assert_eq!(frames, vec!["job-4"]);
}

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn recording_failure_never_stops_monitoring() {
    let logs = LogBuf::default();
    let w = logs.clone();
    let sub = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || w.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(sub);

    let r = rig();
    std::fs::write(r.state_dir().join("history"), "not a directory").unwrap();
    std::fs::write(r.state_dir().join("frames"), "not a directory").unwrap();
    r.printer.set("PRINTING", Some(7));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    r.advance(30);
    assert!(
        r.printer.calls().contains(&("pause".to_string(), 7)),
        "detection and escalation still work"
    );
    let text = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(
        r.mon.snapshot().background_io["storage_errors"].as_u64().unwrap() > 0,
        "storage faults must be visible: {text}"
    );
}

fn run_cli(args: &[&str], cwd: &Path) -> std::process::Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_prusa-watch"));
    cmd.args(args).current_dir(cwd);
    for (k, _) in std::env::vars() {
        if k.starts_with("PRUSA_WATCH_") {
            cmd.env_remove(k);
        }
    }
    cmd.output().unwrap()
}

#[test]
fn report_command_summarizes_latest_print() {
    let r = rig();
    r.printer.set("PRINTING", Some(417));
    r.advance(40);
    r.grabber.set_image(Some(solid(255)));
    r.advance(20);

    let state_dir = r.state_dir();
    let conf = state_dir.parent().unwrap().join("c.yaml");
    std::fs::write(&conf, format!("state_dir: '{}'\n", state_dir.display())).unwrap();
    let conf_s = conf.to_string_lossy().into_owned();
    let cwd = r.dir.path();

    let o = run_cli(&["report", "-c", &conf_s], cwd);
    let out = String::from_utf8_lossy(&o.stdout);
    assert_eq!(
        o.status.code(),
        Some(0),
        "stdout:\n{out}\nstderr:\n{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(out.contains("job-417  part-417.bgcode"), "{out}");
    assert!(out.contains("peak p") && out.contains("first warn"), "{out}");

    let o = run_cli(&["report", "999", "-c", &conf_s], cwd);
    assert_eq!(
        o.status.code(),
        Some(1),
        "stderr:\n{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
