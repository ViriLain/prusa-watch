# prusa-watch

Local, Bambu-style spaghetti detection for the Prusa Core One. It watches the Buddy3D camera over RTSP, runs Obico's open-source failure-detection model on your own machine, and **pauses the print through PrusaLink** when it's confident the print has failed. It sends an ntfy/Discord/webhook alert with the annotated frame and Resume / Cancel buttons.

No cloud and no OctoPrint. Everything stays on your LAN.

```
 Buddy3D camera ──RTSP (tcp)──▶ FrameGrabber ──latest frame every 10 s──▶ YOLO (ONNX, CPU)
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
cp config.example.yaml config.yaml
cp .env.example .env        # PRUSALINK_PASSWORD, NTFY_TOPIC, WEB_TOKEN, PUBLIC_URL
```

### 3a. Run with Docker (recommended, on an always-on box)

```bash
docker compose up -d --build        # the build downloads the ~250 MB model once
docker compose run --rm prusa-watch check
```

### 3b. Or run natively (Windows / macOS / Linux, Python ≥ 3.10)

```powershell
py -m venv .venv; .venv\Scripts\activate
pip install -e .
python scripts/fetch_model.py
prusa-watch check      # verifies PrusaLink auth, grabs a camera frame, runs the model on it
prusa-watch run        # dashboard on http://localhost:8484
```

`check` saves the grabbed frame to `data/check_frame.jpg`. Look at it before trusting the system.

**Host placement:** the monitor only protects prints while it's running. A desktop that sleeps is the wrong host. Use a NAS, homelab node, mini PC, or Pi 5 (arm64 wheels exist for everything). CPU inference is about 50–300 ms per frame on anything modern, so the GPU is irrelevant at one frame every 10 s.

### 4. Tune

- **ROI (biggest accuracy win):** crop to the build plate so the frame, door, cable chain, and any tool dock or purge area are excluded. Open *Live (raw)* on the dashboard to see the ROI box, then adjust `camera.roi` (normalized `[x1, y1, x2, y2]`).
- **Test detection** button: runs the model on the current frame without touching decision state. Hold some spaghetti in view to sanity-check it.
- `decision.sensitivity`: `1.0` is Obico's default. Raise it to about `1.25–1.5` if moderate failures only warn. Lower it if you get false pauses.
- `decision.action`: start with `notify` for a few prints to watch the scores, then switch to `pause`.
- **False positive mid-print?** Tap *False alarm: resume + mute* in the ntfy notification, or *Mute this print* on the dashboard.

## Notifications

- **ntfy** (default): the image is attached. When `PUBLIC_URL` is set, the notification gets action buttons: *Resume*, *False alarm: resume + mute*, and *Cancel print*. These buttons call the dashboard directly, so they only work while your phone is on the LAN or VPN (Tailscale, WireGuard). Self-host ntfy or use a long random topic. Topics on ntfy.sh are public-by-name.
- **Discord**: webhook with embedded image.
- **Webhook**: JSON POST with `kind`, `job_name`, `score`, `action_taken`, and `image_url`. Point it at a Home Assistant webhook trigger to flash lights, announce on speakers, and so on.

## Endpoints

| Path | Notes |
|---|---|
| `GET /` | dashboard |
| `GET /api/state` | full JSON state + 2 h score history |
| `GET /frame.jpg`, `/raw.jpg` | last analyzed (annotated) frame / live frame with ROI box |
| `POST /api/pause`, `/api/resume[?mute=1]`, `/api/stop`, `/api/mute`, `/api/unmute`, `/api/test` | require `?token=` or `X-Token` when `web.token` is set |
| `GET /metrics` | Prometheus: score, p, ewm, baseline, inference ms, frame age, camera and printer up, state, counters |

## Security

- Control endpoints can pause or cancel your print. Set `WEB_TOKEN`, and don't port-forward 8484. Use a VPN for remote access.
- PrusaLink is plain HTTP with digest auth. Keep it on the LAN.
- The Buddy3D RTSP stream is unauthenticated and unencrypted. Anyone on your LAN or Wi-Fi can watch it. Put IoT devices on their own VLAN if that matters to you.

## Limitations

- The model is Obico's general-purpose model (single class, "failure"). It's good at spaghetti and blobs. It won't catch layer shifts, warping corners, or under-extrusion.
- It only runs while PrusaLink reports `PRINTING`. When the camera is stale for 30 s during a print, you get a "camera offline" alert and no protection until it's back.
- Once you resume an AI-paused print, the short per-print mean has already absorbed the mess. The monitor will warn again, but it only re-pauses if things get worse. That's Obico's semantics.

## Development

```bash
pip install -e ".[dev]"
pytest                                    # 45 tests, synthetic ONNX model, no hardware
python scripts/fake_printer.py &          # PrusaLink simulator (apikey auth, password "test")
```

The tests build a tiny ONNX model with the same I/O contract as Obico's export (`[1,3,416,416]` → boxes `[1,N,1,4]` and confs `[1,N,1]`). Frame brightness drives confidence, so they exercise the full pipeline: RTSP/file grabber → ONNX Runtime → post-processing and NMS → decision → PrusaLink digest auth → ntfy/Discord/webhook → dashboard and metrics.

## License

AGPL-3.0-or-later. The detector post-processing and decision algorithm are ported from [obico-server](https://github.com/TheSpaghettiDetective/obico-server) (AGPL-3.0), and the model weights are Obico's.
