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
`bin(<encoded envelope>)` wrappers are not LRGP. With `lxmf-core::message_api::LxMessageDraft`,
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
A terminal transition (`completed`, `declined`, or `expired`) MUST NOT itself
clear the session's replay entries. They remain subject to the normal absolute
TTL and bounded-cache eviction. Explicit deletion of a session MAY also delete
only that local identity/session's replay namespace. The cache is bounded replay
deduplication, not a durable record of every action ever received.

---

## 4. Size Constraints

The packed LRGP envelope MUST be no larger than **200 bytes**. This limit
covers only `fields[0xFD]`, not the marker, title, fallback text, timestamp,
stamps, signature, addresses or transport framing.

LXMF packs its message body as `[timestamp, title, content, fields_dict]`.
Whether the complete message fits a single Opportunistic or Link packet
depends on the LXMF/Reticulum implementation, version, fields and active Link
limits. An integration MUST use its transport's packing and size checks; the
LRGP `OPPORTUNISTIC_MAX_CONTENT` and `LINK_PACKET_MAX_CONTENT` constants are
legacy estimates, not a packet-fit guarantee.

Keep fallback text short and measure the complete message. A sender may need
Direct delivery and, for a larger Direct message, a Resource. LRGP does not
promise that every valid envelope avoids a Link handshake.

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
    },
    "n": <8 fresh CSPRNG bytes>
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

Games declare a preferred delivery method per command. These are transport hints, not delivery guarantees. The embedding application decides how to apply them and must handle size limits, path availability and delivery failure. For example, Ratspeak 1.0.37 chooses Direct for game messages in Auto mode.

| Action | Preferred | Rationale |
|--------|-----------|-----------|
| `challenge` | OPPORTUNISTIC | Small; avoids Link setup when the complete message fits |
| `accept` | OPPORTUNISTIC | Small, includes initial state |
| `decline` | OPPORTUNISTIC | Minimal payload |
| `move` | OPPORTUNISTIC | Prefer low overhead when the complete message fits |
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

The envelope still MUST fit the 200-byte LRGP budget. For additional data,
an integration can use LXMF Resource delivery or a standard file attachment
field (`FIELD_FILE_ATTACHMENTS`, `0x05`) alongside the envelope. Those bytes
remain part of the complete LXMF message; transfer ceilings and packet versus
Resource selection belong to the selected transport implementation and peer.
Do not split or enlarge the LRGP envelope to bypass its validation limit.

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

---

## C. Four in a Row Reference Game

Four in a Row (`four_in_a_row.1`) is the built-in two-player gravity game.
Its session type is `turn_based` and validation model is `both`. This appendix
defines an LRGP reference application; it does not change the base LRGP
protocol version.

### Board and Player Assignment

- The board has 7 columns and 6 rows. Columns are numbered `0..6` from left
  to right. Rows are numbered `0..5` from top to bottom.
- Local metadata stores exactly 42 ASCII cells in row-major order. Cell index
  is `row * 7 + column`; `_` is empty and `A`/`B` are theme-neutral markers.
- A move occupies the lowest empty cell in the selected column. A full column
  is illegal. Four identical markers horizontally, vertically, or along
  either diagonal wins.
- The challenger is the first player, owns marker `A`, and takes move 1. The
  responder owns marker `B`. Move number `n` is 1-based and alternates the
  marker deterministically: odd=`A`, even=`B`.
- Both peers reconstruct the board from the same starting state and accepted
  column sequence. Board snapshots and next-turn identities are not sent in
  move envelopes.

### Canonical Command Payloads

Inbound wire payloads MUST contain exactly the keys shown. Missing,
additional, or incorrectly typed values MUST be rejected before mutation.

| Command | Exact wire payload |
|---------|--------------------|
| `challenge` | `{}` |
| `accept` | `{t}` where `t` is the stored challenger/first-turn identity |
| `decline` | `{}` |
| non-terminal `move` | `{c, n, x}` with `c` in `0..6`, sequential `n`, and `x=""` |
| winning `move` | `{c, n, x, w}` with `x="win"` and `w` equal to the authenticated mover |
| drawn `move` | `{c, n, x}` with `x="draw"` on the full non-winning board |
| `resign` | `{}` |
| `draw_offer` | `{}` |
| `draw_accept` | `{}` |
| `draw_decline` | `{}` |

