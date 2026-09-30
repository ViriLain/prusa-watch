//! Local dashboard + control API + Prometheus metrics.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::monitor::{Monitor, constant_time_eq};

pub const INDEX_HTML: &str = include_str!("static/index.html");

#[derive(Clone)]
pub struct AppState {
    pub monitor: Arc<Monitor>,
    test_jpeg: Arc<Mutex<Option<Vec<u8>>>>,
}

type Q = Query<HashMap<String, String>>;

fn detail(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"detail": msg.into()}))).into_response()
}

#[allow(clippy::result_large_err)] // the Err is the ready-made HTTP response
fn require_token(st: &AppState, q: &HashMap<String, String>, headers: &HeaderMap) -> Result<(), Response> {
    let token = &st.monitor.cfg.web.token;
    if token.is_empty() {
        return Ok(());
    }
    let supplied = q
        .get("token")
        .filter(|s| !s.is_empty())
        .cloned()
        .or_else(|| headers.get("x-token").and_then(|v| v.to_str().ok()).map(str::to_string))
        .unwrap_or_default();
    if constant_time_eq(supplied.as_bytes(), token.as_bytes()) {
        Ok(())
    } else {
        Err(detail(StatusCode::UNAUTHORIZED, "bad or missing token"))
    }
}

fn jpeg(data: Option<Vec<u8>>) -> Response {
    match data.filter(|d| !d.is_empty()) {
        Some(d) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            d,
        )
            .into_response(),
        None => detail(StatusCode::NOT_FOUND, "no frame yet"),
    }
}

/// Run a blocking monitor call off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.expect("blocking task panicked")
}

async fn control(
    st: AppState,
    q: HashMap<String, String>,
    headers: HeaderMap,
    command: String,
    mute: bool,
) -> Response {
    if let Err(r) = require_token(&st, &q, &headers) {
        return r;
    }
    let Some(job) = q.get("job_id").and_then(|id| id.parse::<i64>().ok()) else {
        return detail(StatusCode::CONFLICT, "job id is required; refresh the dashboard");
    };
    let Some(session) = q.get("session_id").filter(|s| !s.is_empty()).cloned() else {
        return detail(StatusCode::CONFLICT, "session id is required; refresh the dashboard");
    };
    let m = st.monitor.clone();
    match blocking(move || m.control(job, &session, &command, mute)).await {
        Ok(result) => Json(json!({"ok": true, "result": result})).into_response(),
        Err(e) if e.0.contains("job/session changed") => detail(StatusCode::CONFLICT, e.to_string()),
        Err(e) => detail(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

pub fn router(monitor: Arc<Monitor>) -> Router {
    let st = AppState {
        monitor,
        test_jpeg: Arc::new(Mutex::new(None)),
    };
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/healthz", get(healthz))
        .route("/livez", get(|| async { Json(json!({"ok": true})) }))
        .route("/api/state", get(api_state))
        .route(
            "/frame.jpg",
            get(|State(st): State<AppState>| async move { jpeg(st.monitor.annotated_jpeg().map(|j| j.to_vec())) }),
        )
        .route(
            "/raw.jpg",
            get(|State(st): State<AppState>| async move { jpeg(blocking(move || st.monitor.raw_jpeg()).await) }),
        )
        .route(
            "/test.jpg",
            get(|State(st): State<AppState>| async move { jpeg(st.test_jpeg.lock().unwrap().clone()) }),
        )
        .route(
            "/api/pause",
            post(|State(st): State<AppState>, q: Q, h: HeaderMap| async move {
                control(st, q.0, h, "pause".into(), false).await
            }),
        )
        .route(
            "/api/resume",
            post(|State(st): State<AppState>, q: Q, h: HeaderMap| async move {
                let mute = q
                    .get("mute")
                    .is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"));
                control(st, q.0, h, "resume".into(), mute).await
            }),
        )
        .route(
            "/api/stop",
            post(|State(st): State<AppState>, q: Q, h: HeaderMap| async move {
                control(st, q.0, h, "stop".into(), false).await
            }),
        )
        .route(
            "/api/mute",
            post(|State(st): State<AppState>, q: Q, h: HeaderMap| async move {
                control(st, q.0, h, "mute".into(), false).await
            }),
        )
        .route(
            "/api/unmute",
            post(|State(st): State<AppState>, q: Q, h: HeaderMap| async move {
                control(st, q.0, h, "unmute".into(), false).await
            }),
        )
        .route("/api/incident/{cmd}", post(api_incident))
        .route("/api/test", post(api_test))
        .route("/metrics", get(metrics))
        .with_state(st)
}

async fn healthz(State(st): State<AppState>) -> Response {
    match st.monitor.readiness() {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "reason": reason})),
        )
            .into_response(),
    }
}

