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
- **Backward compatible** — recognizes legacy `rlap.v1` messages on inbound

## Quick Start

```rust
use lrgp::apps::tictactoe::TicTacToeApp;
use lrgp::router::LrgpRouter;

let router = LrgpRouter::new();
router.register(Box::new(TicTacToeApp::new()));

// List available games
for game in router.list_apps() {
    println!("{} v{} — {}", game.app_id, game.version, game.display_name);
}
```

## Architecture

```
src/
  constants.rs     # Protocol constants, game session types, wire keys
  errors.rs        # LrgpError hierarchy
  envelope.rs      # Pack/unpack/validate LRGP envelopes (msgpack)
  session.rs       # Game session state machine and lifecycle
  app_base.rs      # GameApp trait + GameManifest
  router.rs        # Game registry and move dispatch
  store.rs         # SQLite persistence (game_sessions, game_actions)
  transport.rs     # LXMF ↔ LRGP bridge (pure data, no I/O)
  apps/
    tictactoe.rs   # Built-in Tic-Tac-Toe game
```

## Building a Game

Implement the `GameApp` trait:

```rust
use lrgp::app_base::{GameApp, GameManifest, IncomingResult, OutgoingResult};

struct MyGame;

impl GameApp for MyGame {
    fn app_id(&self) -> &str { "mygame" }
    fn version(&self) -> u32 { 1 }
    fn manifest(&self) -> GameManifest { /* ... */ }
    fn handle_incoming(&self, /* ... */) -> IncomingResult { /* ... */ }
    fn handle_outgoing(&self, /* ... */) -> OutgoingResult { /* ... */ }
    fn validate_action(&self, /* ... */) -> (bool, Option<String>) { /* ... */ }
    fn get_session_state(&self, /* ... */) -> HashMap<String, JsonValue> { /* ... */ }
    fn render_fallback(&self, /* ... */) -> String { /* ... */ }
}
```

## Wire Format

Every game move fits in a single LXMF OPPORTUNISTIC packet (≤295 bytes total):

```
fields[0xFB] = "lrgp.v1"                    # protocol marker
fields[0xFD] = {                             # envelope (≤200 bytes)
    "a": "ttt.1",                            # game_id.version
    "c": "move",                             # command
    "s": "a1b2c3d4e5f6g7h8",                # session_id
    "p": {"i": 4, "b": "____X____", ...},   # payload
}
```

Non-LRGP clients see human-readable fallback text (e.g., `"[LRGP TTT] Move 3"`).

## Protocol Spec

See [SPEC.md](SPEC.md) for the full protocol specification.

## See Also

- [lrgp-py](../lrgp-py) — Python implementation (wire-compatible)

## License

MIT — see [LICENSE](LICENSE).
