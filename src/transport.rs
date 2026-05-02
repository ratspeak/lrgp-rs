//! LRGP transport bridge — converts between LRGP envelopes and LXMF field bytes.
//!
//! This module handles the raw byte-level conversion needed to embed LRGP
//! game envelopes inside LXMF messages and extract them on receipt.
//! It is pure data transformation — no I/O.

use std::collections::HashMap;

use crate::constants::*;
use crate::envelope::{self, Envelope};
use crate::errors::LrgpError;

/// Check whether an LXMF fields dict contains an LRGP game message.
pub fn is_lrgp_message(fields: &HashMap<u8, Vec<u8>>) -> bool {
    match fields.get(&FIELD_CUSTOM_TYPE) {
        Some(data) => {
            if let Ok(val) = rmpv::decode::read_value(&mut &data[..]) {
                if let Some(s) = envelope::value_as_str(&val) {
                    return s == PROTOCOL_TYPE;
                }
            }
            false
        }
        None => false,
    }
}

/// Extract an LRGP envelope from raw LXMF field bytes.
///
/// Steps:
///   1. Check `fields[0xFB]` for the `lrgp.v1` protocol marker.
///   2. Decode `fields[0xFD]` from msgpack bytes into an rmpv::Value.
///   3. Convert that value into a `HashMap<String, Value>` envelope.
///
/// Returns `Ok(None)` if the message is not an LRGP message.
pub fn extract_envelope(fields: &HashMap<u8, Vec<u8>>) -> Result<Option<Envelope>, LrgpError> {
    let type_data = match fields.get(&FIELD_CUSTOM_TYPE) {
        Some(d) => d,
        None => return Ok(None),
    };
    let type_val = rmpv::decode::read_value(&mut &type_data[..])
        .map_err(|e| LrgpError::InvalidEnvelope(format!("type field decode error: {e}")))?;
    let marker = envelope::value_as_str(&type_val).unwrap_or("");
    if marker != PROTOCOL_TYPE {
        return Ok(None);
    }

    // 2. Decode meta field
    let meta_data = fields
        .get(&FIELD_CUSTOM_META)
        .ok_or_else(|| LrgpError::InvalidEnvelope("FIELD_CUSTOM_META (0xFD) missing".into()))?;

    let meta_val = rmpv::decode::read_value(&mut &meta_data[..])
        .map_err(|e| LrgpError::InvalidEnvelope(format!("meta field decode error: {e}")))?;

    // 3. Convert to HashMap envelope
    let env = envelope::map_from_value(&meta_val)
        .ok_or_else(|| LrgpError::InvalidEnvelope("meta field is not a map".into()))?;

    for key in &[KEY_APP, KEY_COMMAND, KEY_SESSION, KEY_PAYLOAD, KEY_NONCE] {
        if !env.contains_key(*key) {
            return Err(LrgpError::InvalidEnvelope(format!(
                "Missing required key: {key}"
            )));
        }
    }

    Ok(Some(env))
}

/// Pack an LRGP envelope into raw LXMF field bytes.
///
/// Returns `HashMap<u8, Vec<u8>>` ready to pass to lxmf message construction:
///   - `0xFB` → msgpack("lrgp.v1")
///   - `0xFD` → msgpack(envelope dict)
///
/// Always uses the current protocol marker (`lrgp.v1`) for outbound messages.
pub fn pack_into_fields(envelope: &Envelope) -> Result<HashMap<u8, Vec<u8>>, LrgpError> {
    let mut fields = HashMap::new();

    // Type marker → always lrgp.v1
    let type_val = rmpv::Value::String(PROTOCOL_TYPE.into());
    let mut type_buf = Vec::new();
    rmpv::encode::write_value(&mut type_buf, &type_val)
        .map_err(|e| LrgpError::InvalidEnvelope(format!("type encode error: {e}")))?;
    fields.insert(FIELD_CUSTOM_TYPE, type_buf);

    // Envelope dict
    let env_val = envelope::value_from_map(envelope.clone());
    let mut env_buf = Vec::new();
    rmpv::encode::write_value(&mut env_buf, &env_val)
        .map_err(|e| LrgpError::InvalidEnvelope(format!("envelope encode error: {e}")))?;
    fields.insert(FIELD_CUSTOM_META, env_buf);

    Ok(fields)
}

/// Convert raw LXMF field bytes into typed rmpv values (for use with envelope::unpack_envelope).
pub fn fields_bytes_to_rmpv(
    fields: &HashMap<u8, Vec<u8>>,
) -> Result<HashMap<u8, rmpv::Value>, LrgpError> {
    let mut result = HashMap::new();
    for (&key, data) in fields {
        let val = rmpv::decode::read_value(&mut &data[..])
            .map_err(|e| LrgpError::InvalidEnvelope(format!("field {key:#x} decode error: {e}")))?;
        result.insert(key, val);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pack_and_extract_roundtrip() {
        let env = envelope::pack_envelope("ttt", 1, "challenge", "abcdef0123456789", None, None);
        let raw_fields = pack_into_fields(&env).unwrap();
        let recovered = extract_envelope(&raw_fields).unwrap().unwrap();

        assert_eq!(
            envelope::value_as_str(recovered.get(KEY_APP).unwrap()).unwrap(),
            "ttt.1"
        );
        assert_eq!(
            envelope::value_as_str(recovered.get(KEY_COMMAND).unwrap()).unwrap(),
            "challenge"
        );
    }

    #[test]
    fn test_is_lrgp_message_true() {
        let env = envelope::pack_envelope("ttt", 1, "move", "abc", None, None);
        let raw_fields = pack_into_fields(&env).unwrap();
        assert!(is_lrgp_message(&raw_fields));
    }

    #[test]
    fn test_is_lrgp_message_false() {
        let fields: HashMap<u8, Vec<u8>> = HashMap::new();
        assert!(!is_lrgp_message(&fields));
    }

    #[test]
    fn test_extract_envelope_not_lrgp() {
        let fields: HashMap<u8, Vec<u8>> = HashMap::new();
        assert!(extract_envelope(&fields).unwrap().is_none());
    }

    #[test]
    fn test_fields_bytes_to_rmpv() {
        let env = envelope::pack_envelope("ttt", 1, "move", "abc", None, None);
        let raw = pack_into_fields(&env).unwrap();
        let rmpv_fields = fields_bytes_to_rmpv(&raw).unwrap();
        assert!(rmpv_fields.contains_key(&FIELD_CUSTOM_TYPE));
        assert!(rmpv_fields.contains_key(&FIELD_CUSTOM_META));
    }
}
