//! Failure decision logic.
//!
//! Port of Obico's 1st-gen prediction algorithm (obico-server
//! `backend/lib/prediction.py` + `failure_detection.py`, AGPL-3.0).
//!
//! Why not just "pause if any box > 0.5"? Single frames are noisy: purge lines,
//! brims, support trees, and the INDX tool dock can all light up for a frame or
//! two. Obico instead:
//!
//!   * sums box confidences per frame -> p
//!   * tracks an exponential moving average of p (span 12 frames ~= 2 min)
//!   * subtracts a long-run per-printer baseline (rolling mean over 7200 frames
//!     ~= 20 h of printing) so a camera angle that always shows a little "noise"
//!     doesn't trigger
//!   * compares against a short rolling mean for the current print
//!   * gives 30 frames (5 min at 10 s) of grace at print start
//!   * requires the signal to be `escalating_factor` (1.75x) stronger to pause
//!     than to warn.
//!
//! The long baseline persists across prints and restarts (saved to JSON).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::DecisionConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Ok,
    Warning,
    Failure,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Warning => "warning",
            Verdict::Failure => "failure",
        }
    }
}

fn lenient_int<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).ok_or_else(|| serde::de::Error::custom("expected a number"))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PredictionState {
    #[serde(deserialize_with = "lenient_int")]
    pub current_frame_num: i64,
    #[serde(deserialize_with = "lenient_int")]
    pub lifetime_frame_num: i64,
    pub current_p: f64,
    pub ewm_mean: f64,
    pub rolling_mean_short: f64,
    pub rolling_mean_long: f64,
    pub normalized_p: f64,
}

impl PredictionState {
    pub fn reset_for_new_print(&mut self) {
        self.current_frame_num = 0;
        self.current_p = 0.0;
        self.ewm_mean = 0.0;
        self.rolling_mean_short = 0.0;
        self.normalized_p = 0.0;
    }
}

fn next_ewm(p: f64, cur: f64, alpha: f64) -> f64 {
    p * alpha + cur * (1.0 - alpha)
}

fn next_rolling(p: f64, cur: f64, count: i64, win: i64) -> f64 {
    cur + (p - cur) / (if win <= count { win } else { count + 1 }) as f64
}

pub struct FailureDecider {
    pub cfg: DecisionConfig,
    pub state_path: Option<PathBuf>,
    pub state: PredictionState,
}

impl FailureDecider {
    pub fn new(cfg: DecisionConfig, state_path: Option<&Path>) -> Self {
        let state_path = state_path.map(Path::to_path_buf);
        let state = Self::load(&cfg, state_path.as_deref());
        Self { cfg, state_path, state }
    }

    fn load(cfg: &DecisionConfig, path: Option<&Path>) -> PredictionState {
        if let Some(p) = path.filter(|p| p.exists()) {
            match std::fs::read_to_string(p)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
            {
                Ok(s) => return s,
                // corrupt file shouldn't brick the monitor
                Err(e) => tracing::warn!("Decision: could not load state ({e}); starting fresh"),
            }
        }
        PredictionState { lifetime_frame_num: cfg.baseline_prior_frames.max(0), ..Default::default() }
    }

    pub fn save(&self) {
        let Some(path) = &self.state_path else { return };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&self.state).unwrap())?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            tracing::warn!("Decision: could not save state: {e}");
        }
    }

    pub fn reset_for_new_print(&mut self) {
        self.state.reset_for_new_print();
        self.save();
    }

    pub fn update(&mut self, confidences: &[f64]) -> Verdict {
        let c = &self.cfg;
        // fold from +0.0: f64 `sum()` of an empty slice is -0.0 (Python's sum([]) is 0)
        let p: f64 = confidences.iter().fold(0.0, |a, b| a + b);
        let alpha = 2.0 / (c.ewm_span as f64 + 1.0);
        let s = &mut self.state;
        s.current_p = p;
        s.current_frame_num += 1;
        s.lifetime_frame_num += 1;
        s.ewm_mean = next_ewm(p, s.ewm_mean, alpha);
        s.rolling_mean_short = next_rolling(p, s.rolling_mean_short, s.current_frame_num, c.rolling_win_short);
        s.rolling_mean_long = next_rolling(p, s.rolling_mean_long, s.lifetime_frame_num, c.rolling_win_long);
        self.state.normalized_p = self.normalized_p();
        self.save();

        if self.is_failing(self.cfg.escalating_factor) {
            Verdict::Failure
        } else if self.is_failing(1.0) {
            Verdict::Warning
        } else {
            Verdict::Ok
        }
    }

    fn is_failing(&self, escalating_factor: f64) -> bool {
        let (c, s) = (&self.cfg, &self.state);
        if s.current_frame_num < c.init_safe_frame_num {
            return false;
        }
        let adjusted = (s.ewm_mean - s.rolling_mean_long) * c.sensitivity / escalating_factor;
        if adjusted < c.threshold_low {
            return false;
        }
        if adjusted > c.threshold_high {
            return true;
        }
        adjusted > (s.rolling_mean_short - s.rolling_mean_long) * c.rolling_mean_short_multiple
    }

    /// 0..1 score for display: <1/3 fine, 1/3-2/3 warning band, >2/3 failure band.
    fn normalized_p(&self) -> f64 {
        let (c, s) = (&self.cfg, &self.state);
        let scale = |v: f64, lo: f64, hi: f64, nlo: f64, nhi: f64| -> f64 {
            if hi == lo {
                return nlo;
            }
            nhi.min(nlo.max(((v - lo) * (nhi - nlo)) / (hi - lo) + nlo))
        };
        let mut warn = (s.rolling_mean_short - s.rolling_mean_long) * c.rolling_mean_short_multiple;
        warn = c.threshold_high.min(c.threshold_low.max(warn));
        let fail = warn * c.escalating_factor;
        let p = (s.ewm_mean - s.rolling_mean_long) * c.sensitivity;
        if p > fail {
            scale(p, fail, fail * 1.5, 2.0 / 3.0, 1.0)
        } else if p > warn {
            scale(p, warn, fail, 1.0 / 3.0, 2.0 / 3.0)
        } else {
            scale(p, 0.0, warn, 0.0, 1.0 / 3.0)
        }
    }
}
