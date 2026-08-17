# Rust API

Use `lrgp::protocol` for envelope construction, validation, serialization, and
LXMF field embedding. Its items are exact re-exports of the existing constants,
envelope, error, and transport types and functions, so the facade introduces no
conversion or behavior of its own.

The existing `lrgp::constants`, `lrgp::envelope`, `lrgp::errors`, and
`lrgp::transport` paths remain supported. The compiled examples exercise the
protocol facade while keeping router, game, and session imports explicit.

## Stability

`lrgp` remains a provisional pre-1.0 library. The protocol facade is its
recommended application path, but it does not stabilize the entire crate.

`LrgpRouter`, `GameApp`, session and store ownership, replay-cache
implementation, built-in games, raw map-conversion helpers, and deprecated
`pack_into_fields` remain module-qualified provisional APIs. Promoting or
reducing those areas requires a separate architecture and version decision.

lrgp-rs intentionally does not commit a root `Cargo.lock`, because normal CI
qualifies the dependency ranges seen by library consumers. Reproducible API
evidence uses `api/snapshots/Cargo.lock` only for the snapshot graph; the
verifier installs it temporarily and restores the normal root state.

## Compatibility checks

The `api/` directory contains the evidence used by CI:

- `stability.json` records the package tier, source commits, snapshot hash, and
  current review decision;
- `snapshots/` records the explicit all-feature Apple ARM64 Rust API, its lock,
  and the manifest, feature, dependency, target, and MSRV contract; and
- `fixtures/` compiles recommended and retained imports as an external
  consumer.

These checks catch accidental changes, but they do not replace wire vectors,
Python parity, persistence tests, platform builds, or manual review. The API
snapshot omits auto-derived, auto-trait, and blanket implementations and is not
by itself a complete SemVer verdict.

Run the checks with:

```sh
python3 tools/check-api-baseline.py
python3 tools/check-api-manifest.py
python3 tools/check-api-compatibility.py
cargo check --manifest-path api/fixtures/Cargo.toml --locked
```

Snapshot updates require a clean source commit and an explicit review recorded
in `api/stability.json`. Additions, removals, deprecations, platform impact, and
version consequences must be reviewed before accepting new evidence.
