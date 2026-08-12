# Not a Harness identity migration

The user-facing product, native window, executables, default runtime paths, and
diagnostics use **Not a Harness**. Existing local data remains readable without
an eager move or copy.

## Compatibility rules

New environment variables take precedence. Their Rust Mux equivalents are used
only when the new variable is unset:

| Current | Legacy fallback |
| --- | --- |
| `NOT_A_HARNESS_SOCKET` | `RUST_MUX_SOCKET` |
| `NOT_A_HARNESS_STATE_DIR` | `RUST_MUX_STATE_DIR` |
| `NOT_A_HARNESS_CONFIG` | `RUST_MUX_CONFIG` |
| `NOT_A_HARNESS_PANE_ID` | `RUST_MUX_PANE_ID` |

Child shells receive both pane-ID variables during the compatibility period.
For default config, state, and socket paths, the new `not-a-harness` location is
used for fresh installs. If the new location is absent and the corresponding
legacy `rust-mux` location exists, Not a Harness reuses the legacy location in
place. Once a new location exists, it takes precedence.

This strategy avoids destructive automatic moves and keeps rollback possible.
Users can migrate deliberately by stopping both processes, copying the legacy
contents to the new owner-only location, verifying them, and only then removing
the legacy copy.

## Intentionally stable internal identifiers

The Cargo package names, Rust crate import names, historical task/PDR goal IDs,
and their directory paths remain `rust-mux-*` or `rust-mux`. They are internal
compatibility and history surfaces rather than user-facing branding. The built
binary names are `not-a-harness` and `not-a-harness-service`.
