//! prusa-watch: local AI spaghetti detection for Prusa printers + Buddy3D camera.

pub mod camera;
pub mod config;
pub mod decision;
pub mod detector;
pub mod dotenv;
pub mod escalation;
pub mod http;
pub mod imaging;
pub mod model;
pub mod monitor;
pub mod notify;
pub mod policy;
pub mod prusalink;
pub mod recording;
pub mod replies;
pub mod web;

/// Seconds since the Unix epoch as f64 (the clock everything runs on).
pub fn now_ts() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}
