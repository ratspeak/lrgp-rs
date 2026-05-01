# Changelog

## 0.3.0 — 2026-05-01

### Added

- **Chess (`chess.1`)** — second built-in game alongside Tic-Tac-Toe. UCI-only wire format, board state replayed locally via [cozy-chess](https://crates.io/crates/cozy-chess), two-side validation (`VALIDATION_BOTH`). Terminal reason codes (`cm`, `sm`, `ins`, `3fr`, `50m`, `rsn`, `agr`) keep move envelopes ≤150 bytes. Available as `lrgp::apps::chess::ChessApp`.
- **`examples/chess_game.rs`** — challenge → accept → first-move walkthrough with two `LrgpRouter` instances. Run with `--features test-helpers` to pin the coin flip.
- **Chess binary test vectors** — `tests/chess_*.bin` (challenge / accept / move / move-with-promotion / checkmate / resign / draw_offer). Byte-identical to the copies in `lrgp-py/tests/vectors/`.
- **`test-helpers` Cargo feature** — exposes per-app `force_coin` hooks for deterministic vector generation outside the crate.
- **`AppManifest::snapshot_session` / `rollback_session`** — default-method additions to the `GameApp` trait letting apps opt into transactional rollback (used by the chess implementation).

### Changed

- **`GameManifest` → `AppManifest`** — type rename to match the Ratspeak source-of-truth implementation.
- **Wire `n` is now required.** Every envelope MUST carry an 8-byte CSPRNG nonce under key `n`. `unpack_envelope` rejects missing/malformed nonce. `DedupVerdict` simplifies to `Fresh | Replay`.
- **SPEC.md updated to v0.3** — new section 3.1 documents the replay-protection mechanism; new section B documents the Chess reference game; manifest no longer lists `min_players`, `genre`, or `turn_timeout`.

### Removed

- **Legacy protocol markers `rlap.v1` and `ratspeak.game`** are no longer recognized on inbound. Pre-release implementations using these markers must upgrade to `lrgp.v1`.
- **`LegacyNoNonce` dedup verdict.** No longer needed now that the nonce is required at the wire boundary.
- **`AppManifest::min_players`, `genre`, `turn_timeout`** fields. They were optional metadata only ever used by mock test fixtures and added complexity without a real consumer.

---

## 0.2.0 — 2025-03-12

### Breaking — Renamed to LRGP

RLAP (Reticulum LXMF App Protocol) has been renamed and re-purposed to **LRGP** (Lightweight Reticulum Gaming Protocol). The protocol now focuses specifically on multiplayer gaming over Reticulum mesh networks.

#### Wire Protocol
- Protocol marker: `rlap.v1` → `lrgp.v1`
- Legacy `rlap.v1` and `ratspeak.game` messages still recognized on inbound
- All outbound messages use `lrgp.v1`

#### API Renames
- `RlapApp` trait → `GameApp`
- `AppManifest` → `GameManifest`
- `RlapRouter` → `LrgpRouter`
- `RlapStore` → `LrgpStore`
- `RlapError` → `LrgpError`

#### New Features
- `GameManifest` adds `min_players`, `genre`, and `turn_timeout` fields
- New game session types: `round_based`, `single_round`
- `LEGACY_TYPES` array for multi-marker backward compatibility

#### Database
- `app_sessions` table → `game_sessions`
- `app_actions` table → `game_actions`

#### Fallback Text
- Format changed from `[RLAP ...]` to `[LRGP ...]`

---

## 0.1.0 — 2025-02-28

### Initial Release

- Envelope packing/unpacking with msgpack serialization
- Session state machine (pending → active → completed/expired/declined)
- `RlapApp` trait for pluggable applications
- `RlapRouter` for app registration and message dispatch
- `RlapStore` with SQLite persistence (WAL mode, parameterized queries)
- Transport bridge (LXMF field ↔ RLAP envelope)
- TicTacToe reference app with both-side validation
- Cross-compatible binary test vectors (`ttt_challenge.bin`, `ttt_move.bin`, `ttt_move_win.bin`)
