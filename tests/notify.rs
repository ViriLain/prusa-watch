//! Port of tests/test_notify.py.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prusa_watch::config::NotifyConfig;
use prusa_watch::http::{Body, HttpRequest, HttpResponse, Transport};
use prusa_watch::notify::{Event, Notifier};

/// httpx.MockTransport equivalent: records requests, answers via `handler`.
/// Like httpx, refuses header values that are not valid on the wire.
struct Capture {
    reqs: Mutex<Vec<HttpRequest>>,
    handler: Box<dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync>,
}

impl Capture {
    fn new(handler: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            reqs: Mutex::new(vec![]),
            handler: Box::new(handler),
        })
    }
    fn reqs(&self) -> Vec<HttpRequest> {
        self.reqs.lock().unwrap().clone()
    }
}

impl Transport for Capture {
    fn send(&self, req: &HttpRequest, _t: Duration) -> Result<HttpResponse, String> {
        for (k, v) in &req.headers {
            reqwest::header::HeaderName::from_bytes(k.as_bytes()).map_err(|e| format!("header name {k}: {e}"))?;
            reqwest::header::HeaderValue::from_str(v).map_err(|e| format!("header {k}: {e}"))?;
        }
        self.reqs.lock().unwrap().push(req.clone());
        Ok((self.handler)(req))
    }
}

fn capture() -> Arc<Capture> {
    Capture::new(|_| HttpResponse::new(204, vec![]))
}

fn ev() -> Event {
    let mut e = Event::new("failure", "core-one: failure", "paused", "core-one");
    e.job_id = Some(1);
    e.image_jpeg = Some(Arc::new(b"\xff\xd8jpeg".to_vec()));
    e
}

