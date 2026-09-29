# prusa-watch

Local, Bambu-style spaghetti detection for the Prusa Core One. It watches the Buddy3D camera over RTSP, runs Obico's open-source failure-detection model on your own machine, and **pauses the print through PrusaLink** when it's confident the print has failed. It sends an ntfy/Discord/webhook alert with the annotated frame and Resume / Cancel buttons.

Inference and PrusaLink control run locally. Notifications use the services you configure, including self-hosted ntfy. It's a single Rust binary; the only runtime dependency is `ffmpeg`.

```
 Buddy3D camera ──RTSP (tcp)──▶ ffmpeg ──latest frame every 10 s──▶ YOLO (ONNX via tract, CPU)
                                                                               │ boxes + confidences
 Core One ◀──PUT /api/v1/job/{id}/pause── PrusaLink client ◀── pause ── FailureDecider (Obico EWM/baseline)
     │                                                                         │ warning / failure
     └──GET /api/v1/status (every 5 s: only analyze while PRINTING)            ▼
                                                              ntfy · Discord · webhook · dashboard :8484
```

## Why RTSP and not WebRTC

The Buddy3D's WebRTC stream is brokered through Prusa Connect. The only local interface is RTSP at `rtsp://<camera-ip>/live`, which has to be switched on in the Prusa app. The camera has no snapshot endpoint, so prusa-watch keeps one persistent RTSP session open and only keeps the newest frame.

## How the decision works (and why it won't pause on a single weird frame)

This is a port of Obico's production algorithm (`obico-server/backend/lib/prediction.py`) using their grid-searched hyperparameters:

- **Per frame:** `p` = sum of box confidences.
- **Signal:** an exponential moving average of `p` over about 12 frames (2 min).
- **Baseline:** a long rolling mean over about 20 h of printing. It's subtracted from the signal, so a camera angle that always shows a bit of "noise" stops triggering once the baseline learns it. The baseline persists in `data/prediction_state.json`.
- **Per-print short mean:** a print with lots of stringy supports raises its own bar.
- **Grace:** the first 30 frames (5 min) of every print never trigger.
- **Warn vs pause:** a pause needs the signal to be 1.75× what a warning needs.

**One deliberate deviation from Obico:** on a fresh install the baseline is seeded as if one hour of clean printing had already been watched (`baseline_prior_frames: 360`). Without that seed, pure Obico's baseline on a new install is just the average of the current print. Spaghetti that starts on the first layer of your first monitored print then gets absorbed as "normal" and **never pauses**. The unit tests demonstrate this. With the seed, a fresh install behaves like an established one, which is the state Obico's thresholds were tuned on. Set it to `0` for exact Obico behavior.

Simulated time to action (10 s frames, default sensitivity, established baseline):

| Situation | Warn | Pause |
|---|---|---|
| Heavy spaghetti mid-print (p ≈ 2.5) | ~20 s | ~20 s |
| Moderate (p ≈ 1.5) | ~20 s | ~40 s |
| Light (p ≈ 1.0) | ~30 s | ~70 s |
| Faint (p ≈ 0.8) | ~50 s | never, warn-only (raise `sensitivity` to change) |
| Heavy spaghetti from the first layer | 5 min (end of grace) | 5 min |

`p` is the summed confidence of every box the model draws. A real spaghetti mess typically produces several boxes.

## Setup

### 1. Printer and camera

1. **Enable RTSP** for the Buddy3D in the Prusa app (camera settings). Check it plays in VLC: `rtsp://<camera-ip>/live`. If VLC stutters, set *RTP over RTSP (TCP)*.
2. **Reserve DHCP leases** for the camera and the printer in your router. A changed IP means a blind monitor.
3. **PrusaLink**: on the printer, go to *Settings → Network → PrusaLink*. Make sure it's enabled and note the password. The username is always `maker`.
4. Consider setting the camera to **HD instead of FHD**. The model runs at 416×416 anyway, and FHD is reported to strain the camera's SoC.
5. **Keep the chamber light on during prints.** The model can't see spaghetti in the dark.

