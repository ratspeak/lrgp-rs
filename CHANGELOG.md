# Changelog

## Unreleased

### Added

- A pinned public API compatibility snapshot and package stability ledger;
  `lrgp` remains provisional and no visibility or signature changed.
- **Four in a Row (`four_in_a_row.1`)** — built-in two-player 7x6 gravity
  game with theme-neutral `A`/`B` markers and two-side deterministic
  validation. Move envelopes carry only `{c, n, x}` (plus `w` for a win);
  each peer reconstructs the canonical row-major board independently.
- Cross-language binary fixtures and adversarial coverage for gravity, all
  four win directions, full-board and negotiated draws, terminal claims,
  participant authorization, persistence hydration, rollback, and wire size.

## 0.4.0 — 2026-08-04

### Breaking

- Canonical envelope, session ID, native LXMF field types, strict built-in
  payloads, participant binding, and scoped replay semantics now match
  `lrgp-py` and the normative specification.
- `LrgpStore::save_session` is insert-only. Existing records must be changed
  through the explicit mutable-field allowlist in `update_session`.

### Added

- Typed router results, explicit-participant outgoing preparation, TTL-aware
  hydration/list/removal, challenge-admission limits, and public transactional
  snapshot/rollback helpers for durable inbound and outbound integration.
- Draw-offer ownership, legacy hydration normalization, strict terminal claim
  verification, canonical Python interoperability vectors, and duplicate-key /
  trailing-byte decoder rejection.

## 0.3.1 — 2026-05-01

### Added

- **`LrgpRouter::snapshot_before_outgoing`** and **`LrgpRouter::rollback_outgoing`** — convenience wrappers around the trait-level `GameApp::snapshot_session` / `rollback_session` for transactional dispatch. Standard pattern: snapshot, dispatch, send, rollback on send failure. Returns `None` for unknown apps or apps that haven't opted in to rollback (the trait default returns `None`). Promoted from the Ratspeak vendored copy of this crate.

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
