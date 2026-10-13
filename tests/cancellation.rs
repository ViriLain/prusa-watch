//! A cancelled print is reported only on positive evidence from the printer that the
//! intended job stopped. Unknown states, unreadable responses and reports about a
//! different job confirm nothing.
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use prusa_watch::http::{HttpRequest, HttpResponse, Transport};
use prusa_watch::monitor::{Monitor, Parts};
use prusa_watch::prusalink::{Printer, PrusaLink, STATES, parse_status};
use prusa_watch::session::{Action, ENDED_STATES};
use serde_json::json;

/// States no report may use to confirm a stop: unknown, transitional, faulted, still
/// holding the job, over for some other reason, or not PrusaLink states at all.
const NOT_STOPPED: [&str; 15] = [
    "UNKNOWN",
    "",
    "BUSY",
    "ERROR",
    "ATTENTION",
    "PRINTING",
    "PAUSED",
    "FINISHED", // the print completed: the opposite of stopped
    "IDLE",     // nothing is printing, and nothing says why
    "READY",
    "stopped",
    "STOPPING",
    "CANCELLED",
    "0",
    "null",
];

#[test]
fn only_the_printers_stopped_report_for_the_same_job_confirms_a_stop() {
    for state in NOT_STOPPED {
        for observed in [None, Some(7), Some(8)] {
            assert!(
                !Action::Stop.confirmed(7, observed, state),
                "{state:?} with job {observed:?} confirmed a stop"
            );
        }
    }
    // Every recognized state other than STOPPED is in that list.
    for state in STATES {
        assert_eq!(state == "STOPPED", !NOT_STOPPED.contains(&state), "{state}");
    }
    // Buddy firmware names no job once a print is stopped; an older report may still name it.
    assert!(Action::Stop.confirmed(7, None, "STOPPED"));
    assert!(Action::Stop.confirmed(7, Some(7), "STOPPED"));
    assert!(
        !Action::Stop.confirmed(7, Some(8), "STOPPED"),
        "STOPPED for another job"
    );
    // Pause and resume are claims about a named job, never about "whatever is there".
    assert!(Action::Pause.confirmed(7, Some(7), "PAUSED"));
    assert!(!Action::Pause.confirmed(7, None, "PAUSED"));
    assert!(!Action::Pause.confirmed(7, Some(8), "PAUSED"));
    assert!(Action::Resume.confirmed(7, Some(7), "PRINTING"));
    assert!(!Action::Resume.confirmed(7, None, "PRINTING"));
    assert!(!Action::Resume.confirmed(7, Some(8), "PRINTING"));
}

/// Job 7 printing with an open ask-first incident: nothing has touched the printer yet.
fn failing_print() -> (Rig, String) {
    failing_print_with("{default_policy: ask_first, schedules: []}", |_| {}, |_| {})
}

/// The same failure under any escalation policy. `tweak` adjusts the config and `arrange`
/// the printer before the incident opens (a policy may act the moment it does).
fn failing_print_with(
    escalation: &str,
    tweak: impl FnOnce(&mut prusa_watch::config::Config),
    arrange: impl FnOnce(&Rig),
) -> (Rig, String) {
    let rig = rig_with(|cfg| {
        cfg.escalation = serde_yaml::from_str(escalation).unwrap();
        tweak(cfg);
    });
    rig.printer.set("PRINTING", Some(7));
    rig.advance(60);
    arrange(&rig);
    rig.grabber.set_image(Some(solid(255)));
    for _ in 0..40 {
        rig.advance(1);
        if let Some(id) = rig.mon.incident_id() {
            return (rig, id);
        }
    }
    panic!("no failure incident");
}

fn count_titles(rig: &Rig, part: &str) -> usize {
    rig.titles().iter().filter(|title| title.contains(part)).count()
}

fn assert_not_reported_stopped(rig: &Rig, context: &str) {
    let snapshot = rig.mon.snapshot();
    assert_eq!(snapshot.job.action_taken, None, "{context}: reported as acted");
    assert_eq!(rig.mon.counters().stops, 0, "{context}: counted as a stop");
    assert!(
        !rig.titles().iter().any(|title| title.contains("STOPPED")),
        "{context}: announced as stopped: {:?}",
        rig.titles()
    );
}

