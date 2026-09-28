//! Parity with the original Python implementation: fixtures in tests/fixtures were
//! produced by the Python code (decision sequences, config defaults, escalation
//! built-ins, schedules, notification requests) and must be reproduced exactly.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::NaiveDateTime;
use prusa_watch::config::{Config, DecisionConfig, NotifyConfig, effective_config, parse_duration};
use prusa_watch::decision::FailureDecider;
use prusa_watch::escalation::{fmt_duration, parse_escalation};
use prusa_watch::http::{Body, HttpRequest, HttpResponse, Transport};
use prusa_watch::notify::{Event, Notifier};
use prusa_watch::policy::parse_schedules;
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn decision_sequences_match_python_bit_for_bit() {
    for scen in fixture("decision.json").as_array().unwrap() {
        let name = scen["name"].as_str().unwrap();
        let mut cfg = DecisionConfig::default();
        if let Some(v) = scen["config"].get("sensitivity") {
            cfg.sensitivity = v.as_f64().unwrap();
        }
        if let Some(v) = scen["config"].get("baseline_prior_frames") {
            cfg.baseline_prior_frames = v.as_i64().unwrap();
        }
        let mut d = FailureDecider::new(cfg, None);
        for (i, step) in scen["steps"].as_array().unwrap().iter().enumerate() {
            if step.get("reset").is_some() {
                d.reset_for_new_print();
                continue;
            }
            let confs: Vec<f64> = step["confs"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let v = d.update(&confs);
            assert_eq!(v.as_str(), step["verdict"].as_str().unwrap(), "{name} step {i}");
            let st = serde_json::to_value(&d.state).unwrap();
            for (k, expected) in step["state"].as_object().unwrap() {
                let got = &st[k];
                if expected.is_f64() || got.is_f64() {
                    assert_eq!(
                        got.as_f64().unwrap().to_bits(),
                        expected.as_f64().unwrap().to_bits(),
                        "{name} step {i} {k}: {got} vs {expected}"
                    );
                } else {
                    assert_eq!(got, expected, "{name} step {i} {k}");
                }
            }
        }
    }
}

#[test]
fn durations_match_python() {
    let f = fixture("durations.json");
    for (input, expected) in f["ok"].as_object().unwrap() {
        let v: serde_yaml::Value = serde_json::from_str::<Value>(input).map(|j| serde_yaml::to_value(j).unwrap()).unwrap();
        assert_eq!(parse_duration(&v).unwrap(), expected.as_f64().unwrap(), "{input}");
    }
    for case in f["bad"].as_array().unwrap() {
        let input = case[0].as_str().unwrap();
        let v: serde_yaml::Value = serde_yaml::to_value(serde_json::from_str::<Value>(input).unwrap()).unwrap();
        assert_eq!(parse_duration(&v).is_err(), case[1] == "error", "{input}");
    }
}

#[test]
fn fmt_duration_matches_python() {
    for (k, v) in fixture("fmt_duration.json").as_object().unwrap() {
        assert_eq!(fmt_duration(k.parse().unwrap()), v.as_str().unwrap(), "{k}");
    }
}

#[test]
fn builtin_escalation_matches_python() {
    let f = fixture("escalation_builtin.json");
    let esc = parse_escalation(None, true).unwrap();
    assert_eq!(esc.default_policy, f["default_policy"]);
    assert_eq!(esc.snooze_s, f["snooze_s"].as_f64().unwrap());
    let pols = f["policies"].as_object().unwrap();
    // (the JSON fixture's keys are sorted; Python's definition order is this)
    assert_eq!(
        esc.policies.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["ask_first", "pause_now", "night", "watch_only"]
    );
    assert_eq!(esc.policies.len(), pols.len());
    for (name, steps) in pols {
        let p = esc.policy(name).unwrap();
        for (s, e) in p.steps.iter().zip(steps.as_array().unwrap()) {
            assert_eq!(s.at, e["at"].as_f64().unwrap());
            assert_eq!(s.action.as_deref(), e["action"].as_str());
            assert_eq!(s.priority, e["priority"].as_i64().unwrap());
            assert_eq!(s.attach_image, e["attach_image"].as_bool().unwrap());
            assert_eq!(s.title.as_deref(), e["title"].as_str());
            assert_eq!(serde_json::to_value(&s.buttons).unwrap(), e["buttons"]);
            assert_eq!(serde_json::to_value(&s.notify).unwrap(), e["notify"]);
        }
        assert_eq!(p.steps.len(), steps.as_array().unwrap().len());
    }
    let sch = &f["schedules"][0];
    assert_eq!(esc.schedules[0].name, sch["name"]);
    assert_eq!(esc.schedules[0].start.format("%H:%M:%S").to_string(), sch["start"]);
    assert_eq!(esc.schedules[0].end.format("%H:%M:%S").to_string(), sch["end"]);
}

#[test]
fn schedule_matching_matches_python() {
    let raw: serde_yaml::Value = serde_yaml::from_str(
        r#"
- {name: night, start: "22:00", end: "07:00", policy: a}
- {name: wkday, start: "09:00", end: "17:00", policy: a, days: [mon, tue, wed, thu, fri]}
- {name: satnight, start: "23:00", end: "02:00", policy: a, days: [sat]}
- {name: allday, start: "00:00", end: "00:00", policy: a, days: [sun]}
"#,
    )
    .unwrap();
    let rules = parse_schedules(Some(&raw), &["a".to_string()].into_iter().collect()).unwrap();
    for case in fixture("schedule_cases.json").as_array().unwrap() {
        let dt = NaiveDateTime::parse_from_str(case["dt"].as_str().unwrap(), "%Y-%m-%dT%H:%M:%S").unwrap();
        let got: Vec<bool> = rules.iter().map(|r| r.matches(dt)).collect();
        let exp: Vec<bool> = case["matches"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect();
        assert_eq!(got, exp, "{dt}");
    }
}

#[test]
fn effective_default_config_matches_python() {
    let mut got = serde_json::to_value(effective_config(&Config::default(), true)).unwrap();
    // settings added after the Python version (field regressions, job 419)
    got["camera"].as_object_mut().unwrap().remove("ignore");
    got["decision"].as_object_mut().unwrap().remove("min_frame_p");
    let exp = fixture("effective_defaults.json");
    fn norm(v: &Value) -> Value {
        match v {
            Value::Number(n) => serde_json::json!(n.as_f64().unwrap()),
            Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), norm(v))).collect()),
            other => other.clone(),
        }
    }
    assert_eq!(norm(&got), norm(&exp));
}