The local outgoing API accepts exactly `{c}` for `move` and `{}` for every
other action above. The implementation derives `n`, the landing row/cell,
terminal claim, winner, and next turn. A receiver MUST independently apply
gravity and reject a non-sequential move, full/out-of-range column, move by
the wrong participant, false/missing terminal claim, forged winner, or any
move after completion. `error` uses the global schema in Section 9.

### Canonical Local Metadata

The reference applications expose `board`, `turn`, `first_turn`, `my_marker`,
`first_marker`, `second_marker`, `move_count`, `last_column`, `last_row`,
`last_cell`, `winner`, `terminal`, `draw_offered`, and `draw_offered_by`.
The three `last_*` values are null before move 1. `terminal` is `""`,
`"win"`, `"draw"`, or `"resign"`; only the first three appear on move wire
payloads. Persistence hydration MUST revalidate gravity, marker counts,
move count, turn, last-move coordinates, terminal status, winner identity,
and participant bindings before accepting a stored session.

---

## D. Checkers Reference Game

Checkers (`checkers.1`) is the built-in American checkers / English draughts
implementation. App ID `"checkers"`, version `1`, session type `turn_based`,
validation `both`. This appendix defines an LRGP reference application; it does
not change the base LRGP protocol version.

### Board, Rules, and Player Assignment

- The board is 8x8 with files `a`–`h` and ranks `1`–`8`. `a1` is a dark
  square; only dark squares (file index + rank index even) are playable.
- Black men start on the dark squares of ranks 6–8 and move toward rank 1.
  White men start on the dark squares of ranks 1–3 and move toward rank 8.
  Black moves first.
- A man moves one square diagonally forward. A king moves one square
  diagonally in any direction; kings do not fly.
- A capture jumps an adjacent opposing piece onto the empty square directly
  beyond it. Men capture forward only; kings capture in any direction.
- Captures are mandatory. When several captures exist the mover may choose
  any of them, but a chosen jump sequence MUST be completed while further
  jumps are available.
- Jumped pieces are removed only after the move completes. During the
  sequence they may not be jumped again and they block landing. The moving
  piece's origin square is empty during the sequence.
- A man that reaches the far rank is crowned and its move ends, even if the
  new king could continue jumping.
- A player with no legal move on their turn, including having no pieces,
  loses.
- White's and Black's identities are chosen by a coin flip when the responder
  accepts; the responder sends the Black-player hash back in the `b` key of
  the ACCEPT payload.

### Wire Format Principles

- **Square paths only.** Every move is a path of square names joined by single
  ASCII spaces: the origin followed by each landing square. Examples: `a3 b4`
  (simple move), `a5 c3 e1` (double jump), `e7 c5 a3 c1` (triple jump). Squares
  are lowercase file plus rank digit and MUST be dark. A path has 2–13
  squares. Any other spelling (uppercase, extra or tab whitespace, `-`/`x`
  separators) is invalid. Board snapshots are never transmitted.
- **State by replay.** Each peer reconstructs the current position by replaying
  the path history from the starting position. A path that does not exactly
  match a legal move from the replayed position, including a partial jump
  sequence, is rejected.
- **Draws are claim-based or negotiated.** No position is drawn automatically.
  A peer may send `draw_offer` with `r = "3fr"` when the current position (the
  board plus the side to move) has occurred at least three times, or with
  `r = "40m"` when 80 consecutive plies have contained no capture and no man
  move. The receiver verifies the claim against its replayed position. A valid
  claim ends the game as a draw immediately (no `draw_accept` round trip); an
  invalid claim degrades to a plain draw offer. The claimant pre-terminates its
  local session on a valid claim.

### Payload Schema