#[test]
fn a_cancel_request_is_never_answered_from_an_unknown_state() {
    for (state, job) in [
        ("UNKNOWN", None),
        ("UNKNOWN", Some(7)),
        ("", None),
        ("STOPPING", Some(7)),
        ("null", None),
    ] {
        let (rig, id) = failing_print();
        rig.printer.report(state, job);
        let context = format!("state {state:?} job {job:?}");

        let reply = rig.mon.handle_reply("stop", &id);
        assert!(reply.starts_with("failed"), "{context}: {reply}");
        // The printer could not be read, so nothing was sent and nothing is claimed.
        assert_eq!(rig.printer.count("stop", 7), 0, "{context}");
        assert_eq!(rig.mon.incident_id(), Some(id.clone()), "{context}: incident retired");
        assert_not_reported_stopped(&rig, &context);

        // The dashboard path gets the same answer.
        let session = rig.mon.snapshot().job.session_id;
        assert!(rig.mon.control(7, &session, "stop", false).is_err(), "{context}");
        assert_not_reported_stopped(&rig, &context);

        // The watch loop treats the report as unreadable instead of as "the print is over".
        rig.advance(3);
        assert_eq!(
            rig.mon.incident_id(),
            Some(id),
            "{context}: incident closed by an unknown state"
        );
        let snapshot = rig.mon.snapshot();
        assert!(!snapshot.printer_reachable, "{context}");
        assert!(
            snapshot.printer_error.unwrap().contains("unrecognized printer state"),
            "{context}"
        );
        assert_not_reported_stopped(&rig, &context);
    }
}

#[test]
fn an_accepted_stop_is_confirmed_only_when_the_printer_reports_it() {
    let (rig, id) = failing_print();
    rig.printer.s().ignore_stop = true;
    assert_eq!(
        rig.mon.handle_reply("stop", &id),
        "stop requested; awaiting confirmation"
    );
    assert_eq!(rig.printer.count("stop", 7), 1);
    assert!(rig.mon.snapshot().pending_action.is_some());
    assert_not_reported_stopped(&rig, "accepted");

    // What the firmware reports while it aborts (still PRINTING), then unreadable,
    // transitional, faulted and held reports, well past the confirmation deadline:
    // none of them is evidence of a stop.
    for (state, job) in [
        ("PRINTING", Some(7)),
        ("UNKNOWN", None),
        ("UNKNOWN", Some(7)),
        ("BUSY", Some(7)),
        ("BUSY", None),
        ("ERROR", Some(7)),
        ("ERROR", None),
        ("ATTENTION", Some(7)),
    ] {
        rig.printer.report(state, job);
        rig.advance(2);
        assert!(
            rig.mon.snapshot().pending_action.is_some(),
            "{state}: request forgotten"
        );
        assert_not_reported_stopped(&rig, state);
    }

    // The printer's own STOPPED report settles it, in the form the firmware sends: no job.
    rig.printer.report("STOPPED", None);
    rig.advance(1);
    let snapshot = rig.mon.snapshot();
    assert_eq!(snapshot.job.action_taken.as_deref(), Some("stopped"));
    assert!(snapshot.pending_action.is_none());
    assert_eq!(rig.mon.counters().stops, 1);
    // Whoever asked was told "awaiting confirmation"; they now hear that it happened, once.
    let announced = rig
        .titles()
        .iter()
        .filter(|title| title.contains("print STOPPED"))
        .count();
    assert_eq!(announced, 1, "{:?}", rig.titles());
    rig.advance(3);
    assert_eq!(rig.mon.counters().stops, 1);
}

const ENDED_OTHERWISE: [(&str, Option<i64>); 4] = [
    ("FINISHED", None),
    ("FINISHED", Some(7)),
    ("IDLE", None),
    ("READY", None),
];

