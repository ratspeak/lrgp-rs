/// LXMF field IDs.
pub const FIELD_CUSTOM_TYPE: u8 = 0xFB; // 251
pub const FIELD_CUSTOM_META: u8 = 0xFD; // 253
pub const FIELD_FILE_ATTACHMENTS: u8 = 0x05;

/// Protocol marker stored in FIELD_CUSTOM_TYPE.
pub const PROTOCOL_TYPE: &str = "lrgp.v1";

/// Size limits (bytes).
pub const ENVELOPE_MAX_PACKED: usize = 200;
pub const OPPORTUNISTIC_MAX_CONTENT: usize = 295;
pub const LINK_PACKET_MAX_CONTENT: usize = 319;
/// 16B dest + 16B src + 64B sig + 8B ts + 8B structure.
pub const LXMF_OVERHEAD: usize = 112;

/// Session statuses.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_EXPIRED: &str = "expired";
pub const STATUS_DECLINED: &str = "declined";

/// Game session types.
pub const SESSION_TURN_BASED: &str = "turn_based";
pub const SESSION_REAL_TIME: &str = "real_time";
pub const SESSION_ROUND_BASED: &str = "round_based";
pub const SESSION_SINGLE_ROUND: &str = "single_round";

/// Validation models.
pub const VALIDATION_SENDER: &str = "sender";
pub const VALIDATION_RECEIVER: &str = "receiver";
pub const VALIDATION_BOTH: &str = "both";

/// Standard commands.
pub const CMD_CHALLENGE: &str = "challenge";
pub const CMD_ACCEPT: &str = "accept";
pub const CMD_DECLINE: &str = "decline";
pub const CMD_MOVE: &str = "move";
pub const CMD_RESIGN: &str = "resign";
pub const CMD_DRAW_OFFER: &str = "draw_offer";
pub const CMD_DRAW_ACCEPT: &str = "draw_accept";
pub const CMD_DRAW_DECLINE: &str = "draw_decline";
pub const CMD_ERROR: &str = "error";

/// Standard error codes.
pub const ERR_UNSUPPORTED_APP: &str = "unsupported_app";
pub const ERR_INVALID_MOVE: &str = "invalid_move";
pub const ERR_NOT_YOUR_TURN: &str = "not_your_turn";
pub const ERR_SESSION_EXPIRED: &str = "session_expired";
pub const ERR_PROTOCOL_ERROR: &str = "protocol_error";

/// Session TTL defaults (seconds).
pub const TTL_PENDING: f64 = 86400.0; // 24 hours
pub const TTL_ACTIVE: f64 = 604800.0; // 7 days
pub const TTL_GRACE_PERIOD: f64 = 3600.0; // 1 hour clock-skew tolerance

/// Envelope keys (single-char for wire efficiency).
pub const KEY_APP: &str = "a";
pub const KEY_COMMAND: &str = "c";
pub const KEY_SESSION: &str = "s";
pub const KEY_PAYLOAD: &str = "p";
/// Optional 8-byte per-envelope replay-dedup nonce.
pub const KEY_NONCE: &str = "n";

/// Nonce / replay-dedup sizing.
pub const NONCE_BYTES: usize = 8;
pub const DEDUP_CACHE_PER_SESSION: usize = 512;
pub const DEDUP_TTL_SECONDS: u64 = 600;

/// Error payload keys.
pub const KEY_ERR_CODE: &str = "code";
pub const KEY_ERR_MSG: &str = "msg";
pub const KEY_ERR_REF: &str = "ref";
