# API stability

`lrgp` is a source-distributed pre-1.0 library with `publish = false`. Its
current API is useful and intentionally public, but remains **provisional**
until an explicit stable-version decision is made.

`api-stability.json` and `api-baseline/lrgp.txt` form the reviewed compatibility
baseline. CI regenerates the full explicit public surface with pinned
`cargo-public-api` and rustdoc versions and rejects an unreviewed difference.
This protects source users from accidental breakage without pretending that a
0.x package has already completed its API design.

lrgp-rs intentionally does not commit a root `Cargo.lock`, because ordinary CI
qualifies the dependency ranges seen by library consumers. Reproducible API
evidence has a narrower need: `api-baseline/Cargo.lock` pins only the tool's
snapshot graph. The verifier installs that lock temporarily and restores the
normal root state afterward. It does not change build, test, or consumer lock
policy.

No module, visibility, signature, encoding, persistence, or runtime behavior
changes at this checkpoint. Any later boundary reduction requires a reviewed
API diff, downstream migration evidence, and an explicit version/changelog
decision.

The canonical snapshot uses all features on `aarch64-apple-darwin` and omits
auto-derived, auto-trait, and blanket implementations.

```sh
cargo install cargo-public-api --version 0.52.0 --locked
rustup toolchain install nightly-2026-08-01 --profile minimal
python3 tools/check-api-baseline.py
```

Use `--update` only after reviewing and recording the compatibility impact.