async fn api_state(State(st): State<AppState>) -> Json<Value> {
    let m = &st.monitor;
    let mut d = serde_json::to_value(m.snapshot()).unwrap();
    d["printer_name"] = json!(m.cfg.printer.name);
    // from the immutable config: the control lock is held across PrusaLink calls and inference
    d["sensitivity"] = json!(m.cfg.decision.sensitivity);
    d["auth_required"] = json!(!m.cfg.web.token.is_empty());
    d["history"] = serde_json::to_value(m.history_points()).unwrap();
    d["counters"] = serde_json::to_value(m.counters()).unwrap();
    d["server_time"] = json!(m.now());
    d["history_s"] = json!(m.cfg.web.history_s);
    Json(d)
}

async fn api_incident(State(st): State<AppState>, Path(cmd): Path<String>, q: Q, headers: HeaderMap) -> Response {
    if !crate::replies::COMMANDS.contains(&cmd.as_str()) {
        return detail(StatusCode::NOT_FOUND, "unknown command");
    }
    let m = st.monitor.clone();
    let Some(iid) = q.get("id").filter(|s| !s.is_empty()).cloned() else {
        return detail(StatusCode::CONFLICT, "incident id is required; refresh the dashboard");
    };
    let capability = q
        .get("expires")
        .and_then(|expires| expires.parse::<i64>().ok())
        .zip(q.get("cap"))
        .is_some_and(|(expires, signature)| {
            m.notifier
                .signer
                .verify(&cmd, &iid, expires, signature, crate::now_ts())
        });
    if q.contains_key("cap") || q.contains_key("expires") {
        if !capability {
            return detail(StatusCode::UNAUTHORIZED, "invalid or expired incident capability");
        }
    } else if let Err(response) = require_token(&st, &q, &headers) {
        return response;
    }
    let result = blocking(move || m.handle_reply(&cmd, &iid)).await;
    if result.starts_with("ignored") {
        return detail(StatusCode::CONFLICT, result);
    }
    if result.starts_with("failed") {
        return detail(StatusCode::BAD_GATEWAY, result);
    }
    Json(json!({"ok": true, "result": result})).into_response()
}

async fn api_test(State(st): State<AppState>, q: Q, headers: HeaderMap) -> Response {
    if let Err(r) = require_token(&st, &q, &headers) {
        return r;
    }
    let m = st.monitor.clone();
    let (dets, img) = match blocking(move || m.test_detection()).await {
        Ok(Some(result)) => (result.detections, result.jpeg),
        Ok(None) => return detail(StatusCode::NOT_FOUND, "no camera frame yet"),
        Err(error) => return detail(StatusCode::BAD_GATEWAY, format!("inference failed: {error}")),
    };
    *st.test_jpeg.lock().unwrap() = Some(img);
    let sum: f64 = dets.iter().map(|d| d.confidence).fold(0.0, |a, b| a + b);
    Json(json!({"detections": dets.iter().map(|d| d.as_list()).collect::<Vec<_>>(), "sum_p": sum})).into_response()
}

