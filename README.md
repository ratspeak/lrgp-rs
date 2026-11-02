# LRGP-rs

Rust implementation of the **Lightweight Reticulum Gaming Protocol (LRGP)** — a compact, session-based protocol for multiplayer games over [LXMF](https://github.com/markqvist/LXMF) / [Reticulum](https://github.com/markqvist/Reticulum) mesh networks.

LRGP carries game actions in LXMF custom fields over any reachable Reticulum path. The built-in games are turn-based. Each packed game envelope is limited to 200 bytes; title, fallback text, stamps and the rest of LXMF add overhead. The transport chooses packet or Link delivery for the complete message.

## Features

- **Compact wire format** — MessagePack with single-character keys and a 200-byte envelope limit
- **Game session state machine** — challenge → accept → play → win/draw/resign lifecycle
- **`GameApp` trait** — implement this trait for a two-player game
- **`LrgpRouter`** — register games, dispatch moves, manage manifests
- **`LrgpStore`** — SQLite persistence for game sessions and move history
- **Transport bridge** — conversion between LRGP envelopes and native LXMF fields, with no network I/O
- **Replay protection** — every envelope carries an 8-byte CSPRNG nonce; receivers maintain identity-scoped bounded LRUs with an absolute 10-minute TTL
- **Participant binding** — every session is bound to its authenticated remote peer before state-changing actions are accepted
- **Bounded admission** — unsolicited pending challenges are capped per participant and local identity without evicting active games
- **Built-in games** — Tic-Tac-Toe, Chess (via `cozy-chess`), and Four in a Row

## Quick Start

This checkout declares **0.4.1** and requires Rust **1.87+**. The crate is used from source; select the source revision or release your application has qualified. Run the local examples from the repository root:

```sh
cargo run --example basic_envelope
cargo run --example tictactoe_game
cargo run --example chess_game --features test-helpers
```

The chess example uses the test helper to make the challenger White. To list games in an application:

```rust
use lrgp::router::LrgpRouter;

let router = LrgpRouter::with_builtin_apps();

// List available games
for game in router.list_apps() {
    println!("{} v{} — {}", game.app_id, game.version, game.display_name);
}
```

For envelope construction, validation, and LXMF embedding, use the provisional
[`lrgp::protocol`](api/README.md) facade. Router, game, session, and store
APIs remain module-qualified because their long-term ownership boundary is not
being stabilized by this facade.

## Architecture

```
src/
  constants.rs     # Protocol constants, game session types, wire keys
  errors.rs        # LrgpError hierarchy
  envelope.rs      # Pack/unpack/validate LRGP envelopes (msgpack)
  dedup.rs         # Receiving-identity/session replay cache (8-byte nonces)
  session.rs       # Game session state machine and lifecycle
  app_base.rs      # GameApp trait + AppManifest
  router.rs        # Game registry and move dispatch
  store.rs         # SQLite persistence (game_sessions, game_actions)
  transport.rs     # LXMF ↔ LRGP bridge (pure data, no I/O)
  apps/
    tictactoe.rs   # Built-in Tic-Tac-Toe
    chess.rs       # Built-in Chess (cozy-chess engine, UCI wire format)
    four_in_a_row.rs # Built-in 7x6 gravity game (reconstructed compact wire state)
```

## Building a Game

Implement [`GameApp`](src/app_base.rs), using the built-in
[Tic-Tac-Toe implementation](src/apps/tictactoe.rs) as a complete example.
The trait covers the manifest, incoming/outgoing actions, validation, rendering
and session ownership. An application that accepts challenges must also
implement participant binding, authorization, TTL-aware session access,
hydration/list/removal and snapshot/rollback. The default session methods are
not a functioning network game.

Use `LrgpRouter` for network-facing dispatch. Direct game hooks assume the
router has already checked the envelope and authenticated participant.

Games embedded by an application can be registered with
`LrgpRouter::register`. A game shipped as part of LRGP itself must also be
added once to `apps::builtin_games()`. Applications using
`LrgpRouter::with_builtin_apps()` then discover it automatically, without
maintaining their own concrete registration list. The built-in registry tests
enforce unique IDs, coherent manifests, lifecycle actions, and valid delivery
preferences; router tests additionally exercise a third mock game beside the
standard set.

## Wire Format

LRGP uses these native MessagePack fields; the 200-byte envelope budget does not include the rest of the LXMF message:

```
fields[0xFB] = "lrgp.v1"                    # protocol marker
fields[0xFD] = {                             # envelope (≤200 bytes)
    "a": "four_in_a_row.1",                  # game_id.version
    "c": "move",                             # command
    "s": "a1b2c3d4e5f60718",                # session_id (16-char lowercase hex)
    "p": {"c": 3, "n": 1, "x": ""},        # payload (game-specific)
    "n": <8 bytes>,                          # CSPRNG nonce (replay-dedup)
}
```

Non-LRGP clients see human-readable fallback text (e.g., `"[LRGP TTT] Move 3"`, `"[LRGP Chess] e2e4"`, or `"[LRGP Four in a Row] Move 7"`).

### Replay protection

Every outbound envelope carries an 8-byte CSPRNG nonce under key `n`. Receivers probe each validated envelope without insertion, authorize its transport sender, then atomically check-and-record it before application mutation. The cache is keyed by `(receiving_identity_id, session_id, nonce)`, bounded to 512 nonces per namespace and 1,024 namespaces, and uses an absolute 10-minute TTL from first observation. Duplicates are reported as `DedupVerdict::Replay` and dropped silently. Unauthorized fresh nonces never consume or evict cache entries. Ending a session does not clear its nonces; normal TTL expiry and bounded-cache eviction still apply. Explicit user deletion may remove only the matching local identity/session namespace.

`protocol::pack_envelope`, `protocol::pack_lxmf_fields`, and byte decoders are
checked APIs. They reject non-canonical fields, unsupported lexical forms,
oversize envelopes, and trailing bytes rather than placing malformed LRGP data
on the wire.

Current 0.4.1 has a conformance limit: the Rust byte decoder rejects duplicate
keys in the envelope and its direct payload map, but does not reject duplicate
keys in maps nested deeper inside custom payload values.
The specification still requires rejection at every depth. If
your integration accepts nested custom payloads, validate MessagePack maps
recursively for duplicate keys at the input boundary, before normalizing maps
or dispatching the action.

`pack_lxmf_fields` returns native MessagePack values. If an integration needs
pre-encoded field bytes, use `protocol::pack_into_preencoded_fields` and, with
`lxmf-core::message_api::LxMessageDraft`, install each value using `set_msgpack_field`.
`LxMessageDraft::set_field` is intentionally **not** compatible with this output: it
would wrap the encoded string/map as MessagePack binary values, which Python
LRGP peers do not interpret as LRGP fields.

### Integration trust boundary

Pass `LrgpRouter::dispatch_incoming` only the remote identity derived from
authenticated LXMF/Reticulum delivery metadata. Never derive `sender_hash`
from fallback text, an envelope field, or a display name. LRGP binds the value
to a session and rejects later mismatches, but this transport-independent crate
cannot authenticate an arbitrary caller-supplied string itself. Incoming
dispatch rejects an empty sender or receiving identity before replay insertion
or game mutation.

For durable inbound processing, snapshot before dispatch. If the application
mutation succeeds but its external database transaction fails, call
`LrgpRouter::rollback_incoming` with that exact envelope nonce and snapshot.
The method restores/deletes the live session and releases only the matching
identity/session/nonce replay key so an exact transport retransmission can be
applied safely. If durable recording of an authenticated
`IncomingDispatch::RemoteError` fails, use `forget_incoming_nonce` instead:
that result consumed a nonce but did not mutate game state.

## Protocol Spec

See [SPEC.md](SPEC.md) for the full protocol specification.

## See Also

- [lrgp-py](https://github.com/ratspeak/lrgp-py) — Python implementation (wire-compatible)

## License

MIT — see [LICENSE](LICENSE).
