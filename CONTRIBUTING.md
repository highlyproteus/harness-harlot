# Contributing to Harness Harlot

Harness Harlot is early-stage and direction changes quickly. **Please reach out before starting work** — open a GitHub issue describing what you want to change and wait for a response before investing time in an implementation.

You are still welcome to open a pull request directly, but unsolicited pull requests may be declined or left unmerged, especially if they change the service/UI boundary, the release/update pipeline, or the security posture without prior discussion.

## Local checks

While iterating, run only the tests of the crates you touched
(`cargo test -p <crate>`); CI runs the full suite on Linux and macOS.

Before every push, run the exact CI gate once:

```bash
scripts/preflight.sh
```

It runs `scripts/check-structure.sh` (the 1550-line file limit),
`cargo fmt --all --check`,
`cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`,
`cargo test --locked --workspace --all-targets --all-features --no-fail-fast`,
and `shellcheck` when installed, and prints each stage's time. Don't re-run it
on unchanged code. Linux-only (`cfg(target_os = "linux")`) code can still fail
only in CI.

Start new work in its own worktree with `scripts/new-worktree.sh NAME BRANCH`.
It clones the main checkout's `target/` copy-on-write, so the first build is
incremental instead of a 4–10 minute cold build. Don't point several worktrees
at one shared `CARGO_TARGET_DIR`: parallel builds then wait on one lock.

Keep commits focused. New behavior should include tests at the lowest useful layer, and reliability claims should include a reproducible failure/recovery scenario.

## Releases

Maintainers release with `scripts/release.sh`; see "Fast release path" in
`docs/macos-release.md`.