async fn metrics(State(st): State<AppState>) -> Response {
    let m = &st.monitor;
    let s = m.snapshot();
    let c = m.counters();
    let lbl = format!(
        "printer=\"{}\"",
        m.cfg
            .printer
            .name
            .replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('"', "\\\"")
    );
    let states = [
        "IDLE",
        "BUSY",
        "PRINTING",
        "PAUSED",
        "FINISHED",
        "STOPPED",
        "ERROR",
        "ATTENTION",
        "READY",
        "UNKNOWN",
    ];
    let next_in = s
        .incident
        .as_ref()
        .and_then(|i| i.get("next_action_in_s"))
        .and_then(Value::as_f64)
        .map(|v| format!("{v:.0}"))
        .unwrap_or_else(|| "-1".into());
    let mut lines = vec![
        "# HELP prusa_watch_score Normalized failure score (0-1; >0.33 warning band, >0.66 failure band)".to_string(),
        "# TYPE prusa_watch_score gauge".into(),
        format!("prusa_watch_score{{{lbl}}} {:.4}", s.score),
        "# TYPE prusa_watch_current_p gauge".into(),
        format!("prusa_watch_current_p{{{lbl}}} {:.4}", s.current_p),
        "# TYPE prusa_watch_ewm_mean gauge".into(),
        format!("prusa_watch_ewm_mean{{{lbl}}} {:.4}", s.ewm_mean),
        "# TYPE prusa_watch_baseline gauge".into(),
        format!("prusa_watch_baseline{{{lbl}}} {:.4}", s.baseline),
        "# TYPE prusa_watch_inference_ms gauge".into(),
        format!("prusa_watch_inference_ms{{{lbl}}} {:.1}", s.inference_ms),
        "# TYPE prusa_watch_camera_connected gauge".into(),
        format!("prusa_watch_camera_connected{{{lbl}}} {}", s.camera_connected as u8),
        "# TYPE prusa_watch_frame_age_seconds gauge".into(),
        format!(
            "prusa_watch_frame_age_seconds{{{lbl}}} {}",
            s.frame_age_s.map(py_float).unwrap_or_else(|| "NaN".into())
        ),
        "# TYPE prusa_watch_printer_reachable gauge".into(),
        format!("prusa_watch_printer_reachable{{{lbl}}} {}", s.printer_reachable as u8),
        "# HELP prusa_watch_incident_open 1 while an escalation incident is open".into(),
        "# TYPE prusa_watch_incident_open gauge".into(),
        format!("prusa_watch_incident_open{{{lbl}}} {}", s.incident.is_some() as u8),
        "# HELP prusa_watch_next_action_seconds Seconds until the incident's next pause/stop step (-1 = none)".into(),
        "# TYPE prusa_watch_next_action_seconds gauge".into(),
        format!("prusa_watch_next_action_seconds{{{lbl}}} {next_in}"),
        "# TYPE prusa_watch_printer_state gauge".into(),
    ];
    for st in states {
        lines.push(format!(
            "prusa_watch_printer_state{{{lbl},state=\"{st}\"}} {}",
            (s.printer_state == st) as u8
        ));
    }
    for (name, help, v) in [
        ("frames_analyzed", "Frames run through the model", c.frames_analyzed),
        ("warnings", "Warning notifications sent", c.warnings),
        ("failures", "Failure verdicts", c.failures),
        ("pauses", "Prints paused by prusa-watch", c.pauses),
        ("stops", "Prints stopped by prusa-watch", c.stops),
        ("printer_errors", "PrusaLink request failures", c.printer_errors),
        (
            "vetoes",
            "Pending actions cancelled by the user (Keep printing)",
            c.vetoes,
        ),
        (
            "auto_actions",
            "Pending actions that fired because nobody responded",
            c.auto_actions,
        ),
    ] {
        lines.push(format!("# HELP prusa_watch_{name}_total {help}"));
        lines.push(format!("# TYPE prusa_watch_{name}_total counter"));
        lines.push(format!("prusa_watch_{name}_total{{{lbl}}} {v}"));
    }
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        lines.join("\n") + "\n",
    )
        .into_response()
}

/// Python's repr() for floats: always shows a decimal point (1.0, 0.1, 1e-05).
fn py_float(v: f64) -> String {
    let s = format!("{v:?}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

pub async fn serve(monitor: Arc<Monitor>, host: &str, port: u16) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    axum::serve(listener, router(monitor))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Ctrl-C or SIGTERM (docker stop).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("Shutting down");
}