#[test]
fn a_print_that_ended_another_way_is_never_reported_as_stopped() {
    // Asked to cancel a print that has already finished, or is simply gone.
    for (state, job) in ENDED_OTHERWISE {
        let (rig, id) = failing_print();
        rig.printer.report(state, job);
        let reply = rig.mon.handle_reply("stop", &id);
        assert!(reply.starts_with("failed"), "{state}: {reply}");
        assert!(rig.printer.calls().is_empty(), "{state}: {:?}", rig.printer.calls());
        assert_not_reported_stopped(&rig, state);
        rig.advance(10);
        assert_not_reported_stopped(&rig, state);
        assert!(rig.mon.snapshot().pending_action.is_none(), "{state}");
    }

    // The stop was accepted, and then the print ended without the printer ever saying STOPPED.
    for (state, job) in ENDED_OTHERWISE {
        let (rig, id) = failing_print();
        rig.printer.s().ignore_stop = true;
        assert!(rig.mon.handle_reply("stop", &id).contains("awaiting confirmation"));
        rig.printer.report(state, job);
        rig.advance(2);
        // still inside the confirmation window: a STOPPED report could yet arrive
        assert!(rig.mon.snapshot().pending_action.is_some(), "{state}");
        assert_not_reported_stopped(&rig, state);
        rig.advance(8);
        // window over: stop waiting, claim nothing, and tell the person who asked
        let snapshot = rig.mon.snapshot();
        assert!(snapshot.pending_action.is_none(), "{state}: still waiting");
        assert!(snapshot.action_error.is_none(), "{state}");
        assert_not_reported_stopped(&rig, state);
        let notices = rig
            .titles()
            .into_iter()
            .filter(|title| title.contains("print ended before the stop was confirmed"))
            .count();
        assert_eq!(notices, 1, "{state}: {:?}", rig.titles());
        assert_eq!(rig.printer.count("stop", 7), 1, "{state}");
    }

    // A stop that had already failed loudly is not turned into a success by the print finishing.
    let (rig, id) = failing_print();
    rig.printer.s().ignore_stop = true;
    assert!(rig.mon.handle_reply("stop", &id).contains("awaiting confirmation"));
    rig.advance(40);
    assert!(rig.titles().iter().any(|title| title.contains("Stop FAILED")));
    rig.printer.report("FINISHED", None);
    rig.advance(3);
    assert_not_reported_stopped(&rig, "finished after a failed stop");
    assert!(rig.mon.snapshot().pending_action.is_none());
}

#[test]
fn a_stop_waiting_in_busy_or_error_is_loud_and_survives_until_it_can_be_retried() {
    for state in ["BUSY", "ERROR"] {
        let (rig, id) = failing_print();
        rig.printer.s().ignore_stop = true;
        assert!(rig.mon.handle_reply("stop", &id).contains("awaiting confirmation"));
        rig.printer.report(state, None);
        rig.advance(360); // an hour in that state
        let snapshot = rig.mon.snapshot();
        assert_not_reported_stopped(&rig, state);
        // nothing can be sent in that state, and the wait is not silent
        assert_eq!(rig.printer.count("stop", 7), 1, "{state}");
        assert!(
            snapshot
                .action_error
                .as_deref()
                .is_some_and(|error| error.contains("not confirmed") && error.contains(state)),
            "{state}: {:?}",
            snapshot.action_error
        );
        assert_eq!(
            count_titles(&rig, "Stop NOT CONFIRMED"),
            1,
            "{state}: {:?}",
            rig.titles()
        );
        assert!(
            rig.mon.readiness().is_err(),
            "{state}: readiness hides the unconfirmed stop"
        );
        // waiting uses up no attempt: BUSY is also what crash recovery mid-print looks like
        assert_eq!(
            snapshot.pending_action.map(|pending| pending.attempts),
            Some(1),
            "{state}"
        );

        // the print comes back: the stop is sent again, and this time the printer obeys
        rig.printer.s().report = None;
        rig.printer.s().ignore_stop = false;
        rig.advance(2);
        assert_eq!(rig.printer.count("stop", 7), 2, "{state}");
        let snapshot = rig.mon.snapshot();
        assert_eq!(snapshot.job.action_taken.as_deref(), Some("stopped"), "{state}");
        assert!(
            snapshot.action_error.is_none() && snapshot.pending_action.is_none(),
            "{state}"
        );
        assert_eq!(count_titles(&rig, "print STOPPED"), 1, "{state}: {:?}", rig.titles());
    }
}

