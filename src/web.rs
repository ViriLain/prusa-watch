//! Local dashboard + control API + Prometheus metrics.
//!
//! Access is decided in one place, the `guard` middleware, for every route:
//!
//! - `/`, `/livez` and `/healthz` are public. They carry no printer data: the page is a
//!   static shell, and the probes answer with a status code.
//! - Everything else is private, including any route added later: camera frames, state,
//!   metrics and all controls. With `web.token` set, a request needs that token (the
//!   `X-Token` header, `Authorization: Bearer`, or a `token` query parameter) or a signed
//!   capability for exactly that request. Without a token the service is local-only:
//!   the caller must be this machine, addressing it by a loopback name, with no proxy in
//!   between. `web.allow_unauthenticated` is the explicit opt-out.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::connect_info::IntoMakeServiceWithConnectInfo;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{Value, json};

use crate::monitor::{Monitor, constant_time_eq};

pub const INDEX_HTML: &str = include_str!("static/index.html");

/// Routes that answer without authorization. Nothing here may return printer data.
pub const PUBLIC_ROUTES: [&str; 3] = ["/", "/livez", "/healthz"];

/// Headers a proxy adds. A request carrying one did not originate on this machine,
/// whatever the socket says. Any `x-forwarded-*` header counts as well.
const FORWARDED_HEADERS: [&str; 8] = [
    "forwarded",
    "via",
    "x-real-ip",
    "x-client-ip",
    "x-original-forwarded-for",
    "true-client-ip",
    "cf-connecting-ip",
    "fly-client-ip",
];

#[derive(Clone)]
pub struct AppState {
    pub monitor: Arc<Monitor>,
    test_jpeg: Arc<Mutex<Option<Vec<u8>>>>,
}

/// Whether the caller of a public route would also be allowed to read private data.
#[derive(Clone, Copy)]
struct Caller {
    authorized: bool,
}

type Q = Query<HashMap<String, String>>;

fn detail(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"detail": msg.into()}))).into_response()
}

fn supplied_token(q: &HashMap<String, String>, headers: &HeaderMap) -> String {
    let header_text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header_text("x-token")
        .or_else(|| {
            header_text("authorization").and_then(|v| {
                v.split_once(' ')
                    .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                    .map(|(_, token)| token.trim())
            })
        })
        .map(str::to_string)
        .or_else(|| q.get("token").cloned())
        .unwrap_or_default()
}

