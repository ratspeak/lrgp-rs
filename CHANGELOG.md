# Changelog

## Unreleased

### Added

- **Hex (`hex.1`), Gomoku (`gomoku.1`), and Reversi (`reversi.1`)** — three
  built-in stone-placement games sharing one wire contract: moves are a
  single square such as `h8` in `{m, n, x}` (plus `w` on a win), and each
  peer replays the history. Hex is 11x11 with the swap rule, Gomoku is 15x15
  freestyle (five or more wins), and Reversi passes automatically when a
  player has no legal move, so its winner can be the non-mover. Hydration
  replays the stored history. SPEC.md Appendix E is normative.
- **Dots and Boxes (`dots_and_boxes.1`) and Mancala (`mancala.1`)** — two
  more built-ins on the same engine and wire contract. Dots and Boxes is a
  5x5-box grid with moves naming a line (`c3h`, `c3v`); completing a box
  claims it and earns another move. Mancala uses Kalah rules with six pits of
  four seeds, moves naming a pit (`a`–`l`), extra turns for ending in your
  store, and captures from an empty pit of your own. Extra turns need no
  message: each peer derives who moves next. SPEC.md Appendix F is normative.
- Cross-language fixtures for the single-token games, including one reference
  game per game (a Hex swap, a Reversi automatic pass, a Dots and Boxes
  double box, and Mancala extra turns and captures; Reversi, Dots and Boxes,
  and Mancala are each won by the player who did not move last) plus a
  full-board Gomoku draw and a 24–24 Mancala draw, all shared with `lrgp-py`.

- **Checkers (`checkers.1`)** — built-in American checkers / English
  draughts with mandatory captures, completed multi-jumps, crowning that ends
  the move, and two-side validation by replay. Move envelopes carry a
  space-separated square path such as `a5 c3 e1` in `{m, n, x}` (plus `r`/`w`
  for a win); threefold-repetition and forty-move draws are claim-based.
  Hydration replays the stored history and rejects inconsistent records.
- Cross-language Checkers binary fixtures and a 33-ply reference game shared
  with `lrgp-py`; both engines must agree on every legal-move list and on
  the final winning payload.

## 0.4.1 — 2026-08-17

### Added

- Add the exact-reexport `lrgp::protocol` facade for provisional envelope,
  validation, and LXMF embedding APIs while retaining all existing paths and
  leaving router, session, and persistence boundaries unchanged.

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
