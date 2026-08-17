pub mod app_base;
pub mod apps;
pub mod constants;
pub mod dedup;
pub mod envelope;
pub mod errors;
pub mod router;
pub mod session;
pub mod store;
pub mod transport;

/// Canonical provisional LRGP envelope and LXMF embedding API.
///
/// These are exact re-exports of existing identities. Router, application,
/// session, replay-cache, built-in, and persistence ownership intentionally
/// remain module-qualified provisional APIs.
pub mod protocol {
    pub use crate::constants::{
        CMD_ACCEPT, CMD_CHALLENGE, CMD_DECLINE, CMD_DRAW_ACCEPT, CMD_DRAW_DECLINE, CMD_DRAW_OFFER,
        CMD_ERROR, CMD_MOVE, CMD_RESIGN, ENVELOPE_MAX_PACKED, FIELD_CUSTOM_META, FIELD_CUSTOM_TYPE,
        KEY_APP, KEY_COMMAND, KEY_NONCE, KEY_PAYLOAD, KEY_SESSION, NONCE_BYTES, PROTOCOL_TYPE,
    };
    pub use crate::envelope::{
        Envelope, ValidatedEnvelope, Value, generate_nonce, generate_session_id, is_valid_app_id,
        is_valid_command, is_valid_session_id, pack_envelope, pack_lxmf_fields, pack_to_bytes,
        parse_app_version, unpack_envelope, unpack_from_bytes, validate_envelope,
        validate_envelope_size, value_as_bool, value_as_i64, value_as_str, value_as_u64,
    };
    pub use crate::errors::LrgpError;
    pub use crate::transport::{extract_envelope, is_lrgp_message, pack_into_preencoded_fields};
}
