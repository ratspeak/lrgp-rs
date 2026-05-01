# LRGP Specification v0.3

**Lightweight Reticulum Gaming Protocol**

This document is the normative reference for LRGP. It is implementable without seeing the Rust or Python reference code.

---

## 1. Overview

LRGP defines how multiplayer game sessions are encoded as LXMF messages over Reticulum. Clients that don't understand LRGP see human-readable fallback text in the standard LXMF content field.

LRGP v1 is **2-player only**. All sessions have exactly one initiator and one responder.

---

## 2. LXMF Field Allocation

LRGP uses two LXMF custom extension fields:

| Field | ID | Value |
|-------|----|-------|
| `FIELD_CUSTOM_TYPE` | `0xFB` (251) | `"lrgp.v1"` |
| `FIELD_CUSTOM_META` | `0xFD` (253) | Envelope dict (see Section 3) |

All fields are serialized via **msgpack** (not JSON).

---

## 3. Envelope Schema

The envelope is a msgpack dict stored in `fields[0xFD]`:

```
{
    "a": "<game_id>.<version>",    # e.g. "ttt.1"
    "c": "<command>",              # e.g. "move"
    "s": "<session_id>",          # 16-char hex (8 random bytes)
    "p": { <payload> },           # game-specific, short keys
    "n": <8 bytes>                # CSPRNG replay-dedup nonce (msgpack bin8)
}
```

All keys are single characters to minimize wire size. The `game_id` and `version` are combined into a single string to save one key-value pair.

### Required Fields

All five keys (`a`, `c`, `s`, `p`, `n`) MUST be present in every envelope. msgpack maps are unordered by spec, so implementations MUST NOT rely on a specific key ordering when comparing envelopes byte-for-byte.

### Session ID

Session IDs are 8 random bytes encoded as 16 hexadecimal characters. The challenger generates the session ID.

### Nonce

The `n` field is exactly 8 bytes of CSPRNG output, encoded as msgpack `bin8`. It is freshly generated for every outbound envelope and used by receivers for replay deduplication (see Section 3.1).

### 3.1 Replay Protection

Receivers MUST run each decoded envelope through a per-session bounded LRU before dispatch. The cache is keyed by `(session_id, nonce)` and bounded by:

| Constant | Value | Description |
|---|---|---|
| `NONCE_BYTES` | 8 | nonce length |
| `DEDUP_CACHE_PER_SESSION` | 512 | max entries per session |
| `DEDUP_TTL_SECONDS` | 600 | per-entry TTL (10 min) |

A `Fresh` verdict means the envelope has not been seen in the cache TTL window — dispatch normally and record the nonce. `Replay` means the `(session_id, nonce)` pair is already present — drop the envelope silently. Implementations SHOULD drop the per-session cache when a session reaches a terminal state (`completed` / `declined` / `expired`).

---

## 4. Size Constraints

| Limit | Value | Source |
|-------|-------|--------|
| Envelope (packed) | max **200 bytes** | LRGP budget rule |
| OPPORTUNISTIC content | max **295 bytes** | `LXMessage.ENCRYPTED_PACKET_MAX_CONTENT` |
| DIRECT packet content | max **319 bytes** | `LXMessage.LINK_PACKET_MAX_CONTENT` |
| LXMF overhead | **112 bytes** | 16B dest + 16B src + 64B sig + 8B ts + 8B structure |

LXMF content is packed as `[timestamp, title, content, fields_dict]`.

If content exceeds 295 bytes, LXMF silently escalates from OPPORTUNISTIC to DIRECT delivery, which requires a full Reticulum link handshake. LRGP envelopes MUST be designed to fit within OPPORTUNISTIC limits.

---

## 5. Fallback Text

The LXMF `content` field IS the fallback text. There is no separate fallback key in the envelope.

Format: `[LRGP <GameName>] <description>`

Examples:
- `[LRGP TTT] Sent a challenge!`
- `[LRGP TTT] Move 3`
- `[LRGP TTT] X wins!`

Non-LRGP clients display this as a regular message.

---

## 6. Session Lifecycle

### State Machine

```
challenge --> accept --> action* --> end
    |                      |
    +-> decline            +-> resign
    |                      +-> draw_offer --> draw_accept
    +-> expire (local)     |               +-> draw_decline
                           +-> error (receiver -> sender)
```

### Commands

| Command | Description |
|---------|-------------|
| `challenge` | Initiate a new game session |
| `accept` | Accept a challenge |
| `decline` | Decline a challenge |
| `move` | Game-specific action (e.g., place a piece) |
| `resign` | Voluntary forfeit |
| `draw_offer` | Propose a draw |
| `draw_accept` | Accept a draw proposal |
| `draw_decline` | Decline a draw proposal |
| `error` | Reject an invalid action |

