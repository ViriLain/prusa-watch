//! Port of tests/test_replies.py, extended: a reply is a command only when it carries a
//! signature this installation issued for that command and incident, and it has not expired.

mod common;

use std::io::BufRead;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use common::*;
use prusa_watch::capability::Signer;
use prusa_watch::config::NtfyConfig;
use prusa_watch::replies::{NtfyReplyListener, Reply, StreamOpener, parse_command};
use serde_json::json;

const SIGNATURE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn parse_command_requires_all_four_fields() {
    let parsed = parse_command(&format!("  VETO abc123 1700000000 {SIGNATURE} ")).unwrap();
    assert_eq!(
        parsed,
        Reply {
            command: "veto".into(),
            incident: "abc123".into(),
            expires: 1_700_000_000,
            signature: SIGNATURE.into(),
        }
    );
    for text in [
        "veto abc123", // the unsigned format older versions accepted
        "stop",
        "rm -rf /",
        "veto a b",
        &format!("veto abc123 soon {SIGNATURE}"),
        &format!("nuke abc123 1700000000 {SIGNATURE}"),
        &format!("veto abc123 1700000000 {SIGNATURE} extra"),
        "",
    ] {
        assert_eq!(parse_command(text), None, "{text:?}");
    }
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

fn now() -> i64 {
    prusa_watch::now_ts() as i64
}

/// The body a button for `command` on `incident` posts, signed by `signer`.
fn signed(signer: &Signer, command: &str, incident: &str, expires: i64) -> String {
    format!(
        "{command} {incident} {expires} {}",
        signer.sign(command, incident, expires)
    )
}

fn message(id: &str, text: &str) -> String {
    json!({"id": id, "event": "message", "topic": "r", "message": text}).to_string()
}

type Got = Arc<Mutex<Vec<(String, String)>>>;

fn listener(signer: &Signer) -> (Arc<NtfyReplyListener>, Got) {
    let got: Got = Arc::default();
    let sink = got.clone();
    let cfg = NtfyConfig {
        reply_topic: "r".into(),
        ..Default::default()
    };
    let listener = NtfyReplyListener::new(
        cfg,
        signer.clone(),
        Arc::new(move |command: &str, incident: &str| {
            sink.lock().unwrap().push((command.to_string(), incident.to_string()));
            "ok".to_string()
        }),
    );
    (listener, got)
}

#[test]
fn stream_dispatches_signed_messages_and_tracks_since() {
    let signer = Signer::default();
    let later = now() + 600;
    let lines = [
        json!({"id": "o1", "event": "open", "topic": "r"}).to_string(),
        json!({"id": "k1", "event": "keepalive", "topic": "r"}).to_string(),
        message("m1", &signed(&signer, "veto", "abc", later)),
        message("m2", "hello there"),
        message("m3", "stop abc"), // unsigned: what anyone who knows the topic could post
        message("m4", &signed(&signer, "act", "xyz", later)),
    ];
    let seen: Seen = Arc::default();
    let got: Got = Arc::default();
    let sink = got.clone();
    let cfg = NtfyConfig {
        url: "https://ntfy.example".into(),
        reply_topic: "reply-secret".into(),
        token: "tk_1".into(),
        ..Default::default()
    };
    let lst = NtfyReplyListener::with_opener(
        cfg,
        signer,
        Arc::new(move |c: &str, i: &str| {
            sink.lock().unwrap().push((c.to_string(), i.to_string()));
            "ok".to_string()
        }),
        Arc::new(Canned {
            body: (lines.join("\n") + "\n").into_bytes(),
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
    // prusa-watch reads the topic with its own token; that token never leaves this process
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
    assert_eq!(lst.since(), "m4"); // reconnect resumes after the last message
    assert_eq!(lst.received.load(Ordering::SeqCst), 2);
    assert_eq!(lst.rejected.load(Ordering::SeqCst), 0); // "stop abc" is not even well-formed
}

#[test]
fn garbage_lines_are_ignored() {
    let (lst, got) = listener(&Signer::default());
    assert_eq!(lst.handle_line(""), None);
    assert_eq!(lst.handle_line("not json"), None);
    assert_eq!(
        lst.handle_line(&json!({"event": "message", "message": "veto"}).to_string()),
        None
    );
    assert!(got.lock().unwrap().is_empty());
}

#[test]
fn replies_without_a_valid_scoped_signature_are_dropped() {
    let signer = Signer::default();
    let stranger = Signer::default();
    let later = now() + 600;
    let genuine_veto = signer.sign("veto", "abc", later);
    let forgeries = [
        // no signature at all, or a made-up one
        "stop abc".to_string(),
        format!("stop abc {later} {SIGNATURE}"),
        format!("stop abc {later} {}", "0".repeat(64)),
        format!("stop abc {later} "),
        // signed, but by someone else's key
        signed(&stranger, "stop", "abc", later),
        // a genuine "keep printing" permission moved to another command
        format!("stop abc {later} {genuine_veto}"),
        format!("act abc {later} {genuine_veto}"),
        format!("resume abc {later} {genuine_veto}"),
        // ... to another incident
        format!("veto other {later} {genuine_veto}"),
        // ... or given a longer life
        format!("veto abc {} {genuine_veto}", later + 1),
        // genuine signatures that are out of date, or dated further ahead than is ever issued
        signed(&signer, "stop", "abc", now() - 1),
        signed(&signer, "stop", "abc", 0),
        signed(&signer, "stop", "abc", now() + 86400 * 2),
        // the dashboard's read permission is not a printer command
        format!(
            "stop abc {later} {}",
            signer.sign(prusa_watch::capability::READ_FRAME, "", later)
        ),
    ];
    let (lst, got) = listener(&signer);
    for (index, text) in forgeries.iter().enumerate() {
        assert_eq!(lst.handle_line(&message(&format!("f{index}"), text)), None, "{text}");
        assert!(got.lock().unwrap().is_empty(), "handler ran for {text:?}");
        // dropped messages are still consumed, so a reconnect does not replay them
        assert_eq!(lst.since(), format!("f{index}"), "{text}");
    }
    assert_eq!(lst.received.load(Ordering::SeqCst), 0);
    // the genuine reply still works afterwards
    assert_eq!(
        lst.handle_line(&message("ok", &signed(&signer, "veto", "abc", later))),
        Some("ok".to_string())
    );
    assert_eq!(*got.lock().unwrap(), vec![("veto".to_string(), "abc".to_string())]);
}

#[test]
fn an_expired_genuine_reply_is_reported_but_not_run() {
    let signer = Signer::default();
    let (lst, got) = listener(&signer);
    let expired: Got = Arc::default();
    let sink = expired.clone();
    lst.on_expired(Arc::new(move |command: &str, incident: &str| {
        sink.lock().unwrap().push((command.to_string(), incident.to_string()));
        String::new()
    }));
    // forged and expired-looking: says nothing to anyone
    lst.handle_line(&message("a", &format!("stop abc {} {SIGNATURE}", now() - 60)));
    assert!(expired.lock().unwrap().is_empty());
    // genuine and expired: the person who tapped it hears that it did nothing
    lst.handle_line(&message("b", &signed(&signer, "stop", "abc", now() - 60)));
    assert_eq!(*expired.lock().unwrap(), vec![("stop".to_string(), "abc".to_string())]);
    assert!(got.lock().unwrap().is_empty());
    assert_eq!(lst.rejected.load(Ordering::SeqCst), 2);
}

// ------------------------------------------------------------------ the real buttons, end to end

const SERVICE_TOKEN: &str = "tk_SYNTHETIC_ntfy_service_token_4f1c";

fn reply_rig() -> (Rig, String) {
    let rig = rig_with(|cfg| {
        cfg.escalation = serde_yaml::from_str("{default_policy: ask_first, schedules: []}").unwrap();
        cfg.notify.ntfy.reply_topic = "synthetic-reply-topic-8d2e".into();
        cfg.notify.ntfy.token = SERVICE_TOKEN.into();
    });
    rig.printer.set("PRINTING", Some(7));
    rig.advance(60);
    rig.grabber.set_image(Some(solid(255)));
    for _ in 0..40 {
        rig.advance(1);
        if let Some(id) = rig.mon.incident_id() {
            return (rig, id);
        }
    }
    panic!("no failure incident");
}

/// `label -> body` for every http action of the newest notification that has buttons.
fn buttons(rig: &Rig) -> Vec<(String, String)> {
    let request = rig
        .ntfy()
        .into_iter()
        .rev()
        .find(|request| !header(request, "Actions").is_empty())
        .expect("a notification with buttons");
    header(&request, "Actions")
        .split("; ")
        .filter(|action| action.starts_with("http, "))
        .map(|action| {
            let label = action.split(", ").nth(1).unwrap().to_string();
            let body = action.split("body=").nth(1).unwrap().split(", ").next().unwrap();
            (label, body.to_string())
        })
        .collect()
}

/// The listener exactly as `Monitor::start` builds it, minus the network connection.
fn monitor_listener(rig: &Rig) -> Arc<NtfyReplyListener> {
    rig.mon.reply_listener(Arc::new(Canned {
        body: vec![],
        seen: Seen::default(),
    }))
}

#[test]
fn notification_buttons_carry_no_reusable_credential() {
    let (rig, id) = reply_rig();
    let with_buttons: Vec<_> = rig
        .ntfy()
        .into_iter()
        .filter(|request| !header(request, "Actions").is_empty())
        .collect();
    assert!(!with_buttons.is_empty());
    for request in &with_buttons {
        // prusa-watch authenticates its own publish; that header stays on this connection
        assert_eq!(header(request, "Authorization"), format!("Bearer {SERVICE_TOKEN}"));
        // everything that is delivered to phones and stored by the server is credential-free
        for (name, value) in &request.headers {
            if name.eq_ignore_ascii_case("authorization") {
                continue;
            }
            assert!(!value.contains(SERVICE_TOKEN), "{name} carries the ntfy token: {value}");
            assert!(!value.contains("tok"), "{name} carries the dashboard token: {value}");
            let lower = value.to_lowercase();
            assert!(
                !lower.contains("authorization") && !lower.contains("bearer"),
                "{name}: {value}"
            );
        }
        assert!(
            !String::from_utf8_lossy(&body_bytes(request)).contains(SERVICE_TOKEN),
            "message body carries the ntfy token"
        );
    }
    // What a button does carry: this command, this incident, an expiry and a signature.
    let all = buttons(&rig);
    assert_eq!(
        all.iter().map(|(label, _)| label.as_str()).collect::<Vec<_>>(),
        ["Keep printing", "Pause now", "Cancel print"]
    );
    for ((_, body), command) in all.iter().zip(["veto", "act", "stop"]) {
        let reply = parse_command(body).unwrap_or_else(|| panic!("unparseable button body {body:?}"));
        assert_eq!(
            (reply.command.as_str(), reply.incident.as_str()),
            (command, id.as_str())
        );
        let lifetime = reply.expires - now();
        assert!((3500..=3600).contains(&lifetime), "expires in {lifetime}s");
        assert!(
            rig.mon
                .notifier
                .signer
                .verify(command, &id, reply.expires, &reply.signature, now() as f64)
        );
    }
}

#[test]
fn a_real_button_works_and_edited_copies_of_it_do_not() {
    let (rig, id) = reply_rig();
    let listener = monitor_listener(&rig);
    let all = buttons(&rig);
    let keep = &all.iter().find(|(label, _)| label == "Keep printing").unwrap().1;
    let cancel = &all.iter().find(|(label, _)| label == "Cancel print").unwrap().1;

    // Someone who can read the alert turns "Keep printing" into other commands.
    for forged in [
        keep.replacen("veto", "stop", 1),
        keep.replacen("veto", "act", 1),
        format!("stop {id}"),
        format!("stop {id} {} {SIGNATURE}", now() + 600),
        cancel.replacen(&id, "another-incident", 1),
    ] {
        assert_eq!(listener.handle_line(&message("x", &forged)), None, "{forged}");
        assert!(rig.printer.calls().is_empty(), "{forged} reached the printer");
        assert_eq!(rig.mon.incident_id(), Some(id.clone()), "{forged} changed the incident");
    }

    // The untouched button does what it says.
    assert_eq!(listener.handle_line(&message("y", cancel)), Some("stopped".to_string()));
    assert_eq!(rig.printer.state(), "STOPPED");
    assert_eq!(rig.printer.count("stop", 7), 1);

    // Replaying it afterwards does nothing: the incident it names is closed.
    let replay = listener.handle_line(&message("z", cancel)).unwrap();
    assert!(replay.starts_with("ignored"), "{replay}");
    assert_eq!(rig.printer.count("stop", 7), 1);
}

#[test]
fn an_expired_button_tells_the_user_and_leaves_the_printer_alone() {
    let (rig, id) = reply_rig();
    let listener = monitor_listener(&rig);
    let signer = &rig.mon.notifier.signer;
    let before = rig.sent().len();
    let notices = |rig: &Rig| -> Vec<String> { rig.titles().into_iter().skip(before).collect() };

    // A forged "expired" reply, and a genuine one for an incident that is not open: silence.
    listener.handle_line(&message("a", &format!("stop {id} {} {SIGNATURE}", now() - 60)));
    listener.handle_line(&message("b", &signed(signer, "stop", "a-closed-incident", now() - 60)));
    assert!(notices(&rig).is_empty(), "{:?}", notices(&rig));

    // The real button for the open incident, tapped an hour too late.
    assert_eq!(
        listener.handle_line(&message("c", &signed(signer, "stop", &id, now() - 60))),
        None
    );
    assert!(rig.printer.calls().is_empty());
    assert_eq!(rig.printer.state(), "PRINTING");
    assert_eq!(rig.mon.incident_id(), Some(id.clone()));
    assert_eq!(notices(&rig), ["core-one: that button has expired"]);

    // one notice per minute at most, however often an old alert is tapped
    listener.handle_line(&message("d", &signed(signer, "veto", &id, now() - 60)));
    assert_eq!(notices(&rig).len(), 1);
    assert_eq!(rig.mon.incident_id(), Some(id));
}
