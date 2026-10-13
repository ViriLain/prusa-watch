# Field evaluation and operational evidence

Automated parity verifies the implementation on identical pixels. It does not
measure false alarms, missed failures or whether a physical printer obeys a
command. Keep these evidence types separate when reporting results.

## Recognition dataset

Capture unannotated, lossless frames at a regular cadence before deciding whether
they look interesting. Include clean prints, first layers, brims, supports, blobs,
spaghetti, varied bed heights, chamber lighting, ROI settings and ignore zones.
For one lossless frame from a live stream:

```sh
ffmpeg -rtsp_transport tcp -i "$CAMERA_URL" -frames:v 1 raw.png
```

Record the source/print ID, timestamp, camera/lighting settings, ROI/ignore zones,
model hash and effective redacted configuration beside each frame. Keep all
frames from a print in the same train/tuning/evaluation split. Have a person label
visible failures and the earliest point at which a pause would be useful,
independently of the detector's score. Preserve disagreements for review.

Report per-print false interventions, missed visible failures, detection/action
delay and available-camera coverage, with denominators and configuration. Report
raw per-frame classifications separately from the full decision/escalation
sequence. Tune on the tuning split, freeze settings, then evaluate held-out prints.
Annotated diagnostic recordings cannot establish these rates.

## Device and delivery checks

Use a disposable test print with supervision. Confirm pause, resume and stop
through fresh printer status and physical behavior, including a delayed pause.
Check camera disconnection, printer/network loss, a host restart while muted or
paused, and an overdue intervention during downtime. Verify that another print
cannot receive an old dashboard or notification command.

Test notification delivery and each relevant mobile button from the intended
network, with the configured ACLs. Test expiry and incident closure. Confirm the
host stays awake and that `/healthz` becomes 503 when protection is unavailable,
while `/livez` continues to represent process liveness. Confirm recording budget,
write failures and shutdown on the deployment host. Keep dates, firmware,
configuration and outcomes with the evidence.
