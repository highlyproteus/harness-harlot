# Contributing to Harness Harlot

Harness Harlot is early-stage. Start by reading the README and the [product and architecture plan](docs/cmux-informed-product-and-architecture.md), and discuss changes that alter the service/UI boundary before implementation.

## Local checks

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Keep commits focused. New behavior should include tests at the lowest useful layer, and reliability claims should include a reproducible failure/recovery scenario.