### 2. Configure

```bash
cp config.example.yaml config.yaml   # a small starter file: printer IP, camera URL, timezone
cp .env.example .env                 # PRUSALINK_PASSWORD, NTFY_TOPIC, NTFY_REPLY_TOPIC, WEB_TOKEN, PUBLIC_URL
prusa-watch config                   # see the effective settings
```

### 3a. Run with Docker (recommended, on an always-on box)

```bash
docker compose up -d --build        # the build downloads the ~250 MB model once
docker compose run --rm prusa-watch check
```

### 3b. Or run natively (macOS / Linux / Windows)

Needs a Rust toolchain ([rustup.rs](https://rustup.rs)) and `ffmpeg` (macOS: `brew install ffmpeg`, Debian/Ubuntu: `apt install ffmpeg`).

```bash
cargo install --path .     # builds and installs the `prusa-watch` binary into ~/.cargo/bin
prusa-watch check          # verifies PrusaLink auth, grabs a camera frame, runs the model on it
prusa-watch run            # dashboard on http://localhost:8484
```

- `.env` is loaded automatically from next to `config.yaml` (or the current directory), the same file Docker uses. Variables already set in your shell win; `--env-file PATH` picks a different file.
- The first `check` or `run` downloads the ~200 MB model into `models/`. `prusa-watch fetch-model --force` re-downloads it.
- `ffmpeg` decodes the camera stream. If it isn't on your `PATH`, set `PRUSA_WATCH_FFMPEG=/path/to/ffmpeg`.
- On a Mac, keep the machine awake while testing: `caffeinate -i prusa-watch run`.
- **macOS 15+: "No route to host (os error 65)"** for the printer and camera, while `curl` to the same IP works, means your terminal app lacks the Local Network permission. Command-line tools inherit it from the terminal; Apple's own binaries like curl are exempt. Enable it in *System Settings → Privacy & Security → Local Network*, then quit and reopen the terminal.

`check` saves the grabbed frame to `data/check_frame.jpg`. Look at it before trusting the system.

**Host placement:** the monitor only protects prints while it's running. A desktop that sleeps is the wrong host. Use a NAS, homelab node, mini PC, or Pi 5 (the binary builds for arm64 as well as x86-64). Inference runs on the CPU with [tract](https://github.com/sonos/tract) in roughly 0.5 s per frame on a modern x86 core, which is plenty at one frame every 10 s; `detector.use_gpu` is accepted but ignored.

### 4. Tune

- **ROI (biggest accuracy win):** crop to the build plate so the frame, door, cable chain, and any tool dock or purge area are excluded. Open *Live (raw)* on the dashboard to see the ROI box, then adjust `camera.roi` (normalized `[x1, y1, x2, y2]`).
- **A spot that keeps fooling the model** (dust or a sheet texture that glints at one bed height): clean the bed first. If it recurs, mask it with `camera.ignore`, a list of `[x1, y1, x2, y2]` rectangles in full-frame coordinates (same space as `roi`, drawn grey on the dashboard). Boxes centred inside are dropped before scoring. Keep zones tight: spaghetti inside a zone is invisible too.
- `decision.min_frame_p` (default 0.3): an incident only opens if the current frame itself shows something. Obico's verdict follows a moving average that outlives short bursts; without this gate the tail of a glint can pause a print on a clean frame. `0` restores pure Obico behaviour.
- **Test detection** button: runs the model on the current frame without touching decision state. Hold some spaghetti in view to sanity-check it.
- **After a test print or a false alarm, run `prusa-watch report`** (or `prusa-watch report <job-id>`). It prints the peak summed confidence (`p`), the peak score (1/3 is the warning line, 2/3 the pause line), and when the first warning and failure verdicts happened. The raw data is in `data/history/job-<id>.csv` (one row per analyzed frame), and annotated frames where the model saw something are in `data/frames/job-<id>/`. See `recording:` in [`config.reference.yaml`](config.reference.yaml) for limits and retention.
- `decision.sensitivity`: `1.0` is Obico's default. Raise it to about `1.25–1.5` if moderate failures only warn. Lower it if you get false pauses.
- `escalation.default_policy`: start with a notify-only policy (the example has `watch_only`) for a few prints to watch the scores, then switch to one that pauses.
- **False positive mid-print?** Tap *False alarm: resume + mute* in the ntfy notification, or *Mute this print* on the dashboard.

## Configuration

Sensible defaults are built in, so your `config.yaml` only lists what's different. The starter [`config.example.yaml`](config.example.yaml) is a small file: printer IP, camera URL, ntfy topic and timezone.

| Layer (low → high) | Where | Example |
|---|---|---|
| Built-in defaults | [`config.reference.yaml`](config.reference.yaml) shows every setting and its default | `decision.sensitivity: 1.0` |
| Your file | `config.yaml`, only your changes; `${VAR}` / `${VAR:-default}` expand from `.env` | `decision: {sensitivity: 1.25}` |
| Environment | `PRUSA_WATCH__<SECTION>__<KEY>=value`, parsed as YAML | `PRUSA_WATCH__ESCALATION__DEFAULT_POLICY=watch_only` |

- **`prusa-watch config`** prints the effective result (defaults + file + env) with secrets masked. `prusa-watch config --defaults` prints the built-ins.
- **Escalation merges by policy name.** Define `ask_first:` to replace just that policy, and the other built-ins stay. `schedules:` replaces the whole list (`[]` turns night mode off). `name: null` removes a built-in policy. Defining a new policy doesn't make it active; set `default_policy` or schedule it.
- **Durations:** any `*_s` setting, and every escalation `at:`, accepts seconds or a duration: `90`, `"90s"`, `"2m"`, `"1h30m"`.
- **Strict keys:** misspelled keys are rejected at startup, including in env overrides. So are keys from older versions, and the error names where each one moved.
- **The reference can't go stale:** tests fail if `config.reference.yaml` stops matching the built-in defaults or misses a setting.

**Built-in behavior when you configure nothing else:**
- **During the day** (`ask_first`): ask with *Keep printing / Pause now / Cancel* buttons, remind at 1 min, pause at 2 min.
- **22:00–07:00** (`night`): pause immediately with a silent notification.
- **Never** cancel a print on its own. That's opt-in (see below).
- Also built in: `pause_now` and `watch_only`.

## Escalation policies

Detection (`decision:`) decides *whether* a print is failing. Escalation (`escalation:`) decides *what happens next*. A failure verdict opens an **incident**, and the active **policy** runs its timed **steps**. Example override: a longer window, and give up on a print left paused for 2 h:

```yaml
escalation:
  policies:
    ask_first:                   # replaces the built-in ask_first; night/pause_now/watch_only stay
      steps:
        - {at: 0,   notify: [ntfy, discord, webhook], buttons: [keep, act, stop]}
        - {at: 3m,  notify: [ntfy], title: "{printer}: still failing, {next_action} in {next_action_in}"}
        - {at: 5m,  action: pause}
        - {at: 2h5m, action: stop, notify: [ntfy, discord], priority: 4}
```

| Step key | Meaning |
|---|---|
| `at` | time after detection; steps run in order |
| `action` | `pause`, `stop` or none. A pause only happens while printing; a stop also cancels a print that's already paused |
| `notify` | channels (`ntfy`, `discord`, `webhook`); omit = all configured, `[]` = silent |
| `priority` | ntfy 1–5 (5 breaks through Do Not Disturb on Android). Default 5 |
| `buttons` | up to 3 of `keep`, `act`, `stop`, `resume`, `mute`, `dashboard`. Omit to get automatic buttons: keep/act/stop before we act, resume/mute/stop after a pause |
| `title`, `message` | templates with `{printer} {job} {score} {policy} {schedule} {next_action} {next_action_in} {elapsed} {action_taken}` |
| `attach_image` | include the annotated frame (default true) |

What the buttons do:
- **keep:** false alarm, keep printing. It also resumes the print if we'd already paused it.
- **act:** run the next action step now, skipping any reminders before it.
- **stop:** cancel the print.
- **resume:** resume the print.
- **mute:** resume, and no more alerts for this print.

**When an incident ends:**
- You answer it.
- You handle it at the printer (pause, resume or stop there).
- The job ends.
- The steps run out. The exception is a print we paused: that incident stays open so *Resume* keeps working.

**Schedules:** time windows pick the policy, and the first match wins. Windows can cross midnight, and `days` refers to the day the window starts. With no match, `default_policy` applies. The top-level `timezone` setting (an IANA name) sets the schedule clock.

**How the buttons reach prusa-watch from anywhere:** each button POSTs `<command> <incident-id>` to `notify.ntfy.reply_topic`. prusa-watch keeps an **outbound** streaming subscription to that topic, so the buttons work on LTE with no port forwarding and no VPN. Replies that don't match the open incident, or don't make sense in its current state, are ignored. If `reply_topic` isn't set, the buttons call the dashboard directly, which only works on your LAN or VPN.

The dashboard shows a live countdown to the next action, with buttons for whatever commands currently apply. The webhook payload includes `incident_id`, `policy`, `next_action`, `next_action_ts` and `command_urls`, so Home Assistant can build its own actionable notifications.

## Notifications

- **Channels:** ntfy (the frame is attached, and the notification has action buttons), Discord (embedded image), or a generic JSON webhook.
- **Incident notifications** are routed per step, as described above.
- **Everything else** is routed under `notify:`. `notify.warning` covers the "possible failure" heads-up, `notify.camera` covers the camera going offline or coming back, and `notify.info` covers confirmations. Each has `enabled`, `channels`, `priority` and `cooldown_s`.
- **ntfy.sh topics are public-by-name.** Use long random names for `topic` and `reply_topic`, or self-host ntfy with ACLs and set `token`.

## Endpoints

| Path | Notes |
|---|---|
| `GET /` | dashboard |
| `GET /api/state` | full JSON state + 2 h score history |
| `GET /frame.jpg`, `/raw.jpg` | last analyzed (annotated) frame / live frame with ROI box |
| `POST /api/pause`, `/api/resume`, `/api/stop`, `/api/mute`, `/api/unmute` | `X-Token` plus the rendered `job_id` and `session_id`; resume accepts `mute=1` |
| `POST /api/test` | `X-Token`; inference failures return 502 |
| `POST /api/incident/{veto,act,stop,resume,mute}?id=...` | explicit incident ID plus `X-Token` or a scoped, expiring notification capability |
| `GET /livez`, `/healthz` | process liveness / monitor readiness; readiness returns 503 for stale polling, frames, inference or action faults |
| `GET /metrics` | Prometheus: score, p, ewm, baseline, inference ms, frame age, camera and printer up, state, incident open, seconds to the next action, counters (pauses, stops, vetoes, auto actions, ...) |

## Security

- Native runs bind to `127.0.0.1` by default. Set `web.host: 0.0.0.0` and a strong `WEB_TOKEN` to reach them over a trusted LAN/VPN. Docker Compose publishes only on host loopback by default; change its port binding for LAN use. Network binding without a token requires explicit `web.allow_unauthenticated: true`.
- Browser controls send the token in a header and retain it only for the browser session. Notification links contain command-specific HMAC permissions that expire after one hour and become unusable when the incident closes. Keep `data/control-key` private; a replacement key invalidates outstanding links. Read-only state and camera endpoints have no token requirement.
- Control endpoints can pause or cancel your print. Do not port-forward 8484; use a VPN for remote access.
- The ntfy **reply topic** can pause, stop, resume or keep a print, but only for the open incident's one-time id. On ntfy.sh, topics are public-by-name, so give it a long random name. Better: self-host ntfy with an ACL and set `token`.
- PrusaLink is plain HTTP with digest auth. Keep it on the LAN.
- The Buddy3D RTSP stream is unauthenticated and unencrypted. Anyone on your LAN or Wi-Fi can watch it. Put IoT devices on their own VLAN if that matters to you.

## Limitations

- The model is Obico's general-purpose model (single class, "failure"). It's good at spaghetti and blobs. It won't catch layer shifts, warping corners, or under-extrusion.
- Inference runs while PrusaLink reports `PRINTING`; each newly decoded frame contributes once. When frames go stale, new visual evidence stops and an offline alert is sent. An existing incident remains armed until you veto/mute it, handle the print at the printer, or end the job. Lower scores alone do not cancel its scheduled intervention.
- Printer commands remain pending until fresh status confirms their effect. Confirmation deadlines and retry limits are configurable. Exhausted attempts produce a visible fault and urgent notification.
- Versioned checkpoints retain same-print mute, grace, incident and action ownership. Recovery requires matching configuration, job ID/name and plausible elapsed print time. An overdue intervention is retired with a visible fault; it is never replayed automatically.
- Notification delivery has ordered bounded queues per channel and three bounded attempts. Recording has a separate bounded worker and combined count/byte retention, including failure images. Queue drops and I/O errors appear in `/api/state`; a saturated or failed disk can lose diagnostic records. Printer writes remain serialized with control state to prevent conflicting commands.
- Once you resume an AI-paused print, the short per-print mean has already absorbed the mess. The monitor will warn again, but it only re-pauses if things get worse. That's Obico's semantics.

## Development

```bash
cargo test                                          # synthetic ONNX model, no hardware needed (needs ffmpeg for the camera tests)
cargo run --example fake_printer -- --port 8081     # PrusaLink simulator (apikey auth, password "test")
cargo lint && cargo fmt --check                    # warnings are errors
cargo check-all                                    # locked dependencies, all targets/features
```

The tests use a tiny ONNX model (`tests/fixtures/fake-model.onnx`) with the same I/O contract as Obico's export (`[1,3,416,416]` → boxes `[1,N,1,4]` and confs `[1,N,1]`). Frame brightness drives confidence, so they exercise the full pipeline: ffmpeg/file grabber → tract → post-processing and NMS → decision → PrusaLink digest auth → ntfy/Discord/webhook → dashboard and metrics.

Rust 1.98.1 is pinned with rustfmt and Clippy; the supported minimum is 1.91. Configuration forbids unsafe code, rejects ignored results and unfinished debug/TODO macros, and enables correctness, suspicious-code, lock and clone checks. The format uses edition 2024, 120 columns and Unix line endings. See [CONTRIBUTING.md](CONTRIBUTING.md) for the validation workflow.

`tests/parity.rs` preserves the original Python decision mathematics bit for bit. Intentional security/configuration migrations have explicit assertions. Preprocessing uses OpenCV `INTER_LINEAR_EXACT` with independent resize hashes. The production-model corpus is checked in CI with a pinned SHA-256:

```bash
cargo run --locked -- fetch-model
PRUSA_WATCH_MODEL=models/model-weights.onnx PRUSA_WATCH_PARITY_DIR=tests/fixtures/vision \
  cargo test --locked --test parity real_model_matches_python_on_real_frames -- --ignored --nocapture
```

The corpus compares implementations on identical pixels; its saved field inputs are already annotated and do not establish detection accuracy. See [fixture provenance](tests/fixtures/vision/README.md), [review changes](docs/REVIEW_HARDENING.md), and [field evaluation](docs/FIELD_EVALUATION.md).

## License

AGPL-3.0-or-later. The detector post-processing and decision algorithm are ported from [obico-server](https://github.com/TheSpaghettiDetective/obico-server) (AGPL-3.0), and the model weights are Obico's.