#[test]
fn a_stop_confirmed_by_the_retry_is_announced_and_ends_the_incident() {
    let (rig, id) = failing_print();
    rig.printer.s().ignore_stop = true;
    assert!(rig.mon.handle_reply("stop", &id).contains("awaiting confirmation"));
    rig.advance(5); // still inside the confirmation window
    assert_eq!(rig.printer.count("stop", 7), 1);
    rig.printer.s().ignore_stop = false;
    let before = rig.titles().len();
    rig.advance(1); // deadline: sent again, and the read that follows says STOPPED
    assert_eq!(rig.printer.count("stop", 7), 2);
    assert_eq!(rig.mon.counters().stops, 1);
    assert_eq!(rig.mon.snapshot().job.action_taken.as_deref(), Some("stopped"));
    let after: Vec<String> = rig.titles().into_iter().skip(before).collect();
    assert_eq!(
        after,
        ["core-one: print STOPPED"],
        "one announcement, no reminder for a stopped print"
    );
    assert!(rig.mon.incident_id().is_none());
    rig.advance(3);
    assert_eq!(rig.titles().len(), before + 1);
}

#[test]
fn a_pause_waiting_through_attention_is_sent_again_when_printing_resumes() {
    let (rig, id) = failing_print();
    rig.printer.s().pause_lag_polls = 10_000; // accepted, never carried out
    assert!(rig.mon.handle_reply("act", &id).contains("awaiting confirmation"));
    rig.printer.set_state("ATTENTION"); // e.g. a filament or crash dialog at the printer
    rig.advance(30);
    let snapshot = rig.mon.snapshot();
    assert_eq!(rig.printer.count("pause", 7), 1);
    assert_eq!(snapshot.pending_action.map(|pending| pending.attempts), Some(1));
    assert_eq!(count_titles(&rig, "Pause NOT CONFIRMED"), 1, "{:?}", rig.titles());
    assert_eq!(snapshot.job.action_taken, None);

    rig.printer.s().pause_lag_polls = 0;
    rig.printer.set_state("PRINTING");
    rig.advance(2);
    assert_eq!(rig.printer.count("pause", 7), 2);
    assert_eq!(rig.printer.state(), "PAUSED");
    let snapshot = rig.mon.snapshot();
    assert_eq!(snapshot.job.action_taken.as_deref(), Some("paused"));
    assert!(snapshot.action_error.is_none() && snapshot.pending_action.is_none());
}

