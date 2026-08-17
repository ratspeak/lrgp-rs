# Provisional protocol API

`lrgp::protocol` is the canonical provisional application path for LRGP-01:
envelope construction, validation, serialization, and LXMF field embedding.
Its items are exact re-exports of the existing constants, envelope, error, and
transport identities, so the facade introduces no conversion or behavior.

The existing `lrgp::constants`, `lrgp::envelope`, `lrgp::errors`, and
`lrgp::transport` paths remain supported. This milestone adds no deprecations.

The facade deliberately excludes `LrgpRouter`, `GameApp`, session and store
ownership, replay-cache implementation, built-in games, raw map-conversion
helpers, and deprecated `pack_into_fields`. Those remain provisional,
module-qualified APIs and require a later architecture decision before any
promotion or reduction.

The four compiled examples exercise canonical protocol imports while keeping
router, game, and session imports explicit. The external fixture compiles both
canonical and retained paths. Wire vectors, Python parity, application
persistence, and platform builds remain independent compatibility evidence.
