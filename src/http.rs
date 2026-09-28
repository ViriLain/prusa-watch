//! A tiny request/response abstraction over the blocking HTTP client, so
//! PrusaLink and the notifiers can be tested with an in-memory transport.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Empty,
    Bytes(Vec<u8>),
    Json(serde_json::Value),
    Multipart(Vec<Part>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl HttpRequest {
    pub fn new(method: &str, url: impl Into<String>) -> Self {
        Self { method: method.into(), url: url.into(), headers: vec![], body: Body::Empty }
    }
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn body(mut self, b: Body) -> Self {
        self.body = b;
        self
    }
    pub fn get_header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self { status, headers: vec![], body: body.into() }
    }
    pub fn json(status: u16, v: serde_json::Value) -> Self {
        Self { status, headers: vec![("content-type".into(), "application/json".into())], body: v.to_string().into_bytes() }
    }
    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub trait Transport: Send + Sync {
    fn send(&self, req: &HttpRequest, timeout: Duration) -> Result<HttpResponse, String>;
}

pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestTransport {
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("prusa-watch/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");
        Self { client }
    }
}

impl Transport for ReqwestTransport {
    fn send(&self, req: &HttpRequest, timeout: Duration) -> Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| e.to_string())?;
        let mut rb = self.client.request(method, &req.url).timeout(timeout);
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        rb = match &req.body {
            Body::Empty => rb,
            Body::Bytes(b) => rb.body(b.clone()),
            Body::Json(v) => rb.json(v),
            Body::Multipart(parts) => {
                let mut form = reqwest::blocking::multipart::Form::new();
                for p in parts {
                    let mut part = reqwest::blocking::multipart::Part::bytes(p.data.clone());
                    if let Some(f) = &p.filename {
                        part = part.file_name(f.clone());
                    }
                    if let Some(ct) = &p.content_type {
                        part = part.mime_str(ct).map_err(|e| e.to_string())?;
                    }
                    form = form.part(p.name.clone(), part);
                }
                rb.multipart(form)
            }
        };
        let resp = rb.send().map_err(|e| describe(&e))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect();
        let body = resp.bytes().map_err(|e| describe(&e))?.to_vec();
        Ok(HttpResponse { status, headers, body })
    }
}

/// reqwest's Display hides the cause ("error sending request"); include it.
pub fn describe(e: &reqwest::Error) -> String {
    use std::error::Error;
    let mut s = if e.is_timeout() { "timed out".to_string() } else { e.to_string() };
    let mut src = e.source();
    while let Some(inner) = src {
        let t = inner.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        src = inner.source();
    }
    s
}