#[test]
fn the_outcome_of_a_request_is_reported_where_the_request_came_from() {
    // A person's stop is announced everywhere, even if the policy has a silent stop step
    // of its own later on.
    let later_silent_stop = "{default_policy: p, schedules: [], policies: {p: {steps: \
                             [{at: 0, buttons: [keep, stop]}, {at: 30m, action: stop, notify: []}]}}}";
    let (rig, id) = failing_print_with(later_silent_stop, |_| {}, |_| {});
    rig.printer.s().ignore_stop = true;
    assert!(rig.mon.handle_reply("stop", &id).contains("awaiting confirmation"));
    rig.printer.report("STOPPED", None);
    rig.advance(1);
    assert_eq!(count_titles(&rig, "print STOPPED"), 1, "{:?}", rig.titles());

    // An automatic stop from a silent step stays silent, also when BUSY closes the
    // incident before STOPPED arrives.
    let silent_stop = "{default_policy: p, schedules: [], policies: {p: {steps: [{at: 0, action: stop, notify: []}]}}}";
    let (rig, _) = failing_print_with(silent_stop, |_| {}, |rig| rig.printer.s().ignore_stop = true);
    assert_eq!(rig.printer.count("stop", 7), 1);
    rig.printer.report("BUSY", None);
    rig.advance(1);
    rig.printer.report("STOPPED", None);
    rig.advance(1);
    assert_eq!(rig.mon.counters().stops, 1);
    assert_eq!(count_titles(&rig, "print STOPPED"), 0, "{:?}", rig.titles());

    // ... and a quiet one keeps its channel and priority.
    let quiet_stop = "{default_policy: p, schedules: [], policies: {p: {steps: \
                      [{at: 0, action: stop, notify: [ntfy], priority: 2}]}}}";
    let (rig, _) = failing_print_with(
        quiet_stop,
        |cfg| cfg.notify.webhook.url = "https://ha.example/api/webhook/pw".into(),
        |rig| rig.printer.s().ignore_stop = true,
    );
    rig.printer.report("BUSY", None);
    rig.advance(1);
    rig.transport.clear();
    rig.printer.report("STOPPED", None);
    rig.advance(1);
    let announced = rig.sent();
    assert_eq!(announced.len(), 1, "{:?}", rig.titles());
    assert!(announced[0].url.contains("ntfy.example"));
    assert_eq!(header(&announced[0], "Title"), "core-one: print STOPPED");
    assert_eq!(header(&announced[0], "Priority"), "2");

    // The note about a print that ended unconfirmed is the only word on that request: it
    // is sent even with confirmations (notify.info) switched off.
    let (rig, id) = failing_print_with(
        "{default_policy: ask_first, schedules: []}",
        |cfg| cfg.notify.info.enabled = false,
        |_| {},
    );
    rig.printer.s().ignore_stop = true;
    assert!(rig.mon.handle_reply("stop", &id).contains("awaiting confirmation"));
    rig.printer.report("IDLE", None);
    rig.advance(10);
    assert_eq!(
        count_titles(&rig, "print ended before the stop was confirmed"),
        1,
        "{:?}",
        rig.titles()
    );
}

#[test]
fn a_stop_the_printer_never_acts_on_fails_loudly_after_bounded_retries() {
    let (rig, id) = failing_print();
    rig.printer.s().ignore_stop = true;
    assert!(rig.mon.handle_reply("stop", &id).contains("awaiting confirmation"));
    rig.advance(40);
    assert_eq!(rig.printer.state(), "PRINTING");
    assert_eq!(rig.printer.count("stop", 7), 3, "attempts must be bounded");
    assert!(rig.mon.snapshot().action_error.unwrap().contains("not confirmed"));
    assert!(rig.titles().iter().any(|title| title.contains("FAILED")));
    assert_not_reported_stopped(&rig, "ignored stop");
}

#[test]
fn a_changed_job_is_neither_stopped_nor_reported_as_stopped() {
    // The reply arrives after the printer moved on to another job.
    let (rig, id) = failing_print();
    rig.printer.set("PRINTING", Some(8));
    let reply = rig.mon.handle_reply("stop", &id);
    assert!(reply.starts_with("failed") && reply.contains("job changed"), "{reply}");
    assert!(rig.printer.calls().is_empty(), "{:?}", rig.printer.calls());
    assert_not_reported_stopped(&rig, "reply after job change");
    rig.advance(1);
    assert_eq!(rig.mon.snapshot().job.job_id, Some(8));
    assert!(rig.mon.incident_id().is_none());
    assert_not_reported_stopped(&rig, "new job adopted");
    assert_eq!(rig.printer.count("stop", 8), 0);

    // A stop was accepted for job 7, then job 8 started before it was observed.
    let (rig, id) = failing_print();
    rig.printer.s().ignore_stop = true;
    assert!(rig.mon.handle_reply("stop", &id).contains("awaiting confirmation"));
    rig.printer.set("PRINTING", Some(8));
    rig.advance(30);
    let snapshot = rig.mon.snapshot();
    assert_eq!(snapshot.job.job_id, Some(8));
    assert!(
        snapshot.pending_action.is_none(),
        "the old request must not follow the new job"
    );
    assert_eq!(
        rig.printer.count("stop", 8),
        0,
        "the stop for job 7 was replayed on job 8"
    );
    assert_eq!(rig.printer.count("stop", 7), 1);
    assert_not_reported_stopped(&rig, "job changed while pending");

    // Another job being finished says nothing about the one we asked to stop.
    let (rig, id) = failing_print();
    rig.printer.s().ignore_stop = true;
    assert!(rig.mon.handle_reply("stop", &id).contains("awaiting confirmation"));
    for state in ENDED_STATES {
        rig.printer.report(state, Some(8));
        rig.advance(1);
        assert_not_reported_stopped(&rig, &format!("{state} reported for job 8"));
    }
}