#[test]
fn dotenv_matches_python() {
    let text = r#"
# comment
PRUSALINK_PASSWORD=abc123
export NTFY_TOPIC = my-topic   # trailing comment
QUOTED="has # hash and spaces"
SINGLE='x=y'
URL=http://192.168.1.10:8484
EMPTY=
not a line
"#;
    let got = serde_json::to_value(prusa_watch::dotenv::parse_dotenv(text)).unwrap();
    assert_eq!(got, fixture("dotenv.json"));
}

#[derive(Default)]
struct Recorded(Mutex<Vec<HttpRequest>>);

impl Transport for Recorded {
    fn send(&self, req: &HttpRequest, _t: Duration) -> Result<HttpResponse, String> {
        self.0.lock().unwrap().push(req.clone());
        Ok(HttpResponse::json(200, serde_json::json!({})))
    }
}

fn event(name: &str) -> Event {
    let mut e = Event::new("incident", "t", "m", "core-one");
    e.ts = "2026-09-27T12:00:00+00:00".into();
    let jpeg = Some(Arc::new(b"\xff\xd8JPEG".to_vec()));
    match name {
        "incident_buttons" => {
            e.title = "core-one: pausing in 2:00 unless you respond".into();
            e.message = "Spaghetti detected, respond".into();
            e.job_id = Some(7);
            e.job_name = Some("a,b;c.bgcode".into());
            e.score = Some(0.71);
            e.image_jpeg = jpeg;
            e.priority = 5;
            e.buttons = vec!["keep".into(), "act".into(), "stop".into()];
            e.incident_id = Some("abc123".into());
            e.policy = Some("ask_first".into());
            e.next_action = Some("pause".into());
            e.next_action_ts = Some(1790000120.0);
        }
        "paused" => {
            e.kind = "failure".into();
            e.title = "core-one: print PAUSED".into();
            e.message = "Paused.".into();
            e.job_id = Some(7);
            e.job_name = Some("x".into());
            e.score = Some(0.9);
            e.action_taken = Some("paused".into());
            e.priority = 5;
            e.buttons = vec!["resume".into(), "mute".into(), "stop".into()];
            e.incident_id = Some("abc123".into());
            e.policy = Some("night".into());
        }
        "stop_next" => {
            e.job_id = Some(7);
            e.priority = 4;
            e.buttons = vec!["act".into(), "dashboard".into(), "keep".into()];
            e.incident_id = Some("zz9".into());
            e.next_action = Some("stop".into());
        }
        "warning" => {
            e.kind = "warning".into();
            e.title = "core-one: possible print failure".into();
            e.message = "Not acting yet (score 0.35). üñí".into();
            e.job_id = Some(7);
            e.score = Some(0.35);
            e.image_jpeg = jpeg;
            e.priority = 4;
        }
        "camera_up" => {
            e.kind = "camera_up".into();
            e.title = "cam up".into();
            e.message = "ok".into();
            e.priority = 3;
        }
        _ => unreachable!(),
    }
    e
}