### Status Transitions

| From | Command | To |
|------|---------|-----|
| `pending` | `accept` | `active` |
| `pending` | `decline` | `declined` |
| `active` | `move` (terminal) | `completed` |
| `active` | `resign` | `completed` |
| `active` | `draw_accept` | `completed` |
| `active` | `move` (normal) | `active` |
| `active` | `draw_offer` | `active` |
| `active` | `draw_decline` | `active` |
| `active` | `error` | `active` |

---

## 7. Game Session Types

| Type | Description |
|------|-------------|
| `turn_based` | Players alternate turns (e.g., Tic-Tac-Toe, Chess) |
| `real_time` | Both players can act at any time |
| `round_based` | Multiple rounds with scoring between rounds |
| `single_round` | Single round per session (e.g., coin flip, rock-paper-scissors) |

---

## 8. Validation Models

| Model | Description | Error Behavior |
|-------|-------------|----------------|
| `sender` | Sender validates before sending; receiver trusts | No error actions sent |
| `receiver` | Receiver validates on receipt; rejects invalid | Sends `error` action |
| `both` | Both sides validate independently | Receiver sends `error` if validation disagrees |

---

## 9. Error Actions

When a receiver rejects an action:

```
{
    "a": "<game_id>.<version>",
    "c": "error",
    "s": "<session_id>",
    "p": {
        "code": "<error_code>",
        "msg": "<human-readable message>",
        "ref": "<command that caused the error>"
    }
}
```

### Standard Error Codes

| Code | Meaning |
|------|---------|
| `unsupported_app` | Receiver doesn't have this game |
| `invalid_move` | Move failed validation |
| `not_your_turn` | Out-of-turn action |
| `session_expired` | Session timed out on receiver |
| `protocol_error` | Malformed envelope or unknown command |

Error actions are best-effort. If the error itself fails to deliver, the sender sees no response.

---

## 10. Session Expiry

| Status | Default TTL | Meaning |
|--------|-------------|---------|
| `pending` | 24 hours | Unanswered challenges expire |
| `active` | 7 days | Inactive sessions expire |
| `completed` | N/A | Preserved indefinitely |

Enforcement is **local-only**: each peer expires sessions independently based on its own clock. No LXMF message is sent on expiry.

A 1-hour grace period is applied to account for clock skew between peers.

Games MAY override default TTLs via their manifest.

---

## 11. Delivery Method Guidelines

Games declare preferred delivery per command. LXMF auto-escalates if content exceeds limits, so these are preferences, not guarantees.

| Action | Preferred | Rationale |
|--------|-----------|-----------|
| `challenge` | OPPORTUNISTIC | Small, fire-and-forget |
| `accept` | OPPORTUNISTIC | Small, includes initial state |
| `decline` | OPPORTUNISTIC | Minimal payload |
| `move` | OPPORTUNISTIC | Must fit in 295B |
| `resign` | DIRECT | Delivery confirmation important |
| `draw_offer` | OPPORTUNISTIC | Small |
| `draw_accept` / `draw_decline` | DIRECT | State-changing |
| `error` | OPPORTUNISTIC | Informational |

---

## 12. Game Manifest

Each game declares a manifest:

```
{
    "app_id": "<string>",
    "version": <int>,
    "display_name": "<string>",
    "icon": "<string>",
    "session_type": "turn_based" | "real_time" | "round_based" | "single_round",
    "max_players": 2,
    "validation": "sender" | "receiver" | "both",
    "actions": [<list of command strings>],
    "preferred_delivery": {<command: method>},
    "ttl": {"pending": <seconds>, "active": <seconds>}
}
```

---

## 13. Large Payloads

Most LRGP actions fit in a single packet. For larger data:

**Strategy A**: LXMF Resource auto-escalation. If DIRECT content exceeds 319 bytes, LXMF transfers as a Resource over the link (up to ~3.2 MB). Transparent to the game layer.

**Strategy B**: `FIELD_FILE_ATTACHMENTS` (`0x05`). For explicit bulk data, use the standard LXMF file attachment field alongside the LRGP envelope.

---

## 14. Cross-Client Adoption Levels

| Level | Description |
|-------|-------------|
| **None** | Client ignores LRGP fields; shows fallback text |
| **Basic** | Client recognizes LRGP fields; shows enhanced notification |
| **Full** | Client renders interactive game UI |

Any LXMF client achieves "None" level by default — fallback text appears as a regular message.

---

## 15. Serialization

All LRGP data MUST be serialized with msgpack. JSON is NOT supported on the wire. This is a hard constraint — every byte matters on LoRa links.

