//! LRGP transport bridge — converts between LRGP envelopes and LXMF field bytes.
//!
//! This module handles the raw byte-level conversion needed to embed LRGP
//! game envelopes inside LXMF messages and extract them on receipt.
//! It is pure data transformation — no I/O.
//!
//! # Important: native MessagePack fields
//!
//! The returned byte values each encode **one complete native MessagePack
//! value**. With `lxmf-core::LxMessage`, install them using
//! `set_msgpack_field`, never `set_field`. `set_field` represents its input as
//! a MessagePack binary value, which would put `bin("lrgp.v1")` and
//! `bin(<encoded map>)` on the wire instead of the LRGP string and map. Python
//! LRGP implementations correctly reject/ignore those binary wrappers.

use std::collections::HashMap;

use crate::constants::*;
use crate::envelope::{self, Envelope};
use crate::errors::LrgpError;

fn decode_one(field: &str, data: &[u8]) -> Result<rmpv::Value, LrgpError> {
    let mut cursor = std::io::Cursor::new(data);
    let value = rmpv::decode::read_value(&mut cursor)
        .map_err(|e| LrgpError::InvalidEnvelope(format!("{field} decode error: {e}")))?;
    if cursor.position() != data.len() as u64 {
        return Err(LrgpError::InvalidEnvelope(format!(
            "{field} contains trailing bytes"
        )));
    }
    Ok(value)
}

/// Check whether an LXMF fields dict contains an LRGP game message.
pub fn is_lrgp_message(fields: &HashMap<u8, Vec<u8>>) -> bool {
    match fields.get(&FIELD_CUSTOM_TYPE) {
        Some(data) => decode_one("type field", data)
            .ok()
            .and_then(|value| envelope::value_as_str(&value).map(str::to_owned))
            .is_some_and(|marker| marker == PROTOCOL_TYPE),
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
    let type_val = decode_one("type field", type_data)?;
    let marker = envelope::value_as_str(&type_val).unwrap_or("");
    if marker != PROTOCOL_TYPE {
        return Ok(None);
    }

    // 2. Decode meta field
    let meta_data = fields
        .get(&FIELD_CUSTOM_META)
        .ok_or_else(|| LrgpError::InvalidEnvelope("FIELD_CUSTOM_META (0xFD) missing".into()))?;

    let meta_val = decode_one("meta field", meta_data)?;

    // 3. Convert to HashMap envelope
    let env = envelope::map_from_value(&meta_val)
        .ok_or_else(|| LrgpError::InvalidEnvelope("meta field is not a map".into()))?;

    envelope::validate_envelope(&env)?;

    Ok(Some(env))
}

/// Pack an LRGP envelope into pre-encoded native LXMF field values.
///
/// Returns `HashMap<u8, Vec<u8>>` containing:
///   - `0xFB` → msgpack("lrgp.v1")
///   - `0xFD` → msgpack(envelope dict)
///
/// Each byte vector MUST be installed as a pre-encoded native MessagePack
/// field (for `lxmf-core`, call `LxMessage::set_msgpack_field`). Passing these
/// bytes to `LxMessage::set_field` creates non-interoperable binary wrappers.
///
/// Always uses the current protocol marker (`lrgp.v1`) for outbound messages.
pub fn pack_into_preencoded_fields(envelope: &Envelope) -> Result<HashMap<u8, Vec<u8>>, LrgpError> {
    let mut fields = HashMap::new();
    for (field_id, value) in envelope::pack_lxmf_fields(envelope)? {
        let mut encoded = Vec::new();
        rmpv::encode::write_value(&mut encoded, &value).map_err(|error| {
            LrgpError::InvalidEnvelope(format!("field {field_id:#x} encode error: {error}"))
        })?;
        fields.insert(field_id, encoded);
    }

    Ok(fields)
}

/// Deprecated ambiguous name for [`pack_into_preencoded_fields`].
///
/// The output is not arbitrary field data: each value is already encoded as
/// one native MessagePack object and must be passed to a MessagePack-aware
/// field setter such as `LxMessage::set_msgpack_field`.
#[deprecated(
    since = "0.4.0",
    note = "use pack_into_preencoded_fields and LxMessage::set_msgpack_field"
)]
pub fn pack_into_fields(envelope: &Envelope) -> Result<HashMap<u8, Vec<u8>>, LrgpError> {
    pack_into_preencoded_fields(envelope)
}

