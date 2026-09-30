//! Port of tests/test_web.py: dashboard, state, frames, metrics, auth and incident endpoints.

mod common;

use std::sync::Arc;

use common::*;
use prusa_watch::config::Config;
use prusa_watch::monitor::Monitor;
use serde_json::Value;

/// FastAPI TestClient equivalent: serve the real axum router on 127.0.0.1:0.
struct Client {
    base: String,
    http: reqwest::blocking::Client,
    _rt: tokio::runtime::Runtime,
}

impl Client {
    fn new(mon: Arc<Monitor>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        rt.spawn(async move {
            axum::serve(listener, prusa_watch::web::router(mon)).await.unwrap();
        });
        let http = reqwest::blocking::Client::builder().no_proxy().build().unwrap();
        Self {
            base: format!("http://{addr}"),
            http,
            _rt: rt,
        }
    }
    fn get(&self, path: &str) -> reqwest::blocking::Response {
        self.http.get(format!("{}{path}", self.base)).send().unwrap()
    }
    fn bound(&self, path: &str) -> String {
        if ["/api/pause", "/api/resume", "/api/stop", "/api/mute", "/api/unmute"]
            .iter()
            .any(|route| path.split('?').next() == Some(route))
        {
            let state = self.json("/api/state");
            format!(
                "{path}{}job_id={}&session_id={}",
                if path.contains('?') { "&" } else { "?" },
                state["job"]["job_id"],
                state["job"]["session_id"].as_str().unwrap()
            )
        } else {
            path.into()
        }
    }
    fn post(&self, path: &str) -> reqwest::blocking::Response {
        self.http
            .post(format!("{}{}", self.base, self.bound(path)))
            .send()
            .unwrap()
    }
    fn post_h(&self, path: &str, k: &str, v: &str) -> reqwest::blocking::Response {
        self.http
            .post(format!("{}{}", self.base, self.bound(path)))
            .header(k, v)
            .send()
            .unwrap()
    }
    fn json(&self, path: &str) -> Value {
        self.get(path).json().unwrap()
    }
}

fn bytes(r: reqwest::blocking::Response) -> Vec<u8> {
    r.bytes().unwrap().to_vec()
}