| Key | Type | Used In | Description |
|-----|------|---------|-------------|
| `m` | str | move | Square path (`a3 b4`, `a5 c3 e1`) |
| `n` | int | move | Ply counter, 0-based (0 = Black's first move) |
| `x` | str | move | Terminal status: `""` or `"win"` |
| `r` | str | move, draw_offer | Terminal reason (see codes below) or claim reason on `draw_offer` |
| `w` | str | move (terminal=win) | Winner identity hash |
| `b` | str | accept | Black-player (first mover) identity hash |

#### Canonical Command Payloads

Inbound wire payloads MUST contain exactly the keys shown; missing keys,
additional keys, and values of the wrong MessagePack type are invalid and MUST
be rejected before session mutation.

| Command | Exact wire payload |
|---------|--------------------|
| `challenge` | `{}` |
| `accept` | `{b}` where `b` is one of the two bound participant identities |
| `decline` | `{}` |
| non-terminal `move` | `{m, n, x}` with `x=""` |
| winning `move` | `{m, n, x, r, w}` with `x="win"`, `r` of `np` or `nm`, and `w` equal to the authenticated mover |
| `resign` | `{}` |
| plain `draw_offer` | `{}` |
| claim `draw_offer` | `{r}` where `r` is exactly `3fr` or `40m` |
| `draw_accept` | `{}` |
| `draw_decline` | `{}` |

The local outgoing API accepts concise intent rather than caller-forged state:
`move` accepts exactly `{m}`; `draw_offer` accepts `{}` or exactly `{r}`; all
other Checkers actions above accept `{}`. The game implementation derives the
final canonical wire payload. A recognized but locally ineligible claim reason
degrades to a plain outstanding offer while retaining `{r}` on the wire so the
receiver independently verifies eligibility. A receiver MUST reject a
non-sequential ply, an illegal or non-canonical path, a move by the wrong
participant, a false or missing terminal claim, a wrong terminal reason, a
forged winner, or any move after completion. `error` uses Section 9.

### Terminal Reason Codes

| Code | Meaning |
|------|---------|
| `np` | Opponent has no pieces left |
| `nm` | Opponent has pieces but no legal move |
| `3fr` | Threefold repetition (claimed) |
| `40m` | Forty-move rule (claimed) |
| `rsn` | Resignation |
| `agr` | Draw by agreement |

### Canonical Local Metadata

The reference application exposes `board`, `moves`, `my_color`, `first_turn`,
`turn`, `move_count`, `last_move`, `winner`, `terminal`, `terminal_reason`,
`must_capture`, `legal_moves`, `draw_offered`, `draw_offered_by`, and
`draw_offer_reason`. `board` is 64 ASCII cells in row-major order from `a8`
(index 0) to `h1` (index 63): `_` empty, `b`/`w` men, `B`/`W` kings.
`my_color` is `"b"` or `"w"` and `first_turn` is the Black player. `terminal`
is `""`, `"win"`, or `"draw"`. Persistence hydration MUST replay `moves` and
revalidate the board, move count, last move, colors, turn, terminal status,
terminal reason, winner identity, claim eligibility, and participant bindings
before accepting a stored session.

---

## E. Stone-Placement Reference Games

Hex (`hex.1`), Gomoku (`gomoku.1`), and Reversi (`reversi.1`) are built-in
two-player games in which every move places one stone on a named square. They
share one wire contract (E.1) and differ only in their rules (E.2–E.4). All
three use session type `turn_based` and validation `both`. This appendix
defines LRGP reference applications; it does not change the base LRGP
protocol version.

### E.1 Shared Wire Contract

#### Squares and Player Assignment

- A square is a lowercase file letter followed by a decimal rank with no
  leading zero: `a1` is the bottom-left square and ranks grow upward. On an
  N×N board files run `a` onward for N letters and ranks run `1`–`N`. Any
  other spelling is invalid.
- Black moves first. The responder flips a coin when accepting and sends the
  Black player's identity in the `b` key of the ACCEPT payload.
- Each peer reconstructs the position by replaying the accepted move history
  from the starting position. Boards, legal-move lists, and next-turn
  identities are never transmitted.

#### Payload Schema

| Key | Type | Used In | Description |
|-----|------|---------|-------------|
| `m` | str | move | Square (`h8`), or `swap` in Hex |
| `n` | int | move | Ply counter, 0-based: the number of moves already played |
| `x` | str | move | Terminal status: `""`, `"win"`, or `"draw"` |
| `w` | str | move (terminal=win) | Winner identity hash |
| `b` | str | accept | Black-player (first mover) identity hash |

#### Canonical Command Payloads

Inbound wire payloads MUST contain exactly the keys shown; missing keys,
additional keys, and values of the wrong MessagePack type are invalid and MUST
be rejected before session mutation.

| Command | Exact wire payload |
|---------|--------------------|
| `challenge` | `{}` |
| `accept` | `{b}` where `b` is one of the two bound participant identities |
| `decline` | `{}` |
| non-terminal `move` | `{m, n, x}` with `x=""` |
| winning `move` | `{m, n, x, w}` with `x="win"` and `w` equal to the identity the replayed position names as winner |
| drawn `move` | `{m, n, x}` with `x="draw"` (Gomoku and Reversi only) |
| `resign` | `{}` |
| `draw_offer` | `{}` |
| `draw_accept` | `{}` |
| `draw_decline` | `{}` |

The local outgoing API accepts exactly `{m}` for `move` and `{}` for every
other action above. The implementation derives `n`, the terminal claim, the
winner, and the next turn. A receiver MUST independently replay the history
and reject a non-sequential ply, a non-canonical or illegal square, a move by
the participant who is not to move, a false or missing terminal claim, a
forged winner, or any move after completion. A winning move's `w` is the
identity that the rules declare the winner, which in Reversi need not be the
mover. Draws may also be negotiated with `draw_offer` and `draw_accept`; no
draw claims exist. `error` uses Section 9.

#### Canonical Local Metadata

The reference applications expose `board`, `moves`, `my_color`, `first_turn`,
`turn`, `move_count`, `last_move`, `legal_moves`, `winner`, `terminal`,
`terminal_reason`, `draw_offered`, and `draw_offered_by`. `board` holds N×N
ASCII cells row-major from the top-left square (`a` on rank N) to the
bottom-right square (the last file on rank 1): `_` empty, `b` Black, `w`
White. `my_color` is `"b"` or `"w"` and `first_turn` is the Black player.
`legal_moves` lists the moves available to the player to move in canonical
order (squares by ascending rank, then file; `swap` last) and is empty after
the game ends. `terminal_reason` is `rsn` for resignation, `agr` for a
negotiated draw, or the game's board reason below. Persistence hydration MUST
replay `moves` and revalidate the board, move count, last move, colors, turn,
terminal status, terminal reason, winner identity, and participant bindings
before accepting a stored session. When a session expires, any outstanding
draw offer is cleared.

### E.2 Hex (`hex.1`)

- The board is 11×11 (`a1`–`k11`). Cell `(file, rank)` touches the six cells
  at file±1 on the same rank, rank±1 on the same file, `(file+1, rank-1)`,
  and `(file-1, rank+1)`.
- Black wins by connecting rank 1 to rank 11 with a chain of Black stones;
  White wins by connecting file `a` to file `k`. A move onto an occupied cell
  is illegal. Hex cannot end in a draw, so `x="draw"` is never valid.
- **Swap rule.** As the second move of the game only (`n=1`), White may send
  `m="swap"`. Black's single stone is removed and a White stone is placed on
  the mirrored cell, the one with file and rank exchanged (`c7` becomes
  `g3`). Each player keeps its color and Black moves next, so turns still
  alternate.
- Board reason: `con` (connected).

### E.3 Gomoku (`gomoku.1`)

- The board is 15×15 (`a1`–`o15`). Freestyle rules: a stone that completes
  five or more consecutive stones of one color horizontally, vertically, or
  diagonally wins. A move onto an occupied square is illegal.
- Filling the last empty square without completing five draws the game.
- Board reasons: `5ir` (five in a row) and `ful` (full board).

### E.4 Reversi (`reversi.1`)

- The board is 8×8 (`a1`–`h8`). The game starts with White on `d4` and `e5`
  and Black on `d5` and `e4`.
- A move MUST bracket at least one straight line, in any of the eight
  directions, of one or more opposing discs between the new disc and another
  disc of the mover's color. Every bracketed disc is flipped.
- **Automatic passing.** After a move, if the opponent has a legal move it is
  the opponent's turn. Otherwise, if the mover still has a legal move, the
  mover moves again and no pass message is sent. Otherwise the game is over.
- When the game is over the side with more discs wins and equal counts draw.
  The winner need not be the player who made the last move, so a winning
  move's `w` is the identity with more discs.
- Additional local metadata: `black_count`, `white_count`, and
  `skipped_turn`, which is true when the last move left the opponent without
  a move.
- Board reason: `cnt` (disc count) for both wins and draws.

## F. Dots and Boxes and Mancala Reference Games

Dots and Boxes (`dots_and_boxes.1`) and Mancala (`mancala.1`) are built-in
two-player games in which every move is a single token and a move can earn
the mover another turn. They use the Appendix E.1 wire contract unchanged
(payload schema, canonical command payloads, the `b` coin flip, replay, and
hydration) except for the differences below. Both use session type
`turn_based` and validation `both`. This appendix defines LRGP reference
applications; it does not change the base LRGP protocol version.

### F.1 Differences from E.1

- **Move tokens.** `m` is a line (F.2) or a pit (F.3), not a square. Any
  other spelling is invalid.
- **Roles.** `b` (Black) is the first mover and `w` (White) the second;
  `my_color` and `first_turn` keep their E.1 meaning.
- **Extra turns.** After each move the rules decide who moves next. A move
  that earns another turn is an ordinary `{m, n, x}` move; no pass or
  continue message exists, and `n` still counts every move played. A
  receiver MUST reject a move from the participant the replayed position
  does not name as next to move.
- **Board.** `board` uses the encoding given for each game below.
- **Winner.** A winning move's `w` is the identity with more boxes or
  seeds, which need not be the mover.

### F.2 Dots and Boxes (`dots_and_boxes.1`)

- The grid is 5×5 boxes on 6×6 dots. Dots are named like squares on a 6×6
  board (`a1`–`f6`).
- A line is a dot followed by `h` for the line to the dot on its right or
  `v` for the line to the dot above it: `c3h` joins c3 and d3, `c3v` joins
  c3 and c4. Dots on file `f` have no `h` line and dots on rank 6 have no
  `v` line, so there are 60 lines. Drawing a line that is already drawn is
  illegal.
- A move that completes one or two boxes claims them for the mover, who
  then moves again. Otherwise the turn passes.
- The game ends when every box is claimed; the side with more boxes wins.
  With 25 boxes a draw is impossible, so `x="draw"` is never valid.
- `legal_moves` lists undrawn lines by dot (ascending rank, then file), `h`
  before `v` at the same dot.
- `board` is an 11×11 picture row-major from the top-left dot (`a6`): `+`
  for a dot, `-` or `|` for a drawn line, a space for an undrawn line, and
  `_`, `b`, or `w` for an unclaimed or claimed box.
- Additional local metadata: `black_boxes`, `white_boxes`, and
  `extra_turn`, which is true when the last move claimed a box.
- Board reason: `cnt` (box count).

### F.3 Mancala (`mancala.1`)

- Kalah rules with six pits per side and four seeds in each pit. Black's
  pits are `a`–`f` from Black's left to right, followed by Black's store;
  White's pits are `g`–`l` from White's left to right, followed by White's
  store. Pit `a` faces `l`, `b` faces `k`, and so on to `f` facing `g`.
- A move names one of the mover's own non-empty pits. Its seeds are lifted
  and sown one per hole in the order `a`…`f`, Black's store, `g`…`l`,
  White's store, and around again, skipping the opponent's store.
- If the last seed lands in the mover's store, the mover moves again. If it
  lands in an empty pit of the mover's own and the facing pit holds seeds,
  that seed and the facing pit's seeds go to the mover's store.
- After any move, if either player's six pits are all empty the game ends:
  each player adds the seeds left in their own pits to their own store.
  More seeds in the store wins and 24–24 draws. A game that ends this way
  ends even if the last move landed in the mover's store.
- `legal_moves` lists the mover's non-empty pits alphabetically.
- `board` is the fourteen hole counts in sowing order, comma-separated:
  `a`–`f`, Black's store, `g`–`l`, White's store. The opening board is
  `4,4,4,4,4,4,0,4,4,4,4,4,4,0`.
- Additional local metadata: `black_store`, `white_store`, and
  `extra_turn`, which is true when the last seed landed in the mover's
  store.
- Board reason: `cnt` (store count) for both wins and draws.

### F.4 Reference Games

The shared fixtures include one reference game per game, played by the
self-play policy `legal[(ply * 7 + seed) % len(legal)]` with the challenger
as Black: Dots and Boxes with seed 1 and Mancala with seed 4, both won by
White after Black's last move. A Mancala draw is the same policy with seed
3. Conforming implementations MUST reproduce every legal-move list and the
final move payload of each game.

## G. More Single-Token Reference Games

The games in this appendix are built-in two-player games that use the
Appendix E.1 wire contract unchanged (payload schema, canonical command
payloads, the `b` coin flip, replay, metadata, and hydration) except where a
game says otherwise. Black (`b`) is the first mover. All use session type
`turn_based` and validation `both`. This appendix defines LRGP reference
applications; it does not change the base LRGP protocol version.

### G.1 Ultimate Tic-Tac-Toe (`ultimate_ttt.1`)

- The board is 9×9 (`a1`–`i9`), split into nine 3×3 small boards. A
  square's small board is `(file / 3, rank / 3)` and its cell inside that
  board is `(file % 3, rank % 3)`, counting files and ranks from 0.
- The first move may be any square. After that, the cell a player takes
  names the small board the opponent must play in: a move on the top-right
  cell of any small board sends the opponent to the top-right small board.
  If that small board is finished, the opponent may play on any empty
  square of any unfinished small board.
- Three in a row of one color inside a small board wins that small board.
  A small board filled without a line is dead and counts for nobody. No
  move may be played in a finished small board.
- Three small boards won by one color in a row (on the 3×3 grid of small
  boards) wins the game. When every small board is finished without such a
  line, the game is drawn.
- `legal_moves` lists the open squares by ascending rank, then file.
- Additional local metadata: `boards`, nine characters for the small boards
  row-major from the top-left one: `_` open, `b` or `w` won, `d` dead.
- Board reasons: `3ib` (three boards in a row) and `ful` (every board
  finished).
- Reference games (Appendix F.4 policy): seed 4, won by White, and seed
  19, drawn after Black finishes the last small board.

### G.2 Go (`go.1`)

- The board is 9×9 (`a1`–`i9`; file `i` is used, unlike some Go
  coordinates). A move is an empty point or `pass`.
- A stone placed next to opposing chains removes every opposing chain left
  without a liberty (an empty orthogonal neighbor). A move that leaves its
  own chain without a liberty after those captures is suicide and illegal.
- **Positional superko.** A move MUST NOT recreate any board that occurred
  earlier in the game, including the empty starting board. Passing never
  breaks this rule.
- Two passes in a row end the game. There is no dead-stone agreement:
  every stone on the board counts as alive, so players capture dead stones
  before passing (under area scoring this costs nothing).
- **Area scoring.** Each side scores its stones plus the empty regions that
  border only its own stones. White adds 7.5 points of komi, so the game
  cannot be drawn and `x="draw"` is never valid. The final move is always
  the second pass, and a winning move's `w` is the identity with the higher
  score, which need not be the mover.
- `legal_moves` lists the legal points by ascending rank, then file, and
  `pass` last.
- Additional local metadata: `black_count` and `white_count` (the area
  score of the current board, without komi), `komi` (`7.5`), and
  `black_captures` and `white_captures` (stones each side has captured).
- Board reason: `cnt` (area count).
- Reference game (Appendix F.4 policy): seed 12. Black passes last and
  holds more area (42 to 39), but White wins on komi.
