# October 2026 safety and privacy fixes

Four defects, fixed before any further feature work. Each has tests that fail on the
old behavior.

| Defect | Change | Tests |
|---|---|---|
| A stop was "confirmed" by any state other than PRINTING/PAUSED/ATTENTION, including `UNKNOWN`, `BUSY`, `ERROR` and states no firmware defines. A missing job id was attributed to the job being stopped. | `Action::confirmed` takes the intended job, the job the printer named and the state from the same report. Stop needs `STOPPED`, for that job or naming no job; pause and resume need the same job named. `FINISHED`/`IDLE`/`READY` end the wait without a claim: the request is dropped with a note and not counted. In a state where nothing can be sent (`BUSY`, `ERROR`, `ATTENTION` for a pause) one "NOT CONFIRMED" alert goes out, readiness fails, and the request stays alive without using up attempts, so it is sent again if the print resumes. A stop confirmed on a later poll or by a retry is announced. `PrusaLink::status` parses strictly (`printer.state` required and one of the nine spec states, `job.id` a whole number) and every status the monitor reads is validated, so a malformed or unrecognized report is "printer unreachable", never "not printing". | `tests/cancellation.rs` |
| Logs, `check` output and state errors could contain camera URL credentials, ntfy topics and hook URLs. | `src/redact.rs` is the single redactor: structural rules for URLs and credential patterns, plus the configured secret values. It runs where transport and ffmpeg errors are created, at every export (state, readiness, API errors, notification text, `check`, `config`), and inside the log writer. | `tests/redaction.rs`, unit tests in `src/redact.rs` |
| ntfy button definitions contained `notify.ntfy.token`. They are delivered to every subscribed device and stored by the ntfy server. | Buttons post `<command> <incident> <expires> <signature>` and nothing else. The reply listener drops any command without a valid signature for exactly that command and incident. Lifetime is `notify.command_ttl_s`. | `tests/replies.rs`, `tests/notify.rs` |
| The token protected controls only. Camera images, state and metrics were readable by anyone who could reach the port. | One access guard in front of every route. `/`, `/livez`, `/healthz` are public and carry no data; everything else, including routes added later, needs the token or a capability signed for that exact request. Without a token the service is local-only (loopback peer, loopback `Host`, same-machine `Origin`, no proxy headers). | `tests/web.rs` |

## Behavior changes to know about

- **Dashboard:** with `web.token` set it asks for the token before showing anything.
  Scripts, Prometheus and Home Assistant must send it for `/api/state`, `/metrics` and the
  images. A webhook's `image_url` is a signed link and needs nothing else.
- **No token configured:** unchanged on the computer itself. Requests that show signs of a
  proxy (forwarding headers, a non-loopback `Host`, a foreign `Origin`) get 403. This is a
  best effort: a same-machine proxy that rewrites `Host` and adds no forwarding header
  (nginx's default `proxy_pass`) cannot be told apart from a local program. Set `web.token`
  before proxying.
- **ntfy with a `token`:** the reply topic must accept anonymous publishes
  (`ntfy access everyone <reply-topic> write-only`). `prusa-watch check` says so.
- **Buttons on alerts sent before the upgrade** use the old unsigned format and are ignored.
- **Buttons expire** after `notify.command_ttl_s` (default 1 h, max 24 h). An expired tap
  changes nothing; while that incident is still open it sends a note (at most one a
  minute). Raise the setting if you answer overnight pauses from the original alert.
- **Stops** report "awaiting confirmation" until the printer reports `STOPPED`, then send
  "print STOPPED". A printer that goes straight to `READY`/`IDLE` (for example with the
  "ready" flag set) yields "print ended before the stop was confirmed" instead. Both go
  where the request came from: the escalation step's channels and priority (nothing for a
  step with `notify: []`), or every channel for a stop a person asked for.
- **An unrecognized printer state** (including the firmware's `UNKNOWN`) is handled like an
  unreachable printer: nothing is concluded from it and escalation steps wait.
- **`/healthz`** returns the reason only to an authorized caller; the status code is unchanged.
- **`check`** waits `open_timeout_s + read_timeout_s` (still 20 s by default) for a frame.

## Found on the way, fixed here

- A Stop or Resume that the printer accepted but did not carry out was never retried while
  an incident was open. It is now retried at its deadline like any other request: bounded
  attempts, then a failure alert. While such a request is pending, and after it has failed,
  it still blocks the incident's own pause; the alerts say to check the printer.
- A pending request in a state where nothing can be sent (`BUSY`, `ERROR`) used to wait
  forever in silence. It now raises one "NOT CONFIRMED" alert and fails readiness while
  it waits.

## What the firmware reports

Read from Prusa-Firmware-Buddy (`printer_state.cpp`, `status_renderer.cpp`,
`prusa_link_api_v1.cpp`), not measured on a printer: after an accepted `DELETE` the
printer keeps reporting `PRINTING` with the job id while it aborts, then `STOPPED` with
no `job` object (or `READY` if the ready flag is set), then `IDLE` once dismissed. The
`job` object is present only while printing, paused or in attention. An unmapped internal
state is reported as `UNKNOWN`.

## Not covered

- No command or notification was sent to a physical printer or a real ntfy account.
- The sequence above has not been captured on a Core One. If its firmware never reports
  `STOPPED`, a real stop ends as "print ended before the stop was confirmed" or as a
  failure alert, never as a false "stopped".
- While the printer reports an unrecognized state or is unreachable, an open incident's
  steps and any pending request wait, and no alert is sent for that (PR #8 adds the
  printer-down alert).
- A new job that starts before a request's deadline drops that request without a note.
- A stop confirmed on a later poll is not counted in `auto_actions`.
- PrusaLink itself (plain HTTP, digest auth) and the unauthenticated RTSP stream are
  unchanged; see the README's security section.
