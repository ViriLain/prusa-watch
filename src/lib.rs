//! prusa-watch: local AI spaghetti detection for Prusa printers + Buddy3D camera.

pub mod camera;
pub mod capability;
pub mod config;
pub mod decision;
pub mod detector;
pub mod dotenv;
pub mod escalation;
pub mod heartbeat;
pub mod http;
pub mod imaging;
pub mod model;
pub mod monitor;
pub mod notify;
pub mod policy;
pub mod prusalink;
pub mod recording;
pub mod replies;
pub mod session;
pub mod storage;
pub mod watchdog;
pub mod web;
pub mod worker;

/// Wall time, for schedules and externally visible timestamps.
pub fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// An epoch-shaped monotonic clock shared by frame capture and elapsed deadlines.
pub fn monotonic_ts() -> f64 {
    static ORIGIN: std::sync::OnceLock<(std::time::Instant, f64)> = std::sync::OnceLock::new();
    let (instant, wall) = ORIGIN.get_or_init(|| (std::time::Instant::now(), now_ts()));
    wall + instant.elapsed().as_secs_f64()
}
