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
            axum::serve(listener, prusa_watch::web::app(mon)).await.unwrap();
        });
        let http = reqwest::blocking::Client::builder().no_proxy().build().unwrap();
        Self {
            base: format!("http://{addr}"),
            http,
            _rt: rt,
        }
    }
    /// As the dashboard does after unlocking: with the token in a header.
    fn get(&self, path: &str) -> reqwest::blocking::Response {
        self.http
            .get(format!("{}{path}", self.base))
            .header("X-Token", "tok")
            .send()
            .unwrap()
    }
    /// As anyone else who can reach the port: no credentials at all.
    fn anon(&self, path: &str) -> reqwest::blocking::Response {
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
    /// Any method, exactly the headers given.
    fn send(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> reqwest::blocking::Response {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().unwrap()
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

// ------------------------------------------------------------------ who may read and operate

const PRIVATE_READS: [&str; 5] = ["/api/state", "/frame.jpg", "/raw.jpg", "/test.jpg", "/metrics"];
const CONTROLS: [&str; 7] = [
    "/api/pause",
    "/api/resume",
    "/api/stop",
    "/api/mute",
    "/api/unmute",
    "/api/test",
    "/api/incident/stop",
];

/// A print in progress with a frame analyzed and a test image stored: every read has data.
fn printing(tweak: impl FnOnce(&mut Config)) -> (Rig, Client) {
    let r = rig_with(tweak);
    r.printer.set("PRINTING", Some(21));
    r.advance(5);
    let c = Client::new(r.mon.clone());
    assert_eq!(c.send("POST", "/api/test", &[("X-Token", "tok")]).status(), 200);
    (r, c)
}

fn is_jpeg(body: &[u8]) -> bool {
    body.starts_with(b"\xff\xd8")
}

/// Nothing a denied caller receives may describe the printer, the print or the camera.
fn assert_reveals_nothing(path: &str, body: &[u8]) {
    assert!(!is_jpeg(body), "{path}: camera image served");
    let text = String::from_utf8_lossy(body);
    for private in [
        "part-21",
        "core-one",
        "PRINTING",
        "session_id",
        "prusa_watch_",
        "frame_num",
    ] {
        assert!(
            !text.contains(private),
            "{path}: denied response contains {private:?}: {text}"
        );
    }
}

/// The public page is a static shell (its script names states and fields), so what it must
/// not contain is this installation's data.
fn assert_no_live_data(r: &Rig, path: &str, body: &[u8]) {
    assert!(!is_jpeg(body), "{path}: camera image served");
    let text = String::from_utf8_lossy(body);
    let job = r.mon.snapshot().job;
    for private in ["part-21", "core-one", job.session_id.as_str(), "tok\""] {
        assert!(!text.contains(private), "{path}: public response contains {private:?}");
    }
}

#[test]
fn camera_images_state_and_metrics_require_the_token() {
    let (r, c) = printing(|_| {});

    for path in PRIVATE_READS {
        for (what, headers, query) in [
            ("no credentials", vec![], ""),
            ("a wrong header token", vec![("X-Token", "wrong")], ""),
            ("an empty header token", vec![("X-Token", "")], ""),
            ("a wrong bearer token", vec![("Authorization", "Bearer wrong")], ""),
            (
                "the token under the wrong scheme",
                vec![("Authorization", "Basic tok")],
                "",
            ),
            ("a wrong query token", vec![], "?token=wrong"),
            ("a prefix of the token", vec![("X-Token", "to")], ""),
        ] {
            let response = c.send("GET", &format!("{path}{query}"), &headers);
            assert_eq!(response.status(), 401, "{path} with {what}");
            assert_reveals_nothing(path, &bytes(response));
        }
        // the three ways to present the token all work
        for (headers, query) in [
            (vec![("X-Token", "tok")], ""),
            (vec![("Authorization", "Bearer tok")], ""),
            (vec![], "?token=tok"),
        ] {
            let response = c.send("GET", &format!("{path}{query}"), &headers);
            assert_eq!(response.status(), 200, "{path} with {headers:?}{query}");
            assert_eq!(response.headers()["cache-control"], "no-store", "{path}");
            let body = bytes(response);
            assert!(
                is_jpeg(&body) || String::from_utf8_lossy(&body).contains("core-one"),
                "{path}: expected the real content"
            );
        }
    }

    // A route that does not exist is not a way to probe either.
    assert_eq!(c.anon("/api/anything-else").status(), 401);
    drop(r);
}

#[test]
fn public_routes_carry_no_printer_data() {
    let (r, c) = printing(|_| {});
    assert_eq!(
        prusa_watch::web::PUBLIC_ROUTES,
        ["/", "/livez", "/healthz"],
        "a new public route needs a deliberate decision and a test here"
    );
    for path in prusa_watch::web::PUBLIC_ROUTES {
        let response = c.anon(path);
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer", "{path}");
        assert_no_live_data(&r, path, &bytes(response));
    }

    // Readiness stays a usable probe, but only says why to someone allowed to read state.
    r.printer.s().reachable = false;
    r.advance(5);
    let anonymous = c.anon("/healthz");
    assert_eq!(anonymous.status(), 503);
    assert_eq!(anonymous.json::<Value>().unwrap(), serde_json::json!({"ok": false}));
    let owner: Value = c.get("/healthz").json().unwrap();
    assert!(
        owner["reason"].as_str().is_some_and(|reason| !reason.is_empty()),
        "{owner}"
    );
}

#[test]
fn unauthenticated_callers_cannot_operate_the_printer() {
    let (r, c) = printing(|_| {});
    let owner = r.mon.snapshot().job;
    for path in CONTROLS {
        // Even with the right job and session, which only an authorized reader can learn.
        let bound = format!("{path}?job_id=21&session_id={}&id=whatever&mute=1", owner.session_id);
        for headers in [
            vec![],
            vec![("X-Token", "wrong")],
            vec![("Authorization", "Bearer wrong")],
        ] {
            let response = c.send("POST", &bound, &headers);
            assert_eq!(response.status(), 401, "{path} with {headers:?}");
            assert_reveals_nothing(path, &bytes(response));
        }
    }
    assert!(r.printer.calls().is_empty(), "{:?}", r.printer.calls());
    assert_eq!(r.printer.state(), "PRINTING");
    assert!(!r.mon.core().job.muted);
    assert_eq!(c.anon("/test.jpg").status(), 401);
}

#[test]
fn without_a_token_only_this_machine_is_served() {
    let (r, c) = printing(|cfg| cfg.web.token.clear());
    let owner = r.mon.snapshot().job;
    let stop = format!("/api/stop?job_id=21&session_id={}", owner.session_id);

    // A request made on this machine, addressed to this machine: served as before.
    for path in PRIVATE_READS {
        assert_eq!(c.anon(path).status(), 200, "{path}");
    }
    assert_eq!(c.json("/api/state")["auth_required"], false);

    // The same socket, but the request did not originate here.
    for outsider in [
        vec![("Host", "printer.attacker.example")], // DNS rebinding
        vec![("Host", "192.168.1.10:8484")],
        vec![("X-Forwarded-For", "203.0.113.9")], // reverse proxy on this machine
        vec![("Forwarded", "for=203.0.113.9")],
        vec![("X-Real-IP", "203.0.113.9")],
        vec![("X-Forwarded-Server", "edge-1")],
        vec![("CF-Connecting-IP", "203.0.113.9")],
        vec![("X-Original-Forwarded-For", "203.0.113.9")],
        vec![("Via", "1.1 proxy")],
        vec![("Origin", "https://attacker.example")], // another site's page in the user's browser
        vec![("Origin", "http://localhost:3000")],    // another local web app's page
        vec![("Origin", "null")],
    ] {
        for path in PRIVATE_READS {
            let response = c.send("GET", path, &outsider);
            assert_eq!(response.status(), 403, "{path} with {outsider:?}");
            assert_reveals_nothing(path, &bytes(response));
        }
        assert_eq!(c.send("POST", &stop, &outsider).status(), 403, "{outsider:?}");
        assert_eq!(c.send("POST", "/api/mute", &outsider).status(), 403, "{outsider:?}");
    }
    assert!(r.printer.calls().is_empty());
    assert!(!r.mon.core().job.muted);

    // The local browser itself still works, including its own Origin on POST.
    let local_origin = c.base.clone();
    assert_eq!(c.send("POST", &stop, &[("Origin", &local_origin)]).status(), 200);
    assert_eq!(r.printer.state(), "STOPPED");
}

#[test]
fn a_service_started_without_peer_addresses_fails_closed() {
    let r = rig_with(|cfg| cfg.web.token.clear());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    // the bare router: no ConnectInfo, so nobody can be shown to be local
    let bare = prusa_watch::web::router(r.mon.clone());
    rt.spawn(async move { axum::serve(listener, bare).await.unwrap() });
    let http = reqwest::blocking::Client::builder().no_proxy().build().unwrap();
    let status = |path: &str| http.get(format!("http://{addr}{path}")).send().unwrap().status();
    assert_eq!(status("/api/state"), 403);
    assert_eq!(status("/frame.jpg"), 403);
    assert_eq!(status("/livez"), 200);
    assert!(r.printer.calls().is_empty());
}

#[test]
fn explicit_opt_out_is_the_only_way_to_serve_strangers() {
    let (_r, c) = printing(|cfg| {
        cfg.web.token.clear();
        cfg.web.allow_unauthenticated = true;
    });
    let response = c.send("GET", "/api/state", &[("X-Forwarded-For", "203.0.113.9")]);
    assert_eq!(response.status(), 200);
}

#[test]
fn a_frame_link_grants_that_frame_and_nothing_else() {
    let (r, c) = printing(|_| {});
    let link = r.mon.notifier.frame_url();
    let query = link.split_once('?').unwrap().1.to_string();
    assert!(link.starts_with("http://watch.lan:8484/frame.jpg?expires="), "{link}");
    assert!(!link.contains("tok"), "{link}");

    // what the receiver of a webhook does: fetch the frame with no other credentials
    assert!(is_jpeg(&bytes(c.anon(&format!("/frame.jpg?{query}")))));

    // the same permission anywhere else
    for path in ["/api/state", "/raw.jpg", "/test.jpg", "/metrics"] {
        let response = c.anon(&format!("{path}?{query}"));
        assert_eq!(response.status(), 401, "{path}");
        assert_reveals_nothing(path, &bytes(response));
    }
    let owner = r.mon.snapshot().job;
    for path in CONTROLS {
        let bound = format!("{path}?job_id=21&session_id={}&id=x&{query}", owner.session_id);
        assert_eq!(c.send("POST", &bound, &[]).status(), 401, "{path}");
    }
    // an invalid capability is refused even next to a valid token: it is never a fallback
    assert_eq!(c.get("/frame.jpg?expires=1&cap=00").status(), 401);

    // expired, altered, or signed for something else
    let signer = &r.mon.notifier.signer;
    let now = prusa_watch::now_ts() as i64;
    let read = prusa_watch::capability::READ_FRAME;
    for (expires, cap) in [
        (now - 1, signer.sign(read, "", now - 1)),
        (now + 86400 * 2, signer.sign(read, "", now + 86400 * 2)),
        (now + 61, signer.sign(read, "", now + 60)),
        (now + 60, signer.sign("veto", "", now + 60)),
        (now + 60, signer.sign(read, "other", now + 60)),
        (
            now + 60,
            prusa_watch::capability::Signer::default().sign(read, "", now + 60),
        ),
    ] {
        let response = c.anon(&format!("/frame.jpg?expires={expires}&cap={cap}"));
        assert_eq!(response.status(), 401, "expires={expires}");
        assert_reveals_nothing("/frame.jpg", &bytes(response));
    }
    assert!(r.printer.calls().is_empty());
}

#[test]
fn api_error_bodies_carry_no_secret() {
    let (r, c) = printing(|cfg| {
        cfg.printer.host = "http://operator-synth:SYNTH-PRINTER-URL-PASS@printer.lan".into();
        cfg.printer.password = "SYNTH-PRUSALINK-PASS".into();
    });
    r.printer.s().status_error = Some(
        "GET /api/v1/status: error sending request for url \
         (http://operator-synth:SYNTH-PRINTER-URL-PASS@printer.lan/api/v1/status), X-Api-Key: SYNTH-PRUSALINK-PASS"
            .into(),
    );
    let response = c.post_h("/api/pause", "X-Token", "tok");
    assert_eq!(response.status(), 502);
    let body = response.text().unwrap();
    for secret in ["SYNTH-PRINTER-URL-PASS", "SYNTH-PRUSALINK-PASS", "operator-synth"] {
        assert!(!body.contains(secret), "error body exposes {secret}: {body}");
    }
    assert!(body.contains("http://***@printer.lan/***"), "{body}");
}
