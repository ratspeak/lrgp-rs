# Source release policy

lrgp-rs source releases are qualified from a clean repository checkout. This
library is not published to a Cargo registry; the package must retain
`publish = false` unless a separate release policy explicitly changes that
decision.

## Version and tag roles

- `Cargo.toml` is the source of the library's semantic version.
- A semantic source-release tag must match that version.
- Ratspeak integration tags identify a tested downstream dependency set. They
  do not replace the library version or changelog and must not move after
  creation.
- Move entries out of `Unreleased` only when preparing an independently
  approved semantic source release.

## Source qualification

Before a semantic source release, verify that the tree is clean and run:

```sh
python3 tools/check-source-release.py
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

lrgp-rs intentionally does not commit `Cargo.lock`: as a library, it is tested
against dependency versions selected from its declared compatibility ranges.
Applications such as Ratspeak provide the lockfile that freezes the deployed
dependency graph. Rust 1.85 remains the declared minimum and is checked
separately in CI.

Tag creation, artifact upload, registry publication, and downstream integration
tagging are separate operations and are not implied by passing these checks.
