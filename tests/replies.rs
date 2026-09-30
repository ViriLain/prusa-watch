//! Port of tests/test_replies.py.

use std::io::BufRead;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use prusa_watch::config::NtfyConfig;
use prusa_watch::replies::{NtfyReplyListener, StreamOpener};
use serde_json::json;

fn pc(s: &str) -> Option<(String, String)> {
    prusa_watch::replies::parse_command(s)
}

fn pair(a: &str, b: &str) -> Option<(String, String)> {
    Some((a.to_string(), b.to_string()))
}

#[test]
fn parse_command() {
    assert_eq!(pc("veto abc123"), pair("veto", "abc123"));
    assert_eq!(pc("  ACT abc "), pair("act", "abc"));
    assert_eq!(pc("stop"), None);
    assert_eq!(pc("rm -rf /"), None);
    assert_eq!(pc("veto a b"), None);
}

type Seen = Arc<Mutex<Vec<(String, String, Vec<(String, String)>)>>>;

struct Canned {
    body: Vec<u8>,
    seen: Seen,
}

impl StreamOpener for Canned {
    fn open(&self, url: &str, since: &str, headers: &[(String, String)]) -> Result<Box<dyn BufRead + Send>, String> {
        self.seen
            .lock()
            .unwrap()
            .push((url.to_string(), since.to_string(), headers.to_vec()));
        Ok(Box::new(std::io::Cursor::new(self.body.clone())))
    }
}

#[test]
fn stream_dispatches_messages_and_tracks_since() {
    let lines = [
        json!({"id": "o1", "event": "open", "topic": "r"}),
        json!({"id": "k1", "event": "keepalive", "topic": "r"}),
        json!({"id": "m1", "event": "message", "topic": "r", "message": "veto abc"}),
        json!({"id": "m2", "event": "message", "topic": "r", "message": "hello there"}),
        json!({"id": "m3", "event": "message", "topic": "r", "message": "act xyz"}),
    ];
    let body = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n") + "\n";
    let seen: Seen = Arc::default();
    let got: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let g = got.clone();
    let cfg = NtfyConfig {
        url: "https://ntfy.example".into(),
        reply_topic: "reply-secret".into(),
        token: "tk_1".into(),
        ..Default::default()
    };
    let lst = NtfyReplyListener::with_opener(
        cfg,
        Arc::new(move |c: &str, i: &str| {
            g.lock().unwrap().push((c.to_string(), i.to_string()));
            "ok".to_string()
        }),
        Arc::new(Canned {
            body: body.into_bytes(),
            seen: seen.clone(),
        }),
    );
    lst.stream_once().unwrap();

    assert_eq!(
        *got.lock().unwrap(),
        vec![
            ("veto".to_string(), "abc".to_string()),
            ("act".to_string(), "xyz".to_string())
        ]
    );
    let seen = seen.lock().unwrap();
    let (url, since, headers) = &seen[0];
    assert_eq!(reqwest::Url::parse(url).unwrap().path(), "/reply-secret/json");
    let auth = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.as_str());
    assert_eq!(auth, Some("Bearer tk_1"));
    // starts from "now", never replays old replies
    assert!(
        !since.is_empty() && since.chars().all(|c| c.is_ascii_digit()),
        "{since}"
    );
    assert_eq!(lst.since(), "m3"); // reconnect resumes after the last message
    assert_eq!(lst.received.load(Ordering::SeqCst), 2);
}

#[test]
fn garbage_lines_are_ignored() {
    let cfg = NtfyConfig {
        reply_topic: "r".into(),
        ..Default::default()
    };
    let lst = NtfyReplyListener::new(cfg, Arc::new(|_: &str, _: &str| "ok".to_string()));
    assert_eq!(lst.handle_line(""), None);
    assert_eq!(lst.handle_line("not json"), None);
    assert_eq!(
        lst.handle_line(&json!({"event": "message", "message": "veto"}).to_string()),
        None
    );
}
