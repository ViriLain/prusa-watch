//! Per-print recording of what the detector saw, for tuning.
//!
//! ```text
//! state_dir/history/job-<id>.csv        one row per analyzed frame
//! state_dir/history/job-<id>.json       job name + when recording started
//! state_dir/frames/job-<id>/*.jpg       annotated frames where the model saw something
//! ```
//!
//! The dashboard's score history lives in memory and is gone once the process
//! stops; these files are what you look at after a test print (or a false alarm)
//! to decide whether to change `decision.sensitivity` or `camera.roi`.
//!
//! Recording must never take the monitor down: every write is best-effort and
//! logs on failure.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};
use serde_json::json;

use crate::config::RecordingConfig;

pub const COLUMNS: [&str; 14] = [
    "time",
    "ts",
    "frame",
    "progress",
    "boxes",
    "p",
    "max_conf",
    "ewm",
    "baseline",
    "short_mean",
    "score",
    "verdict",
    "inference_ms",
    "frame_file",
];

fn job_key(job_id: Option<i64>) -> String {
    match job_id {
        Some(id) => format!("job-{id}"),
        None => "job-unknown".into(),
    }
}

fn local_dt(ts: f64) -> chrono::DateTime<Local> {
    Local.timestamp_opt(ts.floor() as i64, ((ts - ts.floor()) * 1e9) as u32).single().unwrap_or_else(Local::now)
}

pub fn iso_local(ts: f64) -> String {
    local_dt(ts).format("%Y-%m-%dT%H:%M:%S").to_string()
}

pub struct Recorder {
    pub cfg: RecordingConfig,
    pub history_dir: PathBuf,
    pub frames_dir: PathBuf,
    pub key: Option<String>,
    pub frames_saved: i64,
}

/// Everything recorded about one analyzed frame.
pub struct FrameRecord<'a> {
    pub now: f64,
    pub frame_num: i64,
    pub progress: Option<f64>,
    pub confidences: &'a [f64],
    pub ewm: f64,
    pub baseline: f64,
    pub short_mean: f64,
    pub score: f64,
    pub verdict: &'a str,
    pub inference_ms: f64,
    pub annotated_jpeg: Option<&'a [u8]>,
}

impl Recorder {
    pub fn new(cfg: RecordingConfig, state_dir: impl AsRef<Path>) -> Self {
        let d = state_dir.as_ref();
        Self { cfg, history_dir: d.join("history"), frames_dir: d.join("frames"), key: None, frames_saved: 0 }
    }

    pub fn csv_path(&self) -> Option<PathBuf> {
        self.key.as_ref().map(|k| self.history_dir.join(format!("{k}.csv")))
    }

    pub fn job_frames_dir(&self) -> Option<PathBuf> {
        self.key.as_ref().map(|k| self.frames_dir.join(k))
    }

    /// Called when a new print is detected (also after a restart mid-print: files are appended to).
    pub fn start_job(&mut self, job_id: Option<i64>, job_name: Option<&str>, now: f64) {
        self.key = Some(job_key(job_id));
        self.frames_saved = 0;
        let key = self.key.clone().unwrap();
        let res = (|| -> std::io::Result<()> {
            if self.cfg.history {
                fs::create_dir_all(&self.history_dir)?;
                let meta = self.history_dir.join(format!("{key}.json"));
                if !meta.exists() {
                    let body = json!({"job_id": job_id, "job_name": job_name, "started": iso_local(now)});
                    fs::write(&meta, serde_json::to_string_pretty(&body).unwrap())?;
                }
            }
            if self.cfg.frames {
                let dir = self.frames_dir.join(&key);
                if dir.exists() {
                    self.frames_saved = fs::read_dir(&dir)?
                        .filter_map(Result::ok)
                        .filter(|e| e.path().extension().is_some_and(|x| x == "jpg"))
                        .count() as i64;
                }
            }
            self.prune()
        })();
        if let Err(e) = res {
            tracing::warn!("Recording: could not start {key}: {e}");
        }
    }

    /// Append one analyzed frame. Returns the saved frame's file name, if any.
    pub fn record(&mut self, r: &FrameRecord) -> Option<String> {
        let key = self.key.clone()?;
        let p: f64 = r.confidences.iter().fold(0.0, |a, b| a + b);
        let mut frame_file = None;
        if self.cfg.frames
            && r.annotated_jpeg.is_some_and(|j| !j.is_empty())
            && self.frames_saved < self.cfg.max_frames_per_job
            && (r.verdict != "ok" || p >= self.cfg.frame_min_p)
        {
            let stamp = local_dt(r.now).format("%Y%m%d-%H%M%S");
            let name = format!("{stamp}_f{:05}_{}_p{p:.2}.jpg", r.frame_num, r.verdict);
            let dir = self.frames_dir.join(&key);
            match fs::create_dir_all(&dir).and_then(|_| fs::write(dir.join(&name), r.annotated_jpeg.unwrap())) {
                Ok(()) => {
                    self.frames_saved += 1;
                    frame_file = Some(name);
                }
                Err(e) => tracing::warn!("Recording: could not save frame: {e}"),
            }
        }
        if self.cfg.history {
            let max_conf = r.confidences.iter().cloned().fold(0.0f64, f64::max);
            let row = [
                iso_local(r.now),
                format!("{:.1}", r.now),
                r.frame_num.to_string(),
                r.progress.map(|v| format!("{v:.1}")).unwrap_or_default(),
                r.confidences.len().to_string(),
                format!("{p:.4}"),
                format!("{max_conf:.4}"),
                format!("{:.4}", r.ewm),
                format!("{:.4}", r.baseline),
                format!("{:.4}", r.short_mean),
                format!("{:.4}", r.score),
                r.verdict.to_string(),
                format!("{:.0}", r.inference_ms),
                frame_file.clone().unwrap_or_default(),
            ];
            let path = self.history_dir.join(format!("{key}.csv"));
            let res = (|| -> Result<(), Box<dyn std::error::Error>> {
                fs::create_dir_all(&self.history_dir)?;
                let new = !path.exists();
                let f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
                let mut w = csv::WriterBuilder::new().terminator(csv::Terminator::CRLF).from_writer(f);
                if new {
                    w.write_record(COLUMNS)?;
                }
                w.write_record(&row)?;
                w.flush()?;
                Ok(())
            })();
            if let Err(e) = res {
                tracing::warn!("Recording: could not write history: {e}");
            }
        }
        frame_file
    }

