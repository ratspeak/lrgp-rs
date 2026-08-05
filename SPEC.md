# LRGP Specification v0.4

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

The two field values are native MessagePack values in the LXMF field map: `0xFB`
is a MessagePack string and `0xFD` is a MessagePack map. An implementation MUST
NOT first encode either value and then insert those bytes through an API that
wraps arbitrary bytes as MessagePack binary. Such `bin("lrgp.v1")` and
`bin(<encoded envelope>)` wrappers are not LRGP. With `lxmf-core::LxMessage`,
pre-encoded values MUST be installed with `set_msgpack_field`, not `set_field`.

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

### Canonical Form

Every envelope MUST contain exactly the five keys `a`, `c`, `s`, `p`, and
`n`. Missing or additional top-level keys are invalid. msgpack maps are
unordered by specification, so implementations MUST NOT rely on a specific
key ordering when comparing envelopes byte-for-byte. Duplicate map keys are
invalid at every level because collapsing them can produce divergent
sender/receiver interpretations.

Each field has one canonical type and lexical form:

| Key | Canonical value |
|-----|-----------------|
| `a` | msgpack string `<game_id>.<version>`; game ID matches `[a-z][a-z0-9_.-]*`, version is a canonical positive decimal `u32` with no sign or leading zero |
| `c` | msgpack string matching `[a-z][a-z0-9_]*` |
| `s` | msgpack string containing exactly 16 lowercase hexadecimal characters |
| `p` | msgpack map |
| `n` | exactly 8 bytes encoded as msgpack binary (`bin8`) |

An implementation MUST reject an envelope that is not canonical, exceeds the
200-byte packed envelope budget, names an unsupported app/version, or names an
action absent from the selected app manifest. The standard `error` action is
supported independently of an app manifest's action list. A byte-oriented
decoder MUST consume exactly one envelope and reject any trailing bytes.

### Session ID

Session IDs are 8 random bytes encoded as exactly 16 lowercase hexadecimal
characters. The challenger generates the session ID. Implementations MAY
accept an empty session ID only as a local API request to generate a new
outgoing challenge ID; an empty ID is never valid on the wire.

### Nonce

The `n` field is exactly 8 bytes of CSPRNG output, encoded as msgpack `bin8`. It is freshly generated for every outbound envelope and used by receivers for replay deduplication (see Section 3.1).

### 3.1 Replay Protection

Receivers MUST run each decoded and supported envelope through a bounded replay
cache before participant authorization or application dispatch. The cache is
keyed by `(receiving_identity_id, session_id, nonce)` and bounded by:

| Constant | Value | Description |
|---|---|---|
| `NONCE_BYTES` | 8 | nonce length |
| `DEDUP_CACHE_PER_SESSION` | 512 | max entries per session |
| `DEDUP_CACHE_SESSIONS` | 1024 | max receiving-identity/session namespaces |
| `DEDUP_TTL_SECONDS` | 600 | per-entry TTL (10 min) |

A receiver first probes the scoped cache without recording a fresh nonce or
evicting any existing entry. `Replay` means the scoped nonce is already
present: drop the envelope silently. For a fresh probe, authorize the transport
sender, then atomically check-and-record the nonce before application mutation.
The second check resolves concurrent duplicate races. An authorization failure
MUST NOT record the nonce, so unauthenticated traffic can neither reserve a
legitimate nonce nor evict legitimate replay entries. The TTL is absolute from
first observation; receiving a duplicate MUST NOT extend it.

If application dispatch succeeds but the receiver cannot durably commit the
result, it MUST restore the pre-dispatch application state and remove only that
accepted `(receiving_identity_id, session_id, nonce)` entry before accepting a
retry. Other replay entries MUST remain intact. An application-level rejection
does not use this transaction rollback path. An authenticated remote `error`
consumes replay state but does not mutate a game session; if durable recording
of that error fails, the receiver MUST remove only its accepted scoped nonce
without restoring or deleting session state.
Nonce entries for terminal sessions (`completed`, `declined`, or `expired`)
MUST remain until their normal nonce TTL expires, so late transport retransmits
remain replays. Explicit deletion of a session MAY also delete only that local
identity/session's replay namespace.

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

### Participant Binding and Session Identity

Every two-player session is scoped by `(local_identity_id, session_id)` and is
bound to exactly one transport-authenticated remote participant. This session
namespace is global across apps: two apps MUST NOT own the same session ID for
the same local identity. Incoming challenges, new outgoing challenges, and
hydrated records that collide with another app fail before application
mutation. A structurally valid incoming collision retains its replay nonce.

An incoming new challenge binds the session to its authenticated sender. A
local outgoing challenge MUST name its intended recipient before any session
is created; both its local identity and intended recipient MUST be non-empty.
The router stores that binding before accepting later responses.
Every subsequent action, including `error`, MUST be authorized against the
bound participant. Missing sessions, missing participant bindings, expired
sessions, and sender or recipient mismatches fail closed before application
mutation.