// ------------------------------------------------------------------ malformed responses

#[test]
fn malformed_status_bodies_are_errors_not_states() {
    for body in [
        json!({}),
        json!([]),
        json!(null),
        json!("IDLE"),
        json!(7),
        json!({"printer": null}),
        json!({"printer": "IDLE"}),
        json!({"printer": {}}),
        json!({"printer": {"state": null}}),
        json!({"printer": {"state": 7}}),
        json!({"printer": {"state": true}}),
        json!({"printer": {"state": ["STOPPED"]}}),
        json!({"printer": {"state": ""}}),
        json!({"printer": {"state": "UNKNOWN"}}),
        json!({"printer": {"state": "STOPPING"}}),
        json!({"state": "STOPPED"}),
        json!({"printer": {"state": "STOPPED"}, "job": "7"}),
        json!({"printer": {"state": "STOPPED"}, "job": [7]}),
        json!({"printer": {"state": "STOPPED"}, "job": {"id": "7"}}),
        json!({"printer": {"state": "STOPPED"}, "job": {"id": 7.5}}),
        json!({"printer": {"state": "STOPPED"}, "job": {"id": -1}}),
        json!({"printer": {"state": "STOPPED"}, "job": {"id": {"n": 7}}}),
    ] {
        let result = parse_status(body.clone());
        assert!(
            result.is_err(),
            "{body} parsed as {:?}",
            result.map(|status| status.state)
        );
    }
    // What a healthy printer sends still parses: with a job, without one, and in any case.
    let status = parse_status(json!({"printer": {"state": "STOPPED"}, "job": {"id": 7}})).unwrap();
    assert_eq!((status.state.as_str(), status.job_id), ("STOPPED", Some(7)));
    let status = parse_status(json!({"printer": {"state": "idle"}})).unwrap();
    assert_eq!((status.state.as_str(), status.job_id), ("IDLE", None));
    for state in STATES {
        parse_status(json!({"printer": {"state": state}, "job": null})).unwrap();
    }
}

/// A PrusaLink endpoint whose status body the test scripts byte for byte.
#[derive(Default)]
struct ScriptedPrinter {
    status: Mutex<Vec<u8>>,
    /// what status returns once a DELETE has been accepted
    after_delete: Mutex<Option<Vec<u8>>>,
    requests: Mutex<Vec<(String, String)>>,
}

impl ScriptedPrinter {
    fn answer(&self, body: &str) {
        *self.status.lock().unwrap() = body.as_bytes().to_vec();
    }
    fn deletes(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "DELETE")
            .count()
    }
}

impl Transport for ScriptedPrinter {
    fn send(&self, request: &HttpRequest, _: Duration) -> Result<HttpResponse, String> {
        let path = reqwest::Url::parse(&request.url).unwrap().path().to_string();
        self.requests
            .lock()
            .unwrap()
            .push((request.method.clone(), path.clone()));
        Ok(match (request.method.as_str(), path.as_str()) {
            ("GET", "/api/v1/status") => HttpResponse::new(200, self.status.lock().unwrap().clone()),
            ("GET", "/api/v1/job") => {
                HttpResponse::json(200, json!({"id": 7, "file": {"display_name": "part.bgcode"}}))
            }
            ("DELETE", "/api/v1/job/7") => {
                if let Some(body) = self.after_delete.lock().unwrap().take() {
                    *self.status.lock().unwrap() = body;
                }
                HttpResponse::new(204, vec![])
            }
            _ => HttpResponse::new(404, vec![]),
        })
    }
}

const PRINTING_JOB_7: &str = r#"{"printer": {"state": "PRINTING"}, "job": {"id": 7, "time_printing": 100}}"#;