/// `host[:port]` names this machine itself: `localhost` or a loopback address, with an
/// optional numeric port and nothing else.
fn names_this_machine(authority: &str) -> bool {
    let (name, port) = if let Some(rest) = authority.strip_prefix('[') {
        match rest.split_once(']') {
            Some((address, "")) => (address, None),
            Some((address, after)) => match after.strip_prefix(':') {
                Some(port) => (address, Some(port)),
                None => return false,
            },
            None => return false,
        }
    } else {
        match authority.rsplit_once(':') {
            Some((name, port)) if !name.contains(':') => (name, Some(port)),
            _ => (authority, None), // no port, or a bare IPv6 address
        }
    };
    if port.is_some_and(|port| port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    (name.eq_ignore_ascii_case("localhost") && !authority.starts_with('['))
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// The request came straight from this machine: a loopback peer, a loopback `Host` (so a
/// rebound DNS name does not qualify), no proxy headers, and, if a browser sent an
/// `Origin`, the page is this dashboard itself. An unknown peer is not local.
///
/// A proxy on this machine that rewrites `Host` and adds no forwarding header looks the
/// same as a local program; nothing here can tell them apart. That is why a proxy
/// deployment needs `web.token`.
fn is_local(peer: Option<SocketAddr>, headers: &HeaderMap) -> bool {
    let text = |name: header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    let forwarded = headers.keys().any(|name| {
        let name = name.as_str();
        name.starts_with("x-forwarded-") || FORWARDED_HEADERS.contains(&name)
    });
    let Some(host) = text(header::HOST) else {
        return false;
    };
    peer.is_some_and(|peer| peer.ip().to_canonical().is_loopback())
        && !forwarded
        && names_this_machine(host)
        && text(header::ORIGIN).is_none_or(|origin| {
            origin
                .split_once("://")
                .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
        })
}

/// A signed capability authorizes exactly one thing: one incident command, or one read of
/// the analyzed frame. It never widens to other routes.
fn capability_allows(st: &AppState, path: &str, q: &HashMap<String, String>) -> bool {
    let (Some(expires), Some(cap)) = (q.get("expires").and_then(|v| v.parse::<i64>().ok()), q.get("cap")) else {
        return false;
    };
    let signer = &st.monitor.notifier.signer;
    let now = crate::now_ts();
    if path == "/frame.jpg" {
        return signer.verify(crate::capability::READ_FRAME, "", expires, cap, now);
    }
    match (path.strip_prefix("/api/incident/"), q.get("id")) {
        (Some(cmd), Some(id)) if crate::replies::COMMANDS.contains(&cmd) => signer.verify(cmd, id, expires, cap, now),
        _ => false,
    }
}

/// The access decision for a private route.
#[allow(clippy::result_large_err)] // the Err is the ready-made HTTP response
fn access(
    st: &AppState,
    path: &str,
    q: &HashMap<String, String>,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<(), Response> {
    let web = &st.monitor.cfg.web;
    if q.contains_key("cap") || q.contains_key("expires") {
        return if capability_allows(st, path, q) {
            Ok(())
        } else {
            Err(detail(StatusCode::UNAUTHORIZED, "invalid or expired capability"))
        };
    }
    if !web.token.is_empty() {
        return if constant_time_eq(supplied_token(q, headers).as_bytes(), web.token.as_bytes()) {
            Ok(())
        } else {
            let mut response = detail(StatusCode::UNAUTHORIZED, "bad or missing token");
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            Err(response)
        };
    }
    if web.allow_unauthenticated || is_local(peer, headers) {
        return Ok(());
    }
    Err(detail(
        StatusCode::FORBIDDEN,
        "set web.token to use prusa-watch from another device or through a proxy",
    ))
}

/// Every request passes through here before it reaches a handler.
async fn guard(State(st): State<AppState>, mut request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let query = Query::<HashMap<String, String>>::try_from_uri(request.uri())
        .map(|q| q.0)
        .unwrap_or_default();
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|info| info.0);
    let decision = access(&st, &path, &query, request.headers(), peer);
    let public = PUBLIC_ROUTES.contains(&path.as_str());
    if let (false, Err(denied)) = (public, &decision) {
        tracing::debug!("Web: denied {} {path} ({})", request.method(), denied.status());
    }
    let mut response = match decision {
        Err(denied) if !public => denied,
        decision => {
            request.extensions_mut().insert(Caller {
                authorized: decision.is_ok(),
            });
            next.run(request).await
        }
    };
    let headers = response.headers_mut();
    // Capability links must not leak through Referer, and private answers must not be cached.
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if !public {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
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

async fn control(st: AppState, q: HashMap<String, String>, command: String, mute: bool) -> Response {
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
        Err(e) => detail(StatusCode::BAD_GATEWAY, st.monitor.redactor.text(&e.0)),
    }
}

/// The service to hand to `axum::serve`: the router plus each connection's peer address,
/// which local-only access depends on.
pub fn app(monitor: Arc<Monitor>) -> IntoMakeServiceWithConnectInfo<Router, SocketAddr> {
    router(monitor).into_make_service_with_connect_info::<SocketAddr>()
}

/// The routes behind the access guard. Serve it through [`app`]: without a peer address no
/// request counts as local, so a token-less configuration answers 403.
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
            post(|State(st): State<AppState>, q: Q| async move { control(st, q.0, "pause".into(), false).await }),
        )
        .route(
            "/api/resume",
            post(|State(st): State<AppState>, q: Q| async move {
                let mute = q
                    .get("mute")
                    .is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"));
                control(st, q.0, "resume".into(), mute).await
            }),
        )
        .route(
            "/api/stop",
            post(|State(st): State<AppState>, q: Q| async move { control(st, q.0, "stop".into(), false).await }),
        )
        .route(
            "/api/mute",
            post(|State(st): State<AppState>, q: Q| async move { control(st, q.0, "mute".into(), false).await }),
        )
        .route(
            "/api/unmute",
            post(|State(st): State<AppState>, q: Q| async move { control(st, q.0, "unmute".into(), false).await }),
        )
        .route("/api/incident/{cmd}", post(api_incident))
        .route("/api/test", post(api_test))
        .route("/metrics", get(metrics))
        // One layer over every route and the fallback: nothing is reachable around it.
        .layer(axum::middleware::from_fn_with_state(st.clone(), guard))
        .with_state(st)
}

/// Readiness for probes. The status code is public; the reason describes the printer and
/// is only given to a caller who may read state.
async fn healthz(State(st): State<AppState>, Extension(caller): Extension<Caller>) -> Response {
    match st.monitor.readiness() {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(reason) if caller.authorized => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "reason": reason})),
        )
            .into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false}))).into_response(),
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