/// Convert raw LXMF field bytes into typed rmpv values (for use with envelope::unpack_envelope).
pub fn fields_bytes_to_rmpv(
    fields: &HashMap<u8, Vec<u8>>,
) -> Result<HashMap<u8, rmpv::Value>, LrgpError> {
    let mut result = HashMap::new();
    for (&key, data) in fields {
        let val = decode_one(&format!("field {key:#x}"), data)?;
        result.insert(key, val);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pack_and_extract_roundtrip() {
        let env =
            envelope::pack_envelope("ttt", 1, "challenge", "abcdef0123456789", None, None).unwrap();
        let raw_fields = pack_into_preencoded_fields(&env).unwrap();
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
        let env =
            envelope::pack_envelope("ttt", 1, "move", "abcdef0123456789", None, None).unwrap();
        let raw_fields = pack_into_preencoded_fields(&env).unwrap();
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
        let env =
            envelope::pack_envelope("ttt", 1, "move", "abcdef0123456789", None, None).unwrap();
        let raw = pack_into_preencoded_fields(&env).unwrap();
        let rmpv_fields = fields_bytes_to_rmpv(&raw).unwrap();
        assert!(rmpv_fields.contains_key(&FIELD_CUSTOM_TYPE));
        assert!(rmpv_fields.contains_key(&FIELD_CUSTOM_META));
    }

    #[test]
    fn native_field_reencoding_is_python_interoperable() {
        let env = envelope::pack_envelope(
            "ttt",
            1,
            "move",
            "abcdef0123456789",
            Some(HashMap::from([("i".into(), rmpv::Value::from(4))])),
            None,
        )
        .unwrap();
        let typed = envelope::pack_lxmf_fields(&env).unwrap();
        let mut native_wire_values = HashMap::new();
        for (field_id, value) in typed {
            let mut encoded = Vec::new();
            rmpv::encode::write_value(&mut encoded, &value).unwrap();
            native_wire_values.insert(field_id, encoded);
        }

        assert_eq!(extract_envelope(&native_wire_values).unwrap(), Some(env));
    }

    #[test]
    fn binary_wrapped_fields_are_not_lrgp() {
        let env =
            envelope::pack_envelope("ttt", 1, "move", "abcdef0123456789", None, None).unwrap();
        let native = pack_into_preencoded_fields(&env).unwrap();
        let wrapped = native
            .into_iter()
            .map(|(field_id, encoded_native)| {
                let mut encoded_binary = Vec::new();
                rmpv::encode::write_value(
                    &mut encoded_binary,
                    &rmpv::Value::Binary(encoded_native),
                )
                .unwrap();
                (field_id, encoded_binary)
            })
            .collect();

        assert!(!is_lrgp_message(&wrapped));
        assert!(extract_envelope(&wrapped).unwrap().is_none());
    }

    #[test]
    fn raw_field_decoders_reject_trailing_bytes() {
        let env =
            envelope::pack_envelope("ttt", 1, "move", "abcdef0123456789", None, None).unwrap();
        let mut trailing_type = pack_into_preencoded_fields(&env).unwrap();
        trailing_type
            .get_mut(&FIELD_CUSTOM_TYPE)
            .unwrap()
            .push(0xc0);
        assert!(!is_lrgp_message(&trailing_type));
        assert!(extract_envelope(&trailing_type).is_err());
        assert!(fields_bytes_to_rmpv(&trailing_type).is_err());

        let mut trailing_meta = pack_into_preencoded_fields(&env).unwrap();
        trailing_meta
            .get_mut(&FIELD_CUSTOM_META)
            .unwrap()
            .push(0xc0);
        assert!(extract_envelope(&trailing_meta).is_err());
        assert!(fields_bytes_to_rmpv(&trailing_meta).is_err());
    }
}