The LRGP integration MUST derive the remote participant identifier from
transport-authenticated LXMF/Reticulum delivery metadata. It MUST NOT use
fallback content, an LRGP envelope value, a display name, or other
attacker-controlled presentation data as the authenticated sender. LRGP
enforces the resulting participant binding but, as a transport-independent
protocol, does not independently authenticate a caller-supplied identifier.
Both the authenticated remote identifier and receiving local identity MUST be
non-empty; dispatch fails before replay insertion or application mutation when
either is absent.

An outgoing challenge MUST be rejected if its session ID already exists for
the local identity. A same-participant incoming challenge with an existing
session ID and a fresh nonce is an idempotent transport retry: it produces no
state mutation and no duplicate UI event. The same challenge from any other
sender is unauthorized. A byte-identical retry is handled earlier by replay
deduplication. To retry an outgoing transport send, a sender retransmits the
exact previously prepared envelope; asking the router to prepare a new
challenge with the same ID is a duplicate local action and MUST be rejected.

### Challenge Admission

Implementations MUST bound unsolicited pending challenges for each local
identity. After replay filtering and participant authorization, a new incoming
challenge for which the selected app has no existing session is admitted only
when both limits remain below their caps:

| Scope | Limit |
|---|---:|
| Pending sessions from one remote participant | 16 |
| Pending sessions for one local identity, across all apps | 128 |

Counts MUST apply status-specific TTL before counting and include only sessions
that remain `pending`. The participant limit is checked before the identity
limit. Admission and session creation MUST be atomic or serialized across
apps. A same-participant retry of an existing session bypasses admission and
remains idempotent. Rejection MUST NOT evict or modify any existing session,
and active or terminal sessions are never counted or evicted. The rejected
challenge's nonce remains consumed; an implementation MAY drop the challenge
without sending an error to avoid amplification.

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

### Draw Offer Ownership

An outstanding draw offer is local session state consisting of both a boolean
and the offering participant's authenticated identity (`draw_offered_by`). The
owner field is not transmitted; it is derived from the authenticated sender of
an incoming offer or the local identity that prepares an outgoing offer.

Only the other bound participant may send `draw_accept` or `draw_decline`. A
participant MUST NOT answer its own offer, and either response without a
complete outstanding offer MUST be rejected before mutation. A second plain
offer MUST NOT replace an outstanding offer or its owner. The offer and owner
MUST be cleared together on a move, valid response, resignation, terminal
transition, or verified claim. A hydrated legacy record whose offer flag lacks
an owner is not answerable and MUST be normalized to no outstanding offer.

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

For turn-based games, a move against an `active` session whose stored `turn`
is unset MUST be rejected: every accept handler assigns `turn`, so an empty
value means corrupted or desynchronized state, and validators fail closed
rather than guess. Claimed terminal state (`x`/`r`/`w`) is never trusted —
receivers recompute it from their replayed local state and reject mismatches.

Routers MUST validate local outgoing intent before an application mutates its
session. An invalid outgoing action returns a typed local failure and leaves
state unchanged. For inbound application validation, the router snapshots the
session before dispatch and restores that snapshot whenever the application
returns a rejection. Rejected inbound actions retain their nonce in the replay
cache so retransmission cannot repeatedly exercise validation or produce
duplicate error responses.

---

## 9. Error Actions

When a receiver rejects an action, it may send the standard `error` action:

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

The error payload MUST contain exactly the three keys `code`, `msg`, and `ref`.
All three values MUST be non-empty msgpack strings. `ref` names the command that
was rejected; it is not a nonce or action correlation identifier.

After canonical validation, replay filtering, and participant authorization,
an incoming `error` action is surfaced as a typed remote protocol error. It
MUST NOT be dispatched to the game handler, interpreted as a local rejection,
used to roll back session state, or answered with another `error`. Duplicate
remote errors are still silently dropped by replay protection.

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

The status-specific TTL MUST be checked whenever a stored session is hydrated
or loaded for listing, before inbound authorization, and before outbound
validation. An expiry transition is durable local state. A hydrated session
MUST also have a canonical session ID, a non-empty local identity, and an
app/version matching the selected implementation.

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

These are the required interoperable core keys. Manifests are local
discovery/API metadata and are not transmitted in LRGP envelopes, so an
implementation MAY expose additional namespaced or implementation-specific
keys. Consumers MUST ignore unknown manifest keys. Such extensions do not
change the wire protocol and another implementation is not required to expose
the same local metadata.

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

The router/application boundary MUST provide TTL-aware get, upsert (hydrate),
list, and remove operations. Explicit removal MUST delete only the selected
local identity/session record; it MAY also delete the matching scoped replay
cache as described in Section 3.1.

Persistent storage MUST distinguish initial insertion from mutation. An initial
session insert with an existing primary key MUST fail instead of replacing the
participant binding or state. Updates MUST target an explicit allowlist of
mutable columns and MUST NOT change `session_id`, `identity_id`, `app_id`,
`app_version`, `contact_hash`, or `initiator`. Action rows are append-only: a
duplicate action number MUST fail rather than replace history. Session deletion
and deletion of that session's action rows MUST be one transaction.

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

#### Canonical Command Payloads

