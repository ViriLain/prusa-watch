//! Minimal PrusaLink v1 client for Buddy firmware (Core One / MK4 / XL).
//!
//! Auth: PrusaLink on Buddy firmware uses HTTP Digest with username `maker` and
//! the password shown on the printer under Settings > Network > PrusaLink. The
//! same secret is also accepted as an `X-Api-Key` header (what PrusaSlicer
//! uses), selectable via `printer.auth: apikey`.
//!
//! Endpoints used (spec: prusa3d/Prusa-Link-Web spec/openapi.yaml):
//!   GET    /api/v1/status              printer state + current job id
//!   GET    /api/v1/job                 job details (file name, progress)
//!   PUT    /api/v1/job/{id}/pause
//!   PUT    /api/v1/job/{id}/resume
//!   DELETE /api/v1/job/{id}            stop

use std::sync::{Arc, Mutex};
use std::time::Duration;

use md5::{Digest, Md5};
use serde_json::Value;

use crate::http::{HttpRequest, HttpResponse, ReqwestTransport, Transport};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct PrusaLinkError(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct PrinterStatus {
    /// IDLE, BUSY, PRINTING, PAUSED, FINISHED, STOPPED, ERROR, ATTENTION, READY
    pub state: String,
    pub job_id: Option<i64>,
    pub progress: Option<f64>,
    pub time_printing: Option<i64>,
    pub temp_nozzle: Option<f64>,
    pub temp_bed: Option<f64>,
    pub raw: Value,
}

impl PrinterStatus {
    pub fn new(state: &str, job_id: Option<i64>) -> Self {
        Self {
            state: state.into(),
            job_id,
            progress: None,
            time_printing: None,
            temp_nozzle: None,
            temp_bed: None,
            raw: Value::Null,
        }
    }
}

/// What the monitor needs from a printer (the real client, or a fake in tests).
pub trait Printer: Send + Sync {
    fn status(&self) -> Result<PrinterStatus, PrusaLinkError>;
    fn job(&self) -> Result<Option<Value>, PrusaLinkError>;
    fn pause(&self, job_id: i64) -> Result<(), PrusaLinkError>;
    fn resume(&self, job_id: i64) -> Result<(), PrusaLinkError>;
    fn stop(&self, job_id: i64) -> Result<(), PrusaLinkError>;

    fn job_name(&self) -> Option<String> {
        let j = self.job().ok()??;
        let f = j.get("file")?;
        f.get("display_name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| f.get("name").and_then(Value::as_str))
            .map(str::to_string)
    }
}

#[derive(Debug, Clone)]
struct Challenge {
    realm: String,
    nonce: String,
    qop: Option<String>,
    opaque: Option<String>,
    algorithm: Option<String>,
    nc: u32,
}

pub struct PrusaLink {
    base: String,
    username: String,
    password: String,
    apikey: bool,
    timeout: Duration,
    transport: Arc<dyn Transport>,
    challenge: Mutex<Option<Challenge>>,
}

impl PrusaLink {
    pub fn new(host: &str, password: &str, username: &str, auth: &str, scheme: &str, timeout_s: f64) -> Self {
        Self::with_transport(host, password, username, auth, scheme, timeout_s, Arc::new(ReqwestTransport::new()))
    }

    pub fn with_transport(
        host: &str,
        password: &str,
        username: &str,
        auth: &str,
        scheme: &str,
        timeout_s: f64,
        transport: Arc<dyn Transport>,
    ) -> Self {
        let base = if host.starts_with("http://") || host.starts_with("https://") {
            host.to_string()
        } else {
            format!("{scheme}://{host}")
        };
        Self {
            base: base.trim_end_matches('/').to_string(),
            username: username.into(),
            password: password.into(),
            apikey: auth == "apikey",
            timeout: Duration::from_secs_f64(timeout_s.max(0.1)),
            transport,
            challenge: Mutex::new(None),
        }
    }

    fn send(&self, method: &str, path: &str, auth: Option<String>) -> Result<HttpResponse, String> {
        let mut req = HttpRequest::new(method, format!("{}{}", self.base, path)).header("Accept", "application/json");
        if self.apikey {
            req = req.header("X-Api-Key", self.password.clone());
        }
        if let Some(a) = auth {
            req = req.header("Authorization", a);
        }
        self.transport.send(&req, self.timeout)
    }

    fn digest_header(&self, method: &str, uri: &str, ch: &mut Challenge) -> String {
        let md5 = |s: String| hex::encode(Md5::digest(s.as_bytes()));
        let ha1 = md5(format!("{}:{}:{}", self.username, ch.realm, self.password));
        let ha2 = md5(format!("{method}:{uri}"));
        let mut h = format!(r#"Digest username="{}", realm="{}", nonce="{}", uri="{}""#, self.username, ch.realm, ch.nonce, uri);
        let qop_auth = ch.qop.as_deref().is_some_and(|q| q.split(',').any(|x| x.trim() == "auth"));
        if qop_auth {
            ch.nc += 1;
            let nc = format!("{:08x}", ch.nc);
            let cnonce = hex::encode(rand::random::<[u8; 8]>());
            let resp = md5(format!("{ha1}:{}:{nc}:{cnonce}:auth:{ha2}", ch.nonce));
            h.push_str(&format!(r#", response="{resp}", qop=auth, nc={nc}, cnonce="{cnonce}""#));
        } else {
            let resp = md5(format!("{ha1}:{}:{ha2}", ch.nonce));
            h.push_str(&format!(r#", response="{resp}""#));
        }
        if let Some(o) = &ch.opaque {
            h.push_str(&format!(r#", opaque="{o}""#));
        }
        if let Some(a) = &ch.algorithm {
            h.push_str(&format!(", algorithm={a}"));
        }
        h
    }

    fn request(&self, method: &str, path: &str) -> Result<HttpResponse, PrusaLinkError> {
        let wrap = |e: String| PrusaLinkError(format!("{method} {path}: {e}"));
        let mut resp;
        if self.apikey {
            resp = self.send(method, path, None).map_err(wrap)?;
        } else {
            // Reuse the last challenge (nonce count increments); re-challenge on 401.
            let cached = self.challenge.lock().unwrap().clone();
            let mut tried_cached = false;
            if let Some(mut ch) = cached {
                let h = self.digest_header(method, path, &mut ch);
                *self.challenge.lock().unwrap() = Some(ch);
                resp = self.send(method, path, Some(h)).map_err(wrap)?;
                tried_cached = true;
            } else {
                resp = self.send(method, path, None).map_err(wrap)?;
            }
            if resp.status == 401 {
                if let Some(mut ch) = resp.header("www-authenticate").and_then(parse_challenge) {
                    let h = self.digest_header(method, path, &mut ch);
                    *self.challenge.lock().unwrap() = Some(ch);
                    resp = self.send(method, path, Some(h)).map_err(wrap)?;
                } else if tried_cached {
                    *self.challenge.lock().unwrap() = None;
                }
            }
        }
        if resp.status == 401 {
            return Err(PrusaLinkError(format!("{method} {path}: 401 Unauthorized (check printer.password / printer.auth)")));
        }
        if resp.status >= 400 {
            let text: String = resp.text().chars().take(200).collect();
            return Err(PrusaLinkError(format!("{method} {path}: HTTP {} {text}", resp.status)));
        }
        Ok(resp)
    }

    fn json(&self, resp: &HttpResponse, what: &str) -> Result<Value, PrusaLinkError> {
        serde_json::from_slice(&resp.body).map_err(|e| PrusaLinkError(format!("{what}: invalid JSON: {e}")))
    }
}

fn parse_challenge(h: &str) -> Option<Challenge> {
    let rest = h.trim().strip_prefix("Digest").or_else(|| h.trim().strip_prefix("digest"))?;
    let mut params = std::collections::HashMap::new();
    let mut chars = rest.trim().chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let key: String = std::iter::from_fn(|| chars.next_if(|c| *c != '=')).collect();
        if key.is_empty() || chars.next().is_none() {
            break;
        }
        let val: String = if chars.peek() == Some(&'"') {
            chars.next();
            let v: String = std::iter::from_fn(|| chars.next_if(|c| *c != '"')).collect();
            chars.next();
            v
        } else {
            std::iter::from_fn(|| chars.next_if(|c| *c != ',')).collect::<String>().trim().to_string()
        };
        params.insert(key.trim().to_lowercase(), val);
    }
    Some(Challenge {
        realm: params.get("realm").cloned().unwrap_or_default(),
        nonce: params.get("nonce")?.clone(),
        qop: params.get("qop").cloned(),
        opaque: params.get("opaque").cloned(),
        algorithm: params.get("algorithm").cloned(),
        nc: 0,
    })
}

fn f64_of(v: &Value) -> Option<f64> {
    v.as_f64()
}

impl Printer for PrusaLink {
    fn status(&self) -> Result<PrinterStatus, PrusaLinkError> {
        let r = self.request("GET", "/api/v1/status")?;
        let data = self.json(&r, "GET /api/v1/status")?;
        let printer = data.get("printer").filter(|v| v.is_object()).cloned().unwrap_or_default();
        let job = data.get("job").filter(|v| v.is_object()).cloned().unwrap_or_default();
        let state = match printer.get("state") {
            Some(Value::String(s)) => s.to_uppercase(),
            Some(Value::Null) | None => "UNKNOWN".into(),
            Some(other) => other.to_string().to_uppercase(),
        };
        Ok(PrinterStatus {
            state,
            job_id: job.get("id").and_then(Value::as_i64),
            progress: job.get("progress").and_then(f64_of),
            time_printing: job.get("time_printing").and_then(Value::as_i64),
            temp_nozzle: printer.get("temp_nozzle").and_then(f64_of),
            temp_bed: printer.get("temp_bed").and_then(f64_of),
            raw: data,
        })
    }

    fn job(&self) -> Result<Option<Value>, PrusaLinkError> {
        let r = self.request("GET", "/api/v1/job")?;
        if r.status == 204 || r.body.is_empty() {
            return Ok(None);
        }
        self.json(&r, "GET /api/v1/job").map(Some)
    }

    fn pause(&self, job_id: i64) -> Result<(), PrusaLinkError> {
        self.request("PUT", &format!("/api/v1/job/{job_id}/pause"))?;
        tracing::warn!("PrusaLink: paused job {job_id}");
        Ok(())
    }

    fn resume(&self, job_id: i64) -> Result<(), PrusaLinkError> {
        self.request("PUT", &format!("/api/v1/job/{job_id}/resume"))?;
        tracing::info!("PrusaLink: resumed job {job_id}");
        Ok(())
    }

    fn stop(&self, job_id: i64) -> Result<(), PrusaLinkError> {
        self.request("DELETE", &format!("/api/v1/job/{job_id}"))?;
        tracing::warn!("PrusaLink: stopped job {job_id}");
        Ok(())
    }
}

/// Digest response for tests / the fake printer (RFC 2617, MD5, qop=auth or none).
pub fn digest_expected(
    username: &str,
    realm: &str,
    password: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    qop: Option<(&str, &str)>,
) -> String {
    let md5 = |s: String| hex::encode(Md5::digest(s.as_bytes()));
    let ha1 = md5(format!("{username}:{realm}:{password}"));
    let ha2 = md5(format!("{method}:{uri}"));
    match qop {
        Some((nc, cnonce)) => md5(format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}")),
        None => md5(format!("{ha1}:{nonce}:{ha2}")),
    }
}

/// Parse an `Authorization: Digest ...` header into its parameters.
pub fn parse_authorization(h: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(rest) = h.trim().strip_prefix("Digest") else { return out };
    for part in split_params(rest) {
        if let Some((k, v)) = part.split_once('=') {
            out.insert(k.trim().to_lowercase(), v.trim().trim_matches('"').to_string());
        }
    }
    out
}

fn split_params(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                cur.push(c);
            }
            ',' if !in_q => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur);
    }
    parts
}