/// Authorized by the access guard: the dashboard token, or a capability for this command and incident.
async fn api_incident(State(st): State<AppState>, Path(cmd): Path<String>, q: Q) -> Response {
    if !crate::replies::COMMANDS.contains(&cmd.as_str()) {
        return detail(StatusCode::NOT_FOUND, "unknown command");
    }
    let m = st.monitor.clone();
    let Some(iid) = q.get("id").filter(|s| !s.is_empty()).cloned() else {
        return detail(StatusCode::CONFLICT, "incident id is required; refresh the dashboard");
    };
    let result = blocking(move || m.handle_reply(&cmd, &iid)).await;
    if result.starts_with("ignored") {
        return detail(StatusCode::CONFLICT, result);
    }
    if result.starts_with("failed") {
        return detail(StatusCode::BAD_GATEWAY, result);
    }
    Json(json!({"ok": true, "result": result})).into_response()
}

async fn api_test(State(st): State<AppState>) -> Response {
    let m = st.monitor.clone();
    let m_redactor = m.redactor.clone();
    let (dets, img) = match blocking(move || m.test_detection()).await {
        Ok(Some(result)) => (result.detections, result.jpeg),
        Ok(None) => return detail(StatusCode::NOT_FOUND, "no camera frame yet"),
        Err(error) => {
            return detail(
                StatusCode::BAD_GATEWAY,
                format!("inference failed: {}", m_redactor.text(&error)),
            );
        }
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
    axum::serve(listener, app(monitor))
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

#[cfg(test)]
mod tests {
    use super::names_this_machine;

    #[test]
    fn only_loopback_names_with_a_plain_port_are_this_machine() {
        for local in [
            "localhost",
            "LOCALHOST:8484",
            "127.0.0.1",
            "127.0.0.1:8484",
            "127.8.9.10:1",
            "[::1]",
            "[::1]:8484",
            "::1",
            "[::ffff:127.0.0.1]:80",
        ] {
            assert!(names_this_machine(local), "{local}");
        }
        for other in [
            "",
            "example.com",
            "192.168.1.10:8484",
            "localhost.",
            "localhost.attacker.example",
            "127.0.0.1.attacker.example",
            "localhost:80@attacker.example",
            "localhost:",
            "localhost:80a",
            "[::1]attacker.example",
            "[localhost]",
            "[::1",
            "[::1]:",
            "0.0.0.0:8484",
            "attacker.example:127.0.0.1",
        ] {
            assert!(!names_this_machine(other), "{other}");
        }
    }
}
