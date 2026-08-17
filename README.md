# LRGP-rs

Rust implementation of the **Lightweight Reticulum Gaming Protocol (LRGP)** — a compact, session-based protocol for multiplayer games over [LXMF](https://github.com/markqvist/LXMF) / [Reticulum](https://github.com/markqvist/Reticulum) mesh networks.

LRGP enables turn-based and real-time multiplayer games to run over LoRa radios, WiFi, TCP, and any other medium Reticulum supports. Game moves are encoded as tiny msgpack envelopes that fit in a single encrypted packet — no link setup needed.

## Features

- **Compact wire format** — msgpack with single-character keys, ~60 bytes per game move
- **Game session state machine** — challenge → accept → play → win/draw/resign lifecycle
- **`GameApp` trait** — implement this trait to create any game
- **`LrgpRouter`** — register games, dispatch moves, manage manifests
- **`LrgpStore`** — SQLite persistence for game sessions and move history
- **Transport bridge** — zero-copy conversion between LRGP envelopes and LXMF fields
- **Replay protection** — every envelope carries an 8-byte CSPRNG nonce; receivers maintain identity-scoped bounded LRUs with an absolute 10-minute TTL
- **Participant binding** — every session is bound to its authenticated remote peer before state-changing actions are accepted
- **Bounded admission** — unsolicited pending challenges are capped per participant and local identity without evicting active games
- **Built-in games** — Tic-Tac-Toe, Chess (via `cozy-chess`), and Four in a Row

## Quick Start

```rust
use lrgp::router::LrgpRouter;

let router = LrgpRouter::with_builtin_apps();

// List available games
for game in router.list_apps() {
    println!("{} v{} — {}", game.app_id, game.version, game.display_name);
}
```

For envelope construction, validation, and LXMF embedding, use the provisional
[`lrgp::protocol`](PROTOCOL_API.md) facade. Router, game, session, and store
APIs remain module-qualified because their long-term ownership boundary is not
being stabilized by this facade.

## Architecture

```
src/
  constants.rs     # Protocol constants, game session types, wire keys
  errors.rs        # LrgpError hierarchy
  envelope.rs      # Pack/unpack/validate LRGP envelopes (msgpack)
  dedup.rs         # Per-session replay-dedup cache (8-byte nonce LRU)
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

Implement the `GameApp` trait:

```rust
use lrgp::app_base::{AppManifest, GameApp, IncomingResult, OutgoingResult};

struct MyGame;

impl GameApp for MyGame {
    fn app_id(&self) -> &str { "mygame" }
    fn version(&self) -> u32 { 1 }
    fn manifest(&self) -> AppManifest { /* ... */ }
    fn handle_incoming(&self, /* ... */) -> IncomingResult { /* ... */ }
    fn handle_outgoing(&self, /* ... */) -> OutgoingResult { /* ... */ }
    fn validate_action(&self, /* ... */) -> (bool, Option<String>) { /* ... */ }
    fn get_session_state(&self, /* ... */) -> HashMap<String, JsonValue> { /* ... */ }
    fn render_fallback(&self, /* ... */) -> String { /* ... */ }
}
```

Games embedded by an application can be registered with
`LrgpRouter::register`. A game shipped as part of LRGP itself must also be
added once to `apps::builtin_games()`. Applications using
`LrgpRouter::with_builtin_apps()` then discover it automatically, without
maintaining their own concrete registration list. The built-in registry tests
enforce unique IDs, coherent manifests, lifecycle actions, and valid delivery
preferences; router tests additionally exercise a third mock game beside the
standard set.

## Wire Format

Every game move fits in a single LXMF OPPORTUNISTIC packet (≤295 bytes total):

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

Every outbound envelope carries an 8-byte CSPRNG nonce under key `n`. Receivers probe each validated envelope without insertion, authorize its transport sender, then atomically check-and-record it before application mutation. The cache is keyed by `(receiving_identity_id, session_id, nonce)`, bounded to 512 nonces per namespace and 1,024 namespaces, and uses an absolute 10-minute TTL from first observation. Duplicates are reported as `DedupVerdict::Replay` and dropped silently. Unauthorized fresh nonces never consume or evict cache entries. Terminal-session nonces remain until that TTL expires so late transport retransmits stay deduplicated; explicit user deletion may remove only the matching local identity/session namespace.

`protocol::pack_envelope`, `protocol::pack_lxmf_fields`, and byte decoders are
checked APIs. They reject non-canonical fields, unsupported lexical forms,
oversize envelopes, and trailing bytes rather than placing malformed LRGP data
on the wire.

`pack_lxmf_fields` returns native MessagePack values. If an integration needs
pre-encoded field bytes, use `protocol::pack_into_preencoded_fields` and, with
`lxmf-core::LxMessage`, install each value using `set_msgpack_field`.
`LxMessage::set_field` is intentionally **not** compatible with this output: it
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