fn notifier(cfg: NotifyConfig, public_url: &str, t: Arc<dyn Transport>) -> Arc<Notifier> {
    let mut n = Notifier::with_transport(cfg, public_url, "", t);
    n.blocking = true;
    Arc::new(n)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn host(url: &str) -> String {
    reqwest::Url::parse(url).unwrap().host_str().unwrap_or("").to_string()
}

#[test]
fn discord_multipart_with_image() {
    let mut cfg = NotifyConfig::default();
    cfg.discord.webhook_url = "https://discord.example/api/webhooks/1/abc".into();
    let t = capture();
    notifier(cfg, "", t.clone()).send(ev(), None);
    let reqs = t.reqs();
    assert_eq!(reqs.len(), 1);
    // The Content-Type multipart/form-data header is added by the real transport
    // (reqwest) from Body::Multipart; here we check the body shape instead.
    let Body::Multipart(parts) = &reqs[0].body else {
        panic!("expected multipart body, got {:?}", reqs[0].body)
    };
    let mut all = Vec::new();
    for p in parts {
        all.extend_from_slice(p.name.as_bytes());
        all.extend_from_slice(&p.data);
    }
    assert!(parts.iter().any(|p| p.name == "payload_json"));
    assert!(
        contains(&all, b"payload_json") && contains(&all, b"attachment://frame.jpg") && contains(&all, b"\xff\xd8jpeg")
    );
    let img = parts.iter().find(|p| p.data == b"\xff\xd8jpeg").unwrap();
    assert_eq!(img.filename.as_deref(), Some("frame.jpg"));
    assert_eq!(img.content_type.as_deref(), Some("image/jpeg"));
}

#[test]
fn webhook_json_and_ntfy_token_and_buttons() {
    let mut cfg = NotifyConfig::default();
    cfg.webhook.url = "http://ha.lan:8123/api/webhook/prusa".into();
    cfg.ntfy.topic = "t".into();
    cfg.ntfy.token = "tk_123".into();
    let t = capture();
    let mut e = ev();
    e.action_taken = Some("paused".into());
    e.buttons = vec!["resume".into(), "mute".into(), "stop".into()];
    e.incident_id = Some("abc".into());
    e.priority = 4;
    notifier(cfg, "http://watch.lan:8484", t.clone()).send(e, None);
    let reqs = t.reqs();
    assert_eq!(reqs.len(), 2);
    let (ntfy, hook) = (&reqs[0], &reqs[1]);
    assert_eq!(ntfy.get_header("Authorization"), Some("Bearer tk_123"));
    assert_eq!(ntfy.get_header("Priority"), Some("4"));
    let acts = ntfy.get_header("Actions").unwrap();
    // no reply topic -> dashboard fallback URLs; label with a comma is quoted
    assert!(
        acts.contains("http, Resume, http://watch.lan:8484/api/incident/resume?id=abc"),
        "{acts}"
    );
    assert!(!acts.contains("\"False alarm: resume + mute\""), "{acts}"); // no comma in that label, no quoting needed
    let Body::Json(body) = &hook.body else {
        panic!("expected JSON body")
    };
    assert_eq!(body["kind"], "failure");
    assert_eq!(body["has_image"], true);
    assert!(body["image_url"].as_str().unwrap().ends_with("/frame.jpg"));
    assert!(
        body["command_urls"]["veto"]
            .as_str()
            .unwrap()
            .starts_with("http://watch.lan:8484/api/incident/veto?id=abc&expires=")
    );
}

#[test]
fn channel_selection() {
    let mut cfg = NotifyConfig::default();
    cfg.ntfy.topic = "t".into();
    cfg.webhook.url = "http://hook".into();
    let t = capture();
    let n = notifier(cfg, "", t.clone());
    n.send(ev(), Some(&["webhook".to_string()]));
    n.send(ev(), Some(&[]));
    n.send(ev(), Some(&["discord".to_string()])); // not configured -> dropped
    assert_eq!(t.reqs().iter().map(|r| host(&r.url)).collect::<Vec<_>>(), vec!["hook"]);
}

#[test]
fn reply_topic_buttons() {
    let mut cfg = NotifyConfig::default();
    cfg.ntfy.topic = "t".into();
    cfg.ntfy.reply_topic = "r".into();
    let n = Notifier::new(cfg, "", "");
    let mut e = ev();
    e.buttons = vec!["keep".into(), "act".into(), "stop".into()];
    e.incident_id = Some("i1".into());
    e.next_action = Some("stop".into());
    assert_eq!(
        n.ntfy_actions(&e),
        vec![
            "http, Keep printing, https://ntfy.sh/r, method=POST, body=veto i1, clear=true",
            "http, Stop now, https://ntfy.sh/r, method=POST, body=act i1, clear=true",
            "http, Cancel print, https://ntfy.sh/r, method=POST, body=stop i1, clear=true",
        ]
    );
    let mut e2 = ev();
    e2.buttons = vec!["keep".into()];
    assert!(n.ntfy_actions(&e2).is_empty()); // no incident id -> no reply buttons
}

#[test]
fn non_ascii_titles_do_not_crash_headers() {
    let mut cfg = NotifyConfig::default();
    cfg.ntfy.topic = "t".into();
    let t = capture();
    let n = notifier(cfg, "", t.clone());
    let mut e = ev();
    e.title = "Druck fehlgeschlagen – Spaghetti ✗".into();
    n.send(e, None);
    assert_eq!(n.errors.load(Ordering::SeqCst), 0);
    assert_eq!(t.reqs().len(), 1);
}

#[test]
fn channel_failure_is_isolated() {
    let mut cfg = NotifyConfig::default();
    cfg.ntfy.topic = "t".into();
    cfg.webhook.url = "http://hook".into();
    let t = Capture::new(|r| {
        if r.url.contains("hook") {
            HttpResponse::new(500, vec![])
        } else {
            HttpResponse::new(200, vec![])
        }
    });
    let n = notifier(cfg, "", t);
    n.send(ev(), None);
    assert_eq!(n.sent.load(Ordering::SeqCst), 1);
    assert_eq!(n.errors.load(Ordering::SeqCst), 1);
}