#[test]
fn notification_requests_match_python() {
    for case in fixture("notify_requests.json").as_array().unwrap() {
        let (ev_name, cfg_name) = (case["event"].as_str().unwrap(), case["config"].as_str().unwrap());
        let mut cfg = NotifyConfig::default();
        cfg.ntfy.topic = "pw-topic".into();
        cfg.ntfy.url = "https://ntfy.example".into();
        let (public, token) = match cfg_name {
            "reply_topic" => {
                cfg.ntfy.reply_topic = "pw-reply".into();
                cfg.ntfy.token = "tk_abc".into();
                ("http://watch.lan:8484", "wt")
            }
            "lan_only" => ("http://watch.lan:8484", "wt"),
            "no_public" => ("", ""),
            "all_channels" => {
                cfg.webhook.url = "https://ha.example/hook".into();
                cfg.discord.webhook_url = "https://discord.example/wh".into();
                ("http://watch.lan:8484", "")
            }
            _ => unreachable!(),
        };
        let t = Arc::new(Recorded::default());
        let mut n = Notifier::with_transport(cfg, public, token, t.clone());
        n.blocking = true;
        let n = Arc::new(n);
        n.send(event(ev_name), None);
        let got = t.0.lock().unwrap().clone();
        let exp = case["requests"].as_array().unwrap();
        assert_eq!(got.len(), exp.len(), "{ev_name}/{cfg_name}");
        for (g, e) in got.iter().zip(exp) {
            let ctx = format!("{ev_name}/{cfg_name} {}", g.url);
            assert_eq!(g.method, e["method"], "{ctx}");
            assert_eq!(g.url, e["url"].as_str().unwrap(), "{ctx}");
            let eh = e["headers"].as_object().unwrap();
            for (k, v) in eh {
                if k == "content-type" {
                    continue; // set by the HTTP client from the body kind
                }
                assert_eq!(g.get_header(k), Some(v.as_str().unwrap()), "{ctx} header {k}");
            }
            let extra: Vec<&str> =
                g.headers.iter().map(|(k, _)| k.as_str()).filter(|k| !eh.contains_key(&k.to_lowercase())).collect();
            assert!(extra.is_empty(), "{ctx}: unexpected headers {extra:?}");
            match &g.body {
                Body::Json(v) => {
                    let exp_body: Value = serde_json::from_str(e["body"].as_str().unwrap()).unwrap();
                    assert_eq!(v, &exp_body, "{ctx} body");
                }
                Body::Bytes(b) => assert_eq!(b.len() as u64, e["body_len"].as_u64().unwrap(), "{ctx}"),
                Body::Multipart(parts) => {
                    assert!(e["headers"]["content-type"].as_str().unwrap().starts_with("multipart/form-data"), "{ctx}");
                    assert_eq!(parts[0].name, "payload_json");
                    let exp_payload = serde_json::json!({"embeds": [{"title": "core-one: possible print failure", "description": "Not acting yet (score 0.35). üñí", "color": 0xF5A524, "timestamp": "2026-09-27T12:00:00+00:00", "image": {"url": "attachment://frame.jpg"}}]});
                    if ev_name == "warning" {
                        assert_eq!(serde_json::from_slice::<Value>(&parts[0].data).unwrap(), exp_payload);
                    }
                    assert_eq!(parts[1].name, "files[0]");
                    assert_eq!(parts[1].filename.as_deref(), Some("frame.jpg"));
                }
                Body::Empty => panic!("{ctx}: empty body"),
            }
        }
    }
}

#[test]
fn reference_file_equals_builtin_defaults() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.reference.yaml");
    let reference = prusa_watch::config::load_config(Some(&path), &BTreeMap::new()).unwrap();
    assert_eq!(parse_escalation(Some(&reference.escalation), false).unwrap(), parse_escalation(None, true).unwrap());
    let mut r = reference.clone();
    r.escalation = Default::default();
    assert_eq!(r, Config::default());
}

