# Contributing to Not a Harness

Not a Harness is early-stage. Start by reading the README and the task plan, and discuss changes that alter the service/UI boundary before implementation.

## Local checks

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Keep commits focused. New behavior should include tests at the lowest useful layer, and reliability claims should include a reproducible failure/recovery scenario.
