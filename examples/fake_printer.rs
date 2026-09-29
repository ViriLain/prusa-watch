//! Tiny PrusaLink simulator for dry runs (no printer needed).
//!
//! ```text
//! cargo run --example fake_printer -- --port 8081 --password test
//! # then set printer.host: 127.0.0.1:8081, printer.auth: apikey, printer.password: test
//! ```
//!
//! Starts in PRINTING with job id 1. Honors pause/resume/stop from prusa-watch.
//! POST /sim/new_job starts a new job.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use clap::Parser;
use serde_json::json;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 8081)]
    port: u16,
    #[arg(long, default_value = "test")]
    password: String,
}

struct Sim {
    state: String,
    job_id: i64,
    progress: f64,
}

#[derive(Clone)]
struct App {
    sim: Arc<Mutex<Sim>>,
    password: Arc<String>,
}

fn authed(app: &App, h: &HeaderMap) -> Result<(), StatusCode> {
    match h.get("x-api-key").and_then(|v| v.to_str().ok()) {
        Some(k) if k == app.password.as_str() => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

async fn status(State(app): State<App>, h: HeaderMap) -> Response {
    if let Err(r) = authed(&app, &h) {
        return r.into_response();
    }
    let mut s = app.sim.lock().unwrap();
    if s.state == "PRINTING" {
        s.progress = (s.progress + 0.2).min(100.0);
    }
    Json(json!({
        "printer": {"state": s.state, "temp_nozzle": 215.0, "temp_bed": 60.0},
        "job": {"id": s.job_id, "progress": s.progress, "time_printing": 1234},
    }))
    .into_response()
}

async fn job(State(app): State<App>, h: HeaderMap) -> Response {
    if let Err(r) = authed(&app, &h) {
        return r.into_response();
    }
    let s = app.sim.lock().unwrap();
    Json(json!({"id": s.job_id, "state": s.state, "file": {"display_name": format!("sim-part-{}.bgcode", s.job_id)}}))
        .into_response()
}

fn set_state(app: &App, h: &HeaderMap, state: &str) -> Response {
    if let Err(r) = authed(app, h) {
        return r.into_response();
    }
    app.sim.lock().unwrap().state = state.into();
    if state == "PAUSED" {
        println!(">>> PAUSED by prusa-watch");
    }
    StatusCode::NO_CONTENT.into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let app = App {
        sim: Arc::new(Mutex::new(Sim {
            state: "PRINTING".into(),
            job_id: 1,
            progress: 0.0,
        })),
        password: Arc::new(args.password),
    };
    let router = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/job", get(job))
        .route(
            "/api/v1/job/{id}/pause",
            put(|State(a): State<App>, h: HeaderMap, Path(_id): Path<i64>| async move { set_state(&a, &h, "PAUSED") }),
        )
        .route(
            "/api/v1/job/{id}/resume",
            put(
                |State(a): State<App>, h: HeaderMap, Path(_id): Path<i64>| async move { set_state(&a, &h, "PRINTING") },
            ),
        )
        .route(
            "/api/v1/job/{id}",
            delete(
                |State(a): State<App>, h: HeaderMap, Path(_id): Path<i64>| async move { set_state(&a, &h, "STOPPED") },
            ),
        )
        .route(
            "/sim/new_job",
            post(|State(a): State<App>| async move {
                let mut s = a.sim.lock().unwrap();
                s.state = "PRINTING".into();
                s.job_id += 1;
                s.progress = 0.0;
                Json(json!({"state": s.state, "job_id": s.job_id, "progress": s.progress}))
            }),
        )
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await?;
    println!("fake printer on http://127.0.0.1:{}", args.port);
    axum::serve(listener, router).await?;
    Ok(())
}