---

## 16. Session Storage Schema

### game_sessions

| Column | Type | Description |
|--------|------|-------------|
| `session_id` | TEXT | 16-char hex, part of composite PK |
| `identity_id` | TEXT | Local identity, part of composite PK |
| `app_id` | TEXT | Game identifier |
| `app_version` | INTEGER | Protocol version |
| `contact_hash` | TEXT | Remote peer's identity hash |
| `initiator` | TEXT | Who sent the challenge |
| `status` | TEXT | pending/active/completed/expired/declined |
| `metadata` | TEXT (JSON) | Game-specific state blob |
| `unread` | INTEGER | 0 or 1 |
| `created_at` | REAL | Unix timestamp |
| `updated_at` | REAL | Unix timestamp |
| `last_action_at` | REAL | Unix timestamp (used for TTL) |

Primary key: `(session_id, identity_id)`

### game_actions (optional)

| Column | Type | Description |
|--------|------|-------------|
| `session_id` | TEXT | Session reference |
| `identity_id` | TEXT | Local identity |
| `action_num` | INTEGER | Sequence number |
| `command` | TEXT | LRGP command |
| `payload_json` | TEXT | Serialized payload |
| `sender` | TEXT | Who sent this action |
| `timestamp` | REAL | Unix timestamp |

Unique constraint: `(session_id, identity_id, action_num)`

---

## A. TicTacToe Reference Game

TicTacToe (`ttt.1`) is the built-in reference game demonstrating LRGP.

### Payload Schema

| Key | Type | Used In | Description |
|-----|------|---------|-------------|
| `i` | int | move | Cell index (0–8) |
| `b` | str | move, accept | Board state (9 chars: `_`, `X`, `O`) |
| `n` | int | move | Move number (1-based) |
| `t` | str | move, accept | Hash of player whose turn it is next |
| `x` | str | move | Terminal status: `""`, `"win"`, `"draw"` |
| `w` | str | move | Winner's hash (only when `x == "win"`) |

---

## B. Chess Reference Game

Chess (`chess.1`) is the built-in chess implementation. App ID `"chess"`, version `1`, session type `turn_based`, validation `both`. White is selected by a coin flip when the responder accepts; the responder communicates the White-player hash back via the `w` key in the ACCEPT payload.

### Wire Format Principles

- **UCI moves only.** Every move is a UCI string (`e2e4`, `e7e8q`). FEN, SAN, and board snapshots are never transmitted.
- **State by replay.** Each peer reconstructs the current position by replaying the UCI history on the starting FEN. Both peers do this independently (validation = `both`); a divergence is a protocol error.
- **Terminal reasons are 2-3 char codes.** Keeps move envelopes well under the 200-byte budget.
- **Threefold repetition and the fifty-move rule are claim-based.** A peer must explicitly send `draw_offer` with the appropriate reason; the rule is not auto-detected mid-game.

### Payload Schema

| Key | Type | Used In | Description |
|-----|------|---------|-------------|
| `m` | str | move | UCI move (`e2e4`, `e7e8q` for promotions) |
| `n` | int | move | Ply counter, 0-based (0 = White's first move) |
| `x` | str | move | Terminal status: `""`, `"win"`, `"draw"` |
| `r` | str | move, draw_offer | Terminal reason (see codes below) or claim reason on `draw_offer` |
| `w` | str | move (terminal=win), accept | Winner identity hash (move) OR White-player identity hash (accept) — context-dependent on `c` |

The `w` key reuses the same character in two payload contexts. Receivers MUST disambiguate by looking at the message command (`accept` → White-player; `move` with `x="win"` → winner).

### Terminal Reason Codes

| Code | Meaning |
|------|---------|
| `cm` | Checkmate |
| `sm` | Stalemate |
| `ins` | Insufficient material |
| `3fr` | Threefold repetition (claimed) |
| `50m` | Fifty-move rule (claimed) |
| `rsn` | Resignation |
| `agr` | Draw by agreement |

A move that delivers checkmate carries `x="win"`, `r="cm"`, and `w` = the mating player's hash. A claim-based draw is sent as `draw_offer` with `r` set to the claim reason; the opponent responds with `draw_accept` (which transitions the session to `completed` with terminal=`draw`).

### Engine Notes

The reference Rust implementation uses [cozy-chess](https://crates.io/crates/cozy-chess); the reference Python implementation uses [python-chess](https://pypi.org/project/chess/). Any chess library that implements legal-move generation, checkmate / stalemate / insufficient-material detection, and threefold / fifty-move-rule predicates can be substituted as long as it produces canonical UCI strings.