Inbound wire payloads MUST contain exactly the keys shown; missing keys,
additional keys, and values of the wrong MessagePack type are invalid and MUST
be rejected before session mutation.

| Command | Exact wire payload |
|---------|--------------------|
| `challenge` | `{}` |
| `accept` | `{b, t}` where `b="_________"` and `t` is the challenge's stored first-turn identity |
| `decline` | `{}` |
| non-terminal `move` | `{i, b, n, t, x}` with `x=""` |
| winning `move` | `{i, b, n, t, x, w}` with `x="win"`, empty `t`, and `w` equal to the authenticated mover |
| drawn `move` | `{i, b, n, t, x}` with `x="draw"` and empty `t` |
| `resign` | `{}` |
| `draw_offer` | `{}` |
| `draw_accept` | `{}` |
| `draw_decline` | `{}` |

The local outgoing API accepts concise intent rather than caller-forged state:
`move` accepts exactly `{i}` and all other TicTacToe actions above accept `{}`.
The game implementation derives the final canonical wire payload. `error`
uses the global schema in Section 9.

---

## B. Chess Reference Game

Chess (`chess.1`) is the built-in chess implementation. App ID `"chess"`, version `1`, session type `turn_based`, validation `both`. White is selected by a coin flip when the responder accepts; the responder communicates the White-player hash back via the `w` key in the ACCEPT payload.

### Wire Format Principles

- **UCI moves only.** Every move is a UCI string (`e2e4`, `e7e8q`). FEN, SAN, and board snapshots are never transmitted.
- **State by replay.** Each peer reconstructs the current position by replaying the UCI history on the starting FEN. Both peers do this independently (validation = `both`); a divergence is a protocol error.
- **Terminal reasons are 2-3 char codes.** Keeps move envelopes well under the 200-byte budget.
- **Threefold repetition and the fifty-move rule are claim-based.** A peer must explicitly send `draw_offer` with the appropriate reason (`3fr`/`50m`); the rule is not auto-detected mid-game. The receiver verifies the claim against its replayed position: a valid claim terminates the game as a draw immediately (FIDE semantics — no `draw_accept` round-trip), while an invalid claim degrades to a plain draw offer. The claimant pre-terminates its local session on a valid claim.

### Payload Schema

| Key | Type | Used In | Description |
|-----|------|---------|-------------|
| `m` | str | move | UCI move (`e2e4`, `e7e8q` for promotions) |
| `n` | int | move | Ply counter, 0-based (0 = White's first move) |
| `x` | str | move | Terminal status: `""`, `"win"`, `"draw"` |
| `r` | str | move, draw_offer | Terminal reason (see codes below) or claim reason on `draw_offer` |
| `w` | str | move (terminal=win), accept | Winner identity hash (move) OR White-player identity hash (accept) — context-dependent on `c` |

The `w` key reuses the same character in two payload contexts. Receivers MUST disambiguate by looking at the message command (`accept` → White-player; `move` with `x="win"` → winner).

#### Canonical Command Payloads

Inbound wire payloads MUST contain exactly the keys shown; missing keys,
additional keys, and values of the wrong MessagePack type are invalid and MUST
be rejected before session mutation.

| Command | Exact wire payload |
|---------|--------------------|
| `challenge` | `{}` |
| `accept` | `{w}` where `w` is one of the two bound participant identities |
| `decline` | `{}` |
| non-terminal `move` | `{m, n, x}` with `x=""` |
| winning `move` | `{m, n, x, r, w}` with `x="win"`, a non-empty terminal reason, and `w` equal to the authenticated mover |
| drawn `move` | `{m, n, x, r}` with `x="draw"` and a non-empty terminal reason |
| `resign` | `{}` |
| plain `draw_offer` | `{}` |
| claim `draw_offer` | `{r}` where `r` is exactly `3fr` or `50m` |
| `draw_accept` | `{}` |
| `draw_decline` | `{}` |

The local outgoing API accepts concise intent rather than caller-forged state:
`move` accepts exactly `{m}`; `draw_offer` accepts `{}` or exactly `{r}`; all
other Chess actions above accept `{}`. The game implementation derives the
final canonical wire payload. A recognized but locally ineligible claim reason
degrades to a plain outstanding offer while retaining `{r}` on the wire so the
receiver independently verifies eligibility. `error` uses Section 9.

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

A move that delivers checkmate carries `x="win"`, `r="cm"`, and `w` = the mating player's hash. A claim-based draw is sent as `draw_offer` with `r` set to the claim reason; a receiver that verifies the claim against its replayed position transitions directly to `completed` with terminal=`draw` (no `draw_accept` round-trip). A plain `draw_offer` (no claim reason, or an invalid claim) still requires `draw_accept`.

### Engine Notes

The reference Rust implementation uses [cozy-chess](https://crates.io/crates/cozy-chess); the reference Python implementation uses [python-chess](https://pypi.org/project/chess/). Any chess library that implements legal-move generation, checkmate / stalemate / insufficient-material detection, and threefold / fifty-move-rule predicates can be substituted as long as it produces canonical UCI strings.
