//! Port of tests/test_prusalink.py.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use prusa_watch::http::{HttpRequest, HttpResponse, Transport};
use prusa_watch::prusalink::{Printer, PrusaLink, digest_expected, parse_authorization};
use serde_json::json;

fn status_body() -> serde_json::Value {
    json!({
        "job": {"id": 42, "progress": 12.0, "time_remaining": 3600, "time_printing": 600},
        "printer": {"state": "PRINTING", "temp_nozzle": 215.1, "temp_bed": 60.0},
    })
}

/// Mimics Buddy firmware's PrusaLink: digest auth (user 'maker') + X-Api-Key.
struct FakeBuddy {
    password: String,
    calls: Mutex<Vec<(String, String)>>,
}

const REALM: &str = "Printer API";
const NONCE: &str = "abc123";

impl FakeBuddy {
    fn new() -> Arc<Self> {
        Arc::new(Self { password: "secret".into(), calls: Mutex::new(vec![]) })
    }
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
    fn digest_ok(&self, req: &HttpRequest) -> bool {
        let auth = req.get_header("authorization").unwrap_or("");
        if !auth.starts_with("Digest ") {
            return false;
        }
        let p = parse_authorization(auth);
        let (Some(uri), Some(nonce)) = (p.get("uri"), p.get("nonce")) else { return false };
        let qop = if p.contains_key("qop") {
            match (p.get("nc"), p.get("cnonce")) {
                (Some(nc), Some(cn)) => Some((nc.as_str(), cn.as_str())),
                _ => return false,
            }
        } else {
            None
        };
        if p.get("qop").is_some_and(|q| q != "auth") {
            return false;
        }
        let expected = digest_expected("maker", REALM, &self.password, &req.method, uri, nonce, qop);
        p.get("username").map(String::as_str) == Some("maker") && p.get("response") == Some(&expected)
    }
}

fn path_of(url: &str) -> String {
    reqwest::Url::parse(url).unwrap().path().to_string()
}

impl Transport for FakeBuddy {
    fn send(&self, req: &HttpRequest, _t: Duration) -> Result<HttpResponse, String> {
        let authed = req.get_header("x-api-key") == Some(self.password.as_str()) || self.digest_ok(req);
        if !authed {
            return Ok(HttpResponse::new(401, vec![])
                .with_header("WWW-Authenticate", &format!(r#"Digest realm="{REALM}", nonce="{NONCE}", qop="auth""#)));
        }
        let path = path_of(&req.url);
        let m = req.method.as_str();
        self.calls.lock().unwrap().push((m.to_string(), path.clone()));
        Ok(match (m, path.as_str()) {
            ("GET", "/api/v1/status") => HttpResponse::json(200, status_body()),
            ("GET", "/api/v1/job") => HttpResponse::json(200, json!({"id": 42, "file": {"display_name": "benchy.bgcode"}})),
            ("PUT", "/api/v1/job/42/pause") | ("PUT", "/api/v1/job/42/resume") => HttpResponse::new(204, vec![]),
            ("DELETE", "/api/v1/job/42") => HttpResponse::new(204, vec![]),
            (_, p) if p.starts_with("/api/v1/job/") => HttpResponse::json(404, json!({"title": "Not Found"})),
            _ => HttpResponse::new(404, vec![]),
        })
    }
}

fn client_with(fake: &Arc<FakeBuddy>, password: &str, auth: &str) -> PrusaLink {
    PrusaLink::with_transport("192.168.1.50", password, "maker", auth, "http", 5.0, fake.clone())
}

fn client(fake: &Arc<FakeBuddy>) -> PrusaLink {
    client_with(fake, &fake.password.clone(), "digest")
}

#[test]
fn digest_auth_status_and_job() {
    let fake = FakeBuddy::new();
    let pl = client(&fake);
    let st = pl.status().unwrap();
    assert_eq!(st.state, "PRINTING");
    assert_eq!(st.job_id, Some(42));
    assert_eq!(st.progress, Some(12.0));
    assert_eq!(pl.job_name().as_deref(), Some("benchy.bgcode"));
}

#[test]
fn apikey_auth() {
    let fake = FakeBuddy::new();
    let pl = client_with(&fake, "secret", "apikey");
    assert_eq!(pl.status().unwrap().state, "PRINTING");
}

#[test]
fn wrong_password_raises_clear_error() {
    let fake = FakeBuddy::new();
    let pl = client_with(&fake, "nope", "digest");
    let err = pl.status().unwrap_err();
    assert!(err.0.contains("401"), "{err}");
}

#[test]
fn pause_resume_stop_hit_correct_endpoints() {
    let fake = FakeBuddy::new();
    let pl = client(&fake);
    pl.pause(42).unwrap();
    pl.resume(42).unwrap();
    pl.stop(42).unwrap();
    let calls = fake.calls();
    assert!(calls.contains(&("PUT".into(), "/api/v1/job/42/pause".into())));
    assert!(calls.contains(&("PUT".into(), "/api/v1/job/42/resume".into())));
    assert!(calls.contains(&("DELETE".into(), "/api/v1/job/42".into())));
}

#[test]
fn wrong_job_id_raises() {
    let pl = client(&FakeBuddy::new());
    let err = pl.pause(7).unwrap_err();
    assert!(err.0.contains("404"), "{err}");
}

struct Boom;

impl Transport for Boom {
    fn send(&self, _req: &HttpRequest, _t: Duration) -> Result<HttpResponse, String> {
        Err("no route to host".into())
    }
}

#[test]
fn network_error_wrapped() {
    let pl = PrusaLink::with_transport("192.168.1.50", "x", "maker", "digest", "http", 5.0, Arc::new(Boom));
    let err = pl.status().unwrap_err();
    assert!(err.0.contains("no route"), "{err}");
}