#[test]
fn dashboard_state_frames_metrics_and_auth() {
    let r = rig();
    r.printer.set("PRINTING", Some(21));
    r.advance(5);
    let c = Client::new(r.mon.clone());

    let resp = c.get("/");
    assert_eq!(resp.status(), 200);
    assert!(resp.text().unwrap().contains("prusa-watch"));

    let s = c.json("/api/state");
    assert_eq!(s["printer_state"], "PRINTING");
    assert_eq!(s["job"]["job_id"], 21);
    assert_eq!(s["auth_required"], true);
    assert!(!s["history"].as_array().unwrap().is_empty());

    assert_eq!(&bytes(c.get("/frame.jpg"))[..2], b"\xff\xd8");
    assert_eq!(&bytes(c.get("/raw.jpg"))[..2], b"\xff\xd8");

    let m = c.get("/metrics").text().unwrap();
    assert!(
        m.contains(r#"prusa_watch_printer_state{printer="core-one",state="PRINTING"} 1"#),
        "{m}"
    );
    assert!(m.contains("prusa_watch_frames_analyzed_total"));

    // control endpoints require the token
    assert_eq!(c.post("/api/pause").status(), 401);
    assert_eq!(c.post("/api/pause?token=wrong").status(), 401);
    assert_eq!(c.post("/api/pause?token=tok").status(), 200);
    assert_eq!(r.printer.state(), "PAUSED");
    let j: Value = c.post_h("/api/resume", "X-Token", "tok").json().unwrap();
    assert_eq!(j["result"], "resumed");
    assert_eq!(c.post("/api/mute?token=tok").status(), 200);
    assert!(r.mon.core().job.muted);

    r.grabber.set_image(Some(solid(255)));
    let t: Value = c.post("/api/test?token=tok").json().unwrap();
    assert_eq!(t["detections"].as_array().unwrap().len(), 2);
    assert_eq!(&bytes(c.get("/test.jpg"))[..2], b"\xff\xd8");
    assert_eq!(c.get("/healthz").status(), 200);
}

// --- incident helpers (tests/test_monitor.py: ASK_FIRST, _arm, _spaghetti_until_incident)

const ASK_FIRST: &str = r#"
default_policy: ask_first
snooze_s: 10m
policies:
  ask_first:
    steps:
      - {at: 0, notify: [ntfy], priority: 5, buttons: [keep, act, stop]}
      - {at: 1m, notify: [ntfy], priority: 4, title: "{printer}: reminder, {next_action} in {next_action_in}", attach_image: false}
      - {at: 2m, action: pause, notify: [ntfy, webhook]}
      - {at: 32m, action: stop}
  night: {steps: [{at: 0, action: pause, priority: 2}]}
  watch: {steps: [{at: 0, notify: [ntfy], priority: 3}]}
"#;

fn arm(c: &mut Config) {
    c.escalation = serde_yaml::from_str(ASK_FIRST).unwrap();
    c.notify.ntfy.reply_topic = "reply-xyz".into();
    c.notify.webhook.url = "https://ha.example/api/webhook/pw".into();
    c.timezone = "UTC".into();
}

fn spaghetti_until_incident(r: &Rig, job: i64) -> String {
    r.printer.set("PRINTING", Some(job));
    r.advance(60);
    r.grabber.set_image(Some(solid(255)));
    for _ in 0..40 {
        r.advance(1);
        if let Some(id) = r.mon.core().incident.as_ref().map(|i| i.id.clone()) {
            return id;
        }
    }
    panic!("no incident opened");
}

#[test]
fn incident_endpoints() {
    let r = rig_with(arm);
    let c = Client::new(r.mon.clone());
    assert_eq!(c.post("/api/incident/veto?token=tok").status(), 409); // nothing open
    assert_eq!(c.post("/api/incident/nuke?token=tok").status(), 404);
    let inc = spaghetti_until_incident(&r, 40);
    let s = c.json("/api/state");
    assert_eq!(s["incident"]["id"], inc.as_str());
    let nai = s["incident"]["next_action_in_s"].as_f64().unwrap();
    assert!(0.0 < nai && nai <= 120.0, "{nai}");
    assert_eq!(s["incident"]["commands"], serde_json::json!(["veto", "act", "stop"]));
    assert_eq!(s["policy"]["policy"], "ask_first");
    let m = c.get("/metrics").text().unwrap();
    assert!(
        m.contains(r#"prusa_watch_incident_open{printer="core-one"} 1"#)
            && m.contains("prusa_watch_next_action_seconds"),
        "{m}"
    );
    assert_eq!(c.post(&format!("/api/incident/veto?id={inc}")).status(), 401); // token still required
    assert_eq!(c.post("/api/incident/resume?token=tok").status(), 409); // not applicable yet
    assert_eq!(c.post("/api/incident/act?token=tok").status(), 409);
    let resp = c.post(&format!("/api/incident/act?token=tok&id={inc}"));
    assert_eq!(resp.status(), 200);
    let j: Value = resp.json().unwrap();
    assert_eq!(j["result"], "paused");
    assert_eq!(
        c.json("/api/state")["incident"]["commands"],
        serde_json::json!(["resume", "mute", "stop", "veto"])
    );
    let j: Value = c
        .post(&format!("/api/incident/mute?token=tok&id={inc}"))
        .json()
        .unwrap();
    assert!(j["result"].as_str().unwrap().starts_with("resumed"), "{j}");
    assert!(c.json("/api/state")["incident"].is_null());
}

#[test]
fn controls_reject_stale_owners_and_scoped_links_reject_tampering() {
    let r = rig_with(arm);
    let c = Client::new(r.mon.clone());
    let id = spaghetti_until_incident(&r, 40);
    let owner = r.mon.snapshot().job;
    let bad = format!("/api/pause?job_id=40&session_id={}", owner.session_id);
    r.printer.set("PRINTING", Some(41));
    r.advance(1);
    let response = c
        .http
        .post(format!("{}{bad}", c.base))
        .header("X-Token", "tok")
        .send()
        .unwrap();
    assert_eq!(response.status(), 409);
    assert!(r.printer.calls().is_empty());
    assert_eq!(c.post(&format!("/api/incident/stop?token=tok&id={id}")).status(), 409);
    let expires = prusa_watch::now_ts() as i64 + 60;
    let signature = r.mon.notifier.signer.sign("veto", &id, expires);
    assert_eq!(
        c.post(&format!("/api/incident/stop?id={id}&expires={expires}&cap={signature}"))
            .status(),
        401
    );
    let expired = r.mon.notifier.signer.sign("veto", &id, 0);
    assert_eq!(
        c.post(&format!("/api/incident/veto?id={id}&expires=0&cap={expired}"))
            .status(),
        401
    );
}
