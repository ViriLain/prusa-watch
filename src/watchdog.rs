//! Stall watchdog: if the monitor loop stops completing iterations, exit so the supervisor
//! (Docker restart policy, systemd, launchd) starts a fresh process.
//!
//! A stuck loop can't pause a print, and it can't be trusted to notice that it's stuck.
//! Restarting is safe: the session checkpoint carries an open incident across the restart,
//! and overdue actions are never replayed automatically.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::monitor::Monitor;

/// Exit status used after a stall (EX_SOFTWARE). Nonzero, so `restart: on-failure` and
/// systemd's `Restart=on-failure` also restart.
pub const STALL_EXIT_CODE: i32 = 70;

const MARKER: &str = "restart-reason.json";

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    reason: String,
    at: f64,
}

/// `Some(reason)` if the loop has gone `limit_s` without completing an iteration.
pub fn check(monitor: &Monitor, limit_s: f64) -> Option<String> {
    if monitor.stopping() {
        return None;
    }
    let age = monitor.loop_age_s();
    (age > limit_s).then(|| format!("monitor loop made no progress for {age:.0} s (limit {limit_s:.0} s)"))
}

/// Watch `monitor` from a separate thread; call `on_stall` once when it stalls.
pub fn spawn(monitor: Arc<Monitor>, limit_s: f64, on_stall: impl FnOnce(String) + Send + 'static) -> JoinHandle<()> {
    let period = Duration::from_secs_f64((limit_s / 10.0).clamp(1.0, 10.0));
    std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            loop {
                std::thread::sleep(period);
                if monitor.stopping() {
                    return;
                }
                if let Some(reason) = check(&monitor, limit_s) {
                    on_stall(reason);
                    return;
                }
            }
        })
        .expect("spawn watchdog")
}

const RUNNING: &str = "running.json";

fn marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MARKER)
}

/// Leave a note for the next process, so it can tell the user why it restarted.
pub fn record_restart(state_dir: &Path, reason: &str) -> std::io::Result<()> {
    let marker = Marker {
        reason: reason.to_string(),
        at: crate::now_ts(),
    };
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(marker_path(state_dir), serde_json::to_vec(&marker)?)
}

/// Call at startup. Returns why the previous process ended, if it didn't shut down cleanly
/// (a stall exit, a crash, a kill, or power loss), and marks this process as running.
pub fn begin_run(state_dir: &Path) -> Option<String> {
    let noted = std::fs::read(marker_path(state_dir)).ok().map(|bytes| {
        serde_json::from_slice::<Marker>(&bytes)
            .map(|m| m.reason)
            .unwrap_or_else(|_| "unknown (unreadable restart note)".into())
    });
    let _ = std::fs::remove_file(marker_path(state_dir));
    let unclean = state_dir.join(RUNNING).exists();
    let marker = Marker {
        reason: "running".into(),
        at: crate::now_ts(),
    };
    if let Err(e) = std::fs::create_dir_all(state_dir)
        .and_then(|()| std::fs::write(state_dir.join(RUNNING), serde_json::to_vec(&marker).unwrap_or_default()))
    {
        tracing::warn!("Could not write {}: {e}", state_dir.join(RUNNING).display());
    }
    noted.or_else(|| {
        unclean.then(|| "the previous process ended without shutting down (crash, kill or power loss)".into())
    })
}

/// Call after a clean shutdown.
pub fn end_run(state_dir: &Path) {
    let _ = std::fs::remove_file(state_dir.join(RUNNING));
}