    fn prune(&self) -> std::io::Result<()> {
        let keep = self.cfg.keep_jobs;
        if keep <= 0 {
            return Ok(());
        }
        let mut jobs: HashMap<String, std::time::SystemTime> = HashMap::new();
        let mut note = |name: String, path: &Path| {
            if let Ok(m) = fs::metadata(path).and_then(|m| m.modified()) {
                let e = jobs.entry(name).or_insert(m);
                if m > *e {
                    *e = m;
                }
            }
        };
        if let Ok(entries) = fs::read_dir(&self.history_dir) {
            for e in entries.filter_map(Result::ok) {
                let p = e.path();
                let (Some(stem), Some(ext)) = (p.file_stem().and_then(|s| s.to_str()), p.extension().and_then(|s| s.to_str()))
                else {
                    continue;
                };
                if stem.starts_with("job-") && (ext == "csv" || ext == "json") {
                    note(stem.to_string(), &p);
                }
            }
        }
        if let Ok(entries) = fs::read_dir(&self.frames_dir) {
            for e in entries.filter_map(Result::ok) {
                let p = e.path();
                if p.is_dir()
                    && let Some(n) = p.file_name().and_then(|s| s.to_str()).filter(|n| n.starts_with("job-"))
                {
                    note(n.to_string(), &p);
                }
            }
        }
        if let Some(k) = &self.key {
            jobs.remove(k); // never prune the print we're recording
        }
        let mut sorted: Vec<(String, std::time::SystemTime)> = jobs.into_iter().collect();
        sorted.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
        let old: Vec<String> = sorted.into_iter().skip((keep - 1).max(0) as usize).map(|(k, _)| k).collect();
        for key in &old {
            for ext in ["csv", "json"] {
                let _ = fs::remove_file(self.history_dir.join(format!("{key}.{ext}")));
            }
            let _ = fs::remove_dir_all(self.frames_dir.join(key));
        }
        if !old.is_empty() {
            tracing::info!("Recording: pruned {} old job(s)", old.len());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    pub frames: usize,
    pub from: String,
    pub to: String,
    pub peak_p: f64,
    pub peak_p_at: String,
    pub peak_score: f64,
    pub baseline: f64,
    pub verdicts: [(String, usize); 3],
    pub first_warning: Option<String>,
    pub first_failure: Option<String>,
    pub frames_saved: usize,
}

/// Headline numbers for one recorded print (used by `prusa-watch report`).
pub fn summarize(csv_path: &Path) -> anyhow::Result<Summary> {
    let mut rdr = csv::Reader::from_path(csv_path)?;
    let rows: Vec<HashMap<String, String>> = rdr.deserialize().collect::<Result<_, _>>()?;
    let mut s = Summary { verdicts: [("ok".into(), 0), ("warning".into(), 0), ("failure".into(), 0)], ..Default::default() };
    if rows.is_empty() {
        return Ok(s);
    }
    let f = |r: &HashMap<String, String>, k: &str| r.get(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let g = |r: &HashMap<String, String>, k: &str| r.get(k).cloned().unwrap_or_default();
    s.frames = rows.len();
    s.from = g(&rows[0], "time");
    s.to = g(rows.last().unwrap(), "time");
    let mut peak = 0;
    for (i, r) in rows.iter().enumerate() {
        if f(r, "p") > f(&rows[peak], "p") {
            peak = i;
        }
        s.peak_score = s.peak_score.max(f(r, "score"));
        let v = g(r, "verdict");
        if let Some(slot) = s.verdicts.iter_mut().find(|(n, _)| *n == v) {
            slot.1 += 1;
        }
        if v == "warning" && s.first_warning.is_none() {
            s.first_warning = Some(g(r, "time"));
        }
        if v == "failure" && s.first_failure.is_none() {
            s.first_failure = Some(g(r, "time"));
        }
        if !g(r, "frame_file").is_empty() {
            s.frames_saved += 1;
        }
    }
    s.peak_p = f(&rows[peak], "p");
    s.peak_p_at = g(&rows[peak], "time");
    s.baseline = f(rows.last().unwrap(), "baseline");
    Ok(s)
}
