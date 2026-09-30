# Contributing

Install Rust through rustup and install ffmpeg. The repository pins Rust 1.98.1,
rustfmt and Clippy in `rust-toolchain.toml`; Cargo declares minimum Rust 1.91.
Use the locked dependency graph when validating changes.

```sh
cargo fmt --check
cargo lint
cargo test-all
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --locked
cargo +1.91.0 check --all-targets --all-features --locked
```

The minimum-version check requires `rustup toolchain install 1.91.0 --profile minimal`.
CI additionally checks Linux, macOS, production-model parity, Docker
inference/readiness and dependency advisories. Use `cargo test --locked` when
including documentation tests locally.

Prefer explicit domain outcomes over flags or strings that conflate a requested
action with a confirmed intervention. Preserve job/session ownership when doing
I/O. Do slow reads, inference, encoding and recording outside the control lock;
printer writes are serialized to prevent conflicting actions. Keep background
work bounded, ordered where required, observable and owned through shutdown.
Propagate inference errors: an unavailable detector is not a clean frame.

Use realistic fakes for frame arrival and printer state transitions. Assert
behavior through public monitor methods and actual HTTP routes where practical.
Add regression tests for meaningful control changes. Keep reference fixtures
independent of Rust code and document intentional compatibility changes.

New configuration settings need defaults, numeric validation, reference YAML and
tests. Do not commit local credentials, model weights, personal recordings or
generated build/review files. See `docs/FIELD_EVALUATION.md` for the separate
hardware and recognition evidence needed before unattended use.