fn check_dets(got: &[prusa_watch::detector::Detection], exp: &Value, ctx: &str, tol: f64) {
    let exp = exp.as_array().unwrap();
    assert_eq!(got.len(), exp.len(), "{ctx}: {got:?}");
    for (g, e) in got.iter().zip(exp) {
        assert!((g.confidence - e[0].as_f64().unwrap()).abs() < tol, "{ctx}: conf {} vs {}", g.confidence, e[0]);
        for (a, b) in g.bbox.iter().zip(e[1].as_array().unwrap()) {
            assert!((a - b.as_f64().unwrap()).abs() < 1e-3, "{ctx}: box {:?} vs {}", g.bbox, e[1]);
        }
    }
}

#[test]
fn detector_on_synthetic_model_matches_python() {
    use prusa_watch::detector::{Detect, SpaghettiDetector};
    let model = format!("{}/tests/fixtures/fake-model.onnx", env!("CARGO_MANIFEST_DIR"));
    let det = SpaghettiDetector::load(&model, false).unwrap();
    let golden = fixture("detector_fake.json");
    for v in [0u8, 128, 255] {
        let img = image::RgbImage::from_pixel(640, 360, image::Rgb([v, v, v]));
        let dets = det.detect(&img, 0.08, 0.45);
        check_dets(&dets, &golden[format!("fake_solid{v}")]["dets"], &format!("solid{v}"), 1e-6);
    }
}

/// Real Obico model on real frames. Needs files that aren't in the repo:
///   PRUSA_WATCH_PARITY_DIR=dir with golden.json + <name>.ppm (from the Python detector)
///   PRUSA_WATCH_MODEL=path to model-weights.onnx
#[test]
#[ignore]
fn real_model_matches_python_on_real_frames() {
    use prusa_watch::detector::{Detect, SpaghettiDetector};
    let (Ok(dir), Ok(model)) = (std::env::var("PRUSA_WATCH_PARITY_DIR"), std::env::var("PRUSA_WATCH_MODEL")) else {
        eprintln!("skipped: set PRUSA_WATCH_PARITY_DIR and PRUSA_WATCH_MODEL");
        return;
    };
    let det = SpaghettiDetector::load(&model, false).unwrap();
    let golden: Value = serde_json::from_str(&std::fs::read_to_string(format!("{dir}/golden.json")).unwrap()).unwrap();
    for (name, g) in golden.as_object().unwrap() {
        if name.starts_with("fake_") {
            continue;
        }
        let img = image::open(format!("{dir}/{name}.ppm")).unwrap().to_rgb8();
        let dets = det.detect(&img, 0.08, 0.45);
        let p: f64 = dets.iter().map(|d| d.confidence).fold(0.0, |a, b| a + b);
        eprintln!("{name}: {} dets, p={p:.4} (python p={}) in {:.0} ms", dets.len(), g["p"], det.last_inference_ms());
        check_dets(&dets, &g["dets"], name, 1e-5);
    }
}

/// OpenCV INTER_LINEAR parity on non-uniform images of assorted sizes, including an
/// odd width, an upscale and an exact 2x downscale (which OpenCV routes to INTER_AREA).
#[test]
fn resize_matches_opencv_bit_for_bit() {
    use sha2::{Digest, Sha256};
    let f = fixture("resize_opencv.json");
    for (size, exp) in f["cases"].as_object().unwrap() {
        let (w, h): (u32, u32) = {
            let (a, b) = size.split_once('x').unwrap();
            (a.parse().unwrap(), b.parse().unwrap())
        };
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let (x, y) = (x as i64, y as i64);
            image::Rgb(std::array::from_fn(|c| {
                let c = c as i64;
                ((x * (7 + c) + y * 13 + (x * y) % (97 + c) + c * 50) % 256) as u8
            }))
        });
        let out = prusa_watch::imaging::resize_linear(&img, 416, 416);
        let px = |x, y| out.get_pixel(x, y).0.iter().map(|v| *v as i64).collect::<Vec<_>>();
        let expect = |k: &str| exp[k].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect::<Vec<_>>();
        assert_eq!(px(0, 0), expect("first"), "{size} first pixel");
        assert_eq!(px(208, 208), expect("center"), "{size} center pixel");
        assert_eq!(px(415, 415), expect("last"), "{size} last pixel");
        assert_eq!(hex::encode(Sha256::digest(out.as_raw())), exp["sha256"].as_str().unwrap(), "{size} whole image");
    }
}
