# Contributing to Harness Harlot

Harness Harlot is early-stage and direction changes quickly. **Please reach out before starting work** — open a GitHub issue describing what you want to change and wait for a response before investing time in an implementation.

You are still welcome to open a pull request directly, but unsolicited pull requests may be declined or left unmerged, especially if they change the service/UI boundary, the release/update pipeline, or the security posture without prior discussion.

## Local checks

Run these before submitting anything:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Keep commits focused. New behavior should include tests at the lowest useful layer, and reliability claims should include a reproducible failure/recovery scenario.