const MALFORMED: [&str; 13] = [
    "",
    "not json",
    "{",
    "{}",
    "[]",
    "null",
    r#"{"printer": {}}"#,
    r#"{"printer": {"state": null}}"#,
    r#"{"printer": {"state": 7}}"#,
    r#"{"printer": {"state": "UNKNOWN"}}"#,
    r#"{"printer": {"state": "STOPPED"}, "job": {"id": "7"}}"#,
    r#"{"job": {"id": 7}}"#,
    "<html><body>502 Bad Gateway</body></html>",
];

/// The production PrusaLink client and monitor, talking to the scripted endpoint.
fn scripted_monitor(dir: &std::path::Path) -> (Arc<Monitor>, Arc<ScriptedPrinter>, Clock) {
    let transport = Arc::new(ScriptedPrinter::default());
    let clock = Clock::new();
    let printer = Arc::new(PrusaLink::with_transport(
        "printer.test",
        "pw",
        "maker",
        "apikey",
        "http",
        5.0,
        transport.clone(),
    ));
    let mut notifier = prusa_watch::notify::Notifier::with_transport(
        Default::default(),
        "",
        "",
        Arc::new(RecordingTransport::default()),
    );
    notifier.blocking = true;
    let monitor = Monitor::new(
        base_config(dir),
        Parts {
            printer: Some(printer),
            grabber: Some(FakeGrabber::new(clock.clone())),
            detector: Some(fake_detector()),
            notifier: Some(Arc::new(notifier)),
            clock: Some(clock.as_fn()),
            ..Default::default()
        },
    )
    .unwrap();
    (monitor, transport, clock)
}

#[test]
fn the_real_client_rejects_malformed_status_responses() {
    let transport = Arc::new(ScriptedPrinter::default());
    let client = PrusaLink::with_transport("printer.test", "pw", "maker", "apikey", "http", 5.0, transport.clone());
    for body in MALFORMED {
        transport.answer(body);
        let result = client.status();
        assert!(
            result.is_err(),
            "{body:?} read as {:?}",
            result.map(|status| status.state)
        );
    }
    transport.answer(PRINTING_JOB_7);
    assert_eq!(client.status().unwrap().job_id, Some(7));
}

#[test]
fn malformed_responses_never_confirm_a_stop() {
    for body in MALFORMED {
        let dir = tempfile::tempdir().unwrap();
        let (monitor, printer, clock) = scripted_monitor(dir.path());
        printer.answer(PRINTING_JOB_7);
        monitor.tick();
        let session = monitor.snapshot().job.session_id;
        assert_eq!(monitor.snapshot().job.job_id, Some(7));

        // Unreadable before sending: the request fails and nothing reaches the printer.
        printer.answer(body);
        let refused = monitor.control(7, &session, "stop", false);
        assert!(refused.is_err(), "{body:?}: {refused:?}");
        assert_eq!(printer.deletes(), 0, "{body:?}: sent without a readable status");

        // Readable before, accepted with 204, unreadable ever after.
        printer.answer(PRINTING_JOB_7);
        *printer.after_delete.lock().unwrap() = Some(body.as_bytes().to_vec());
        let answer = monitor.control(7, &session, "stop", false).unwrap();
        assert_eq!(answer, "stop requested; awaiting confirmation", "{body:?}");
        assert_eq!(printer.deletes(), 1, "{body:?}");
        for _ in 0..5 {
            clock.add(5.0);
            monitor.tick();
            let snapshot = monitor.snapshot();
            assert_eq!(snapshot.job.action_taken, None, "{body:?}: reported stopped");
            assert_eq!(monitor.counters().stops, 0, "{body:?}");
            assert!(snapshot.pending_action.is_some(), "{body:?}: request forgotten");
            assert!(!snapshot.printer_reachable, "{body:?}: treated as a valid report");
        }

        // Only the printer's own report for job 7 settles it.
        printer.answer(r#"{"printer": {"state": "STOPPED"}, "job": {"id": 7}}"#);
        monitor.tick();
        assert_eq!(
            monitor.snapshot().job.action_taken.as_deref(),
            Some("stopped"),
            "{body:?}"
        );
        assert_eq!(monitor.counters().stops, 1, "{body:?}");
    }
}
