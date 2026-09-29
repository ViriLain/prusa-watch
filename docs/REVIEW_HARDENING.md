# September 2026 review hardening

The Rust rewrite (#5) and its recording prerequisite (#4) were merged first.
This change includes the reviewed ignore-zone/current-frame changes from #6 and
addresses the twelve findings from the September 29 review.

| Finding | Change and regression evidence |
|---|---|
| F1 mute leaves actions armed | Mute retires incident/pending automatic actions; tests cover waiting and already-paused incidents. |
| F2 starter can act at night | Active `watch_only` plus `schedules: []`; example validation and effective defaults are tested. |
| F3 HTTP success is not completion | Typed requested/confirmed/failed outcomes, fresh same-job status, bounded deadlines/backoff/attempts, visible and notified faults. |
| F4 cached frame becomes votes | Decode sequence ownership; one sequence contributes once; fakes publish frames explicitly. |
| F5 invalid numbers/booleans | Construction validates finite bounded durations, divisors, windows, probabilities, thresholds and memory settings. Configuration expansion uses an injected environment map; no unsafe global mutation. |
| F6 restart loses print context | Validated versioned combined checkpoint, UTC conversion of elapsed deadlines, same-print identity reconciliation; overdue actions never replay automatically. |
| F7 vision parity | Portable `INTER_LINEAR_EXACT`, independent byte fixtures and production-model corpus, mandatory CI run; missing inputs fail explicit parity requests. |
| F8 ignored notification environment | Starter YAML references ntfy URL/token, Discord URL and webhook URL. |
| F9 broken notification lifetime/URLs | Notify-only buttons survive until `max(snooze_s, 60s)`; URL encoding handles arbitrary values; signed links expire after one hour. |
| F10 stale dashboard button | Required incident ID; rendered incident identity is captured in its button; generic commands require job plus local session generation. |
| F11 misleading health/setup success | Setup and test errors propagate; separate liveness/readiness; live frame/inference/loop ages and protection status. |
| F12 poisoned prediction state | Semantic validation, nonnegative integral counts and supported checkpoint versions; legacy valid baseline files still load. |

## Design and operational changes

`Session` owns print/intervention state and typed `Action` outcomes. Status reads,
inference, annotation and storage run outside the control lock, and inference
results retain a session generation. Printer writes remain serialized with
control state to prevent conflicting commands. A request already sent to the
printer cannot be recalled by mute; mute prevents remaining scheduled actions
and retries. Explicit human resume requests remain reconcilable after mute.

Elapsed deadlines use a monotonic clock; wall time selects schedules and is used
only at persistence boundaries. An incident latches its policy. Score recovery
or camera loss does not retire an already-open incident; veto/mute, printer
handling, job completion or expiry do. Those product semantics are documented.

Notification workers are bounded and ordered per channel, isolate a slow channel,
retry transient failures three times and join on shutdown. The reply reader has
an owned handle and five-second total request deadline; reconnect uses the last
processed message. Storage is ordered with coalesced atomic checkpoints and
bounded frame tasks. Clean shutdown flushes checkpoints; queue overload and I/O
errors are observable. A slow or unavailable disk can lose diagnostic records,
and filesystem calls themselves are subject to the host's filesystem behavior.

Loopback binding is the default. Network-accessible controls require a token or
explicit opt-in. Header authentication avoids browser token URLs. Notification
capabilities use HMAC-SHA256 with a private persistent random key and are scoped
to one incident, command and expiry. Closed incident identities cannot be replayed.

Recording count and byte limits cover histories, annotated frames and failure
images. An active job is never deleted to make room; recording suspends when its
budget is exhausted. The shipped model has a verified SHA-256, and detector load
checks input/output contracts. Custom models need an explicit expected hash, or
an explicit empty hash if integrity verification is intentionally disabled.

YAML compatibility stays at the loading boundary, using maintained
`serde_norway`; existing duration/merge/decision fixtures remain. `Detect` has a
single fallible inference method. Preview results have a named type rather than
a nested tuple. Rust/package metadata, a pinned toolchain/formatter, strong
Clippy rules, locked Cargo aliases and contributor guidance support maintenance.
CI adds minimum-version, Windows, dependency-audit, production-model and
container inference/readiness checks.

## Validation and remaining evidence

Local validation: 152 tests passed with one opt-in vision test skipped; the explicit
production-model comparison also passed on six inputs. Formatting, Clippy with
warnings as errors, documentation with warnings as errors, and Rust 1.91 checks
passed. GitHub CI supplies the Linux/Windows and container evidence; Docker is
unavailable on the implementation host. See the PR checks for their status.
Deterministic tests cover restart, stale ownership, expiry, queue saturation,
slow inference, background lifecycle, fake-printer failures and actual local
HTTP/FFmpeg behavior. The real-model comparison uses six committed inputs and
an independently generated reference. No command or notification was sent to a
physical printer or live notification account during implementation.

Physical printer behavior, mobile delivery and a held-out raw labeled accuracy
dataset remain separate operational evidence. Their capture/evaluation procedure
is in `FIELD_EVALUATION.md`; neither synthetic tests nor the annotated parity
corpus can honestly replace those checks.
