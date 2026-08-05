//! LRGP envelope packing, unpacking, and validation.

use std::collections::HashMap;

use crate::constants::*;
use crate::errors::LrgpError;

/// An LRGP envelope — the top-level dict stored in LXMF field 0xFD.
pub type Envelope = HashMap<String, rmpv::Value>;

/// Canonically validated envelope fields.
///
/// Construct this only through [`validate_envelope`]. Keeping validation in a
/// single entry point prevents the transport and router from interpreting the
/// same wire map differently.
#[derive(Debug, Clone)]
pub struct ValidatedEnvelope {
    pub app_id: String,
    pub version: u32,
    pub command: String,
    pub session_id: String,
    pub payload: HashMap<String, rmpv::Value>,
    pub nonce: [u8; NONCE_BYTES],
}

/// Convenience re-export of rmpv::Value for payload manipulation.
pub use rmpv::Value;

/// Generate a fresh 8-byte random nonce using the platform CSPRNG.
pub fn generate_nonce() -> [u8; NONCE_BYTES] {
    use rand::RngCore;
    let mut n = [0u8; NONCE_BYTES];
    rand::thread_rng().fill_bytes(&mut n);
    n
}

/// Generate a canonical 16-character lowercase hexadecimal session ID.
pub fn generate_session_id() -> String {
    use rand::RngCore;
    let mut id = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut id);
    hex::encode(id)
}

/// Build an LRGP envelope dict. If `nonce` is `None` a fresh CSPRNG nonce is
/// generated; pass `Some(..)` to build deterministic test vectors.
pub fn pack_envelope(
    app_id: &str,
    version: u32,
    command: &str,
    session_id: &str,
    payload: Option<HashMap<String, rmpv::Value>>,
    nonce: Option<[u8; NONCE_BYTES]>,
) -> Result<Envelope, LrgpError> {
    let env = build_envelope(app_id, version, command, session_id, payload, nonce);
    validate_envelope(&env)?;
    Ok(env)
}

fn build_envelope(
    app_id: &str,
    version: u32,
    command: &str,
    session_id: &str,
    payload: Option<HashMap<String, rmpv::Value>>,
    nonce: Option<[u8; NONCE_BYTES]>,
) -> Envelope {
    let mut env = Envelope::new();
    env.insert(
        KEY_APP.into(),
        rmpv::Value::String(format!("{app_id}.{version}").into()),
    );
    env.insert(KEY_COMMAND.into(), rmpv::Value::String(command.into()));
    env.insert(KEY_SESSION.into(), rmpv::Value::String(session_id.into()));
    env.insert(
        KEY_PAYLOAD.into(),
        match payload {
            Some(p) => value_from_map(p),
            None => rmpv::Value::Map(vec![]),
        },
    );
    let n = nonce.unwrap_or_else(generate_nonce);
    env.insert(KEY_NONCE.into(), rmpv::Value::Binary(n.to_vec()));
    env
}

/// Validate that the packed envelope fits within ENVELOPE_MAX_PACKED.
/// Returns the packed size in bytes.
pub fn validate_envelope_size(envelope: &Envelope) -> Result<usize, LrgpError> {
    let packed = encode_envelope_map(envelope)?;
    let size = packed.len();
    if size > ENVELOPE_MAX_PACKED {
        return Err(LrgpError::EnvelopeTooLarge(size, ENVELOPE_MAX_PACKED));
    }
    Ok(size)
}

/// Validate every protocol-level envelope invariant shared by all LRGP apps.
///
/// App registration, supported version, and supported action checks belong to
/// the router because they depend on its live app registry.
pub fn validate_envelope(envelope: &Envelope) -> Result<ValidatedEnvelope, LrgpError> {
    let required = [KEY_APP, KEY_COMMAND, KEY_SESSION, KEY_PAYLOAD, KEY_NONCE];
    if envelope.len() != required.len()
        || !envelope.keys().all(|key| required.contains(&key.as_str()))
    {
        return Err(LrgpError::InvalidEnvelope(
            "envelope must contain exactly the keys a, c, s, p, and n".into(),
        ));
    }
    for key in &required {
        if !envelope.contains_key(*key) {
            return Err(LrgpError::InvalidEnvelope(format!(
                "Missing envelope key: {key}"
            )));
        }
    }

    validate_envelope_size(envelope)?;

    let app_ver = envelope
        .get(KEY_APP)
        .and_then(value_as_str)
        .ok_or_else(|| LrgpError::InvalidEnvelope("KEY_APP must be a string".into()))?;
    let (app_id, version) = parse_app_version(app_ver)
        .ok_or_else(|| LrgpError::InvalidEnvelope("invalid app.version format".into()))?;
    if !is_valid_app_id(app_id) {
        return Err(LrgpError::InvalidEnvelope(
            "app id must match [a-z][a-z0-9_.-]*".into(),
        ));
    }

    let command = envelope
        .get(KEY_COMMAND)
        .and_then(value_as_str)
        .ok_or_else(|| LrgpError::InvalidEnvelope("KEY_COMMAND must be a string".into()))?;
    if !is_valid_command(command) {
        return Err(LrgpError::InvalidEnvelope(
            "command must match [a-z][a-z0-9_]*".into(),
        ));
    }

    let session_id = envelope
        .get(KEY_SESSION)
        .and_then(value_as_str)
        .ok_or_else(|| LrgpError::InvalidEnvelope("KEY_SESSION must be a string".into()))?;
    if !is_valid_session_id(session_id) {
        return Err(LrgpError::InvalidEnvelope(
            "session id must be exactly 16 lowercase hexadecimal characters".into(),
        ));
    }

    let payload = envelope
        .get(KEY_PAYLOAD)
        .and_then(map_from_value)
        .ok_or_else(|| LrgpError::InvalidEnvelope("KEY_PAYLOAD must be a map".into()))?;

    let nonce = match envelope.get(KEY_NONCE) {
        Some(rmpv::Value::Binary(bytes)) if bytes.len() == NONCE_BYTES => {
            let mut nonce = [0u8; NONCE_BYTES];
            nonce.copy_from_slice(bytes);
            nonce
        }
        _ => {
            return Err(LrgpError::InvalidEnvelope(format!(
                "KEY_NONCE must be {NONCE_BYTES}-byte binary"
            )));
        }
    };

    Ok(ValidatedEnvelope {
        app_id: app_id.to_string(),
        version,
        command: command.to_string(),
        session_id: session_id.to_string(),
        payload,
        nonce,
    })
}

/// Return true only for the canonical 8-byte lowercase-hex session encoding.
pub fn is_valid_session_id(session_id: &str) -> bool {
    session_id.len() == 16
        && session_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Return true for the canonical game identifier grammar.
pub fn is_valid_app_id(app_id: &str) -> bool {
    let mut bytes = app_id.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'-')
        })
}

/// Return true for the canonical command grammar.
pub fn is_valid_command(command: &str) -> bool {
    let mut bytes = command.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Return LXMF fields dict ready for inclusion in an LxMessage.
/// Returns `{0xFB: "lrgp.v1", 0xFD: envelope}` as a HashMap<u8, ...>.
pub fn pack_lxmf_fields(envelope: &Envelope) -> Result<HashMap<u8, rmpv::Value>, LrgpError> {
    validate_envelope(envelope)?;
    let mut fields = HashMap::new();
    fields.insert(FIELD_CUSTOM_TYPE, rmpv::Value::String(PROTOCOL_TYPE.into()));
    fields.insert(FIELD_CUSTOM_META, value_from_map(envelope.clone()));
    Ok(fields)
}

/// Extract and validate an LRGP envelope from LXMF fields.
/// Returns `None` if this is not an LRGP message.
pub fn unpack_envelope(fields: &HashMap<u8, rmpv::Value>) -> Result<Option<Envelope>, LrgpError> {
    let custom_type = fields.get(&FIELD_CUSTOM_TYPE);
    let is_lrgp = match custom_type {
        Some(rmpv::Value::String(s)) => s.as_str() == Some(PROTOCOL_TYPE),
        _ => false,
    };
    if !is_lrgp {
        return Ok(None);
    }

    let meta = fields
        .get(&FIELD_CUSTOM_META)
        .ok_or_else(|| LrgpError::InvalidEnvelope("FIELD_CUSTOM_META is missing".into()))?;

    let envelope = map_from_value(meta)
        .ok_or_else(|| LrgpError::InvalidEnvelope("FIELD_CUSTOM_META is not a map".into()))?;

    validate_envelope(&envelope)?;

    Ok(Some(envelope))
}

/// Split "app_id.version" into (app_id, version).
pub fn parse_app_version(app_ver_string: &str) -> Option<(&str, u32)> {
    let dot = app_ver_string.rfind('.')?;
    let app_id = &app_ver_string[..dot];
    let raw_version = &app_ver_string[dot + 1..];
    if raw_version.is_empty() || !raw_version.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let version: u32 = raw_version.parse().ok()?;
    if version == 0 || version.to_string() != raw_version {
        return None;
    }
    Some((app_id, version))
}

// --- Helpers for rmpv::Value ↔ HashMap conversion ---

/// Convert a HashMap<String, Value> into an rmpv::Value::Map.
pub fn value_from_map(map: HashMap<String, rmpv::Value>) -> rmpv::Value {
    let pairs: Vec<(rmpv::Value, rmpv::Value)> = map
        .into_iter()
        .map(|(k, v)| (rmpv::Value::String(k.into()), v))
        .collect();
    rmpv::Value::Map(pairs)
}

/// Try to convert an rmpv::Value::Map into a HashMap<String, Value>.
pub fn map_from_value(value: &rmpv::Value) -> Option<HashMap<String, rmpv::Value>> {
    match value {
        rmpv::Value::Map(pairs) => {
            let mut map = HashMap::new();
            for (k, v) in pairs {
                let key = match k {
                    rmpv::Value::String(s) => s.as_str()?.to_string(),
                    _ => return None,
                };
                if map.insert(key, v.clone()).is_some() {
                    // Duplicate msgpack map keys are ambiguous and would be
                    // silently collapsed by HashMap/dict decoders. Reject
                    // them instead of allowing sender/receiver disagreement.
                    return None;
                }
            }
            Some(map)
        }
        _ => None,
    }
}

/// Return true when a payload contains exactly the expected keys.
pub(crate) fn has_exact_keys(payload: &HashMap<String, rmpv::Value>, expected: &[&str]) -> bool {
    payload.len() == expected.len() && payload.keys().all(|key| expected.contains(&key.as_str()))
}

/// Serialize an Envelope to msgpack bytes using rmpv.
pub fn pack_to_bytes(envelope: &Envelope) -> Result<Vec<u8>, LrgpError> {
    validate_envelope(envelope)?;
    encode_envelope_map(envelope)
}

fn encode_envelope_map(envelope: &Envelope) -> Result<Vec<u8>, LrgpError> {
    let value = value_from_map(envelope.clone());
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &value)
        .map_err(|e| LrgpError::InvalidEnvelope(format!("msgpack encode error: {e}")))?;
    Ok(buf)
}

/// Deserialize msgpack bytes into an Envelope.
pub fn unpack_from_bytes(data: &[u8]) -> Result<Envelope, LrgpError> {
    let envelope = decode_envelope_map(data)?;
    validate_envelope(&envelope)?;
    Ok(envelope)
}

fn decode_envelope_map(data: &[u8]) -> Result<Envelope, LrgpError> {
    let mut cursor = std::io::Cursor::new(data);
    let value = rmpv::decode::read_value(&mut cursor)
        .map_err(|e| LrgpError::InvalidEnvelope(format!("msgpack decode error: {e}")))?;
    if cursor.position() != data.len() as u64 {
        return Err(LrgpError::InvalidEnvelope(
            "trailing bytes after msgpack envelope".into(),
        ));
    }
    map_from_value(&value)
        .ok_or_else(|| LrgpError::InvalidEnvelope("top-level value is not a map".into()))
}

/// Helper: get a string from an rmpv::Value.
pub fn value_as_str(v: &rmpv::Value) -> Option<&str> {
    match v {
        rmpv::Value::String(s) => s.as_str(),
        _ => None,
    }
}

/// Helper: get a u64 from an rmpv::Value.
pub fn value_as_u64(v: &rmpv::Value) -> Option<u64> {
    match v {
        rmpv::Value::Integer(i) => i.as_u64(),
        _ => None,
    }
}

/// Helper: get an i64 from an rmpv::Value.
pub fn value_as_i64(v: &rmpv::Value) -> Option<i64> {
    match v {
        rmpv::Value::Integer(i) => i.as_i64(),
        _ => None,
    }
}

/// Helper: get a bool from an rmpv::Value.
pub fn value_as_bool(v: &rmpv::Value) -> Option<bool> {
    match v {
        rmpv::Value::Boolean(b) => Some(*b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pack_unpack_roundtrip() {
        let mut payload = HashMap::new();
        payload.insert("i".to_string(), rmpv::Value::Integer(4.into()));
        payload.insert("b".to_string(), rmpv::Value::String("____X____".into()));

        let env = pack_envelope("ttt", 1, "move", "a1b2c3d4e5f60718", Some(payload), None).unwrap();

        let bytes = pack_to_bytes(&env).unwrap();
        let recovered = unpack_from_bytes(&bytes).unwrap();

        assert_eq!(
            value_as_str(recovered.get(KEY_APP).unwrap()).unwrap(),
            "ttt.1"
        );
        assert_eq!(
            value_as_str(recovered.get(KEY_COMMAND).unwrap()).unwrap(),
            "move"
        );
        assert_eq!(
            value_as_str(recovered.get(KEY_SESSION).unwrap()).unwrap(),
            "a1b2c3d4e5f60718"
        );
    }

    #[test]
    fn test_validate_envelope_size_ok() {
        let env = pack_envelope("ttt", 1, "challenge", "a1b2c3d4e5f60718", None, None).unwrap();
        let size = validate_envelope_size(&env).unwrap();
        assert!(size <= ENVELOPE_MAX_PACKED);
    }

    #[test]
    fn test_validate_envelope_size_too_large() {
        let mut payload = HashMap::new();
        // Create a huge payload to exceed the limit
        let big_string = "x".repeat(300);
        payload.insert("data".to_string(), rmpv::Value::String(big_string.into()));

        assert!(matches!(
            pack_envelope("ttt", 1, "move", "a1b2c3d4e5f60718", Some(payload), None),
            Err(LrgpError::EnvelopeTooLarge(_, _))
        ));
    }

    #[test]
    fn test_parse_app_version() {
        let (app, ver) = parse_app_version("ttt.1").unwrap();
        assert_eq!(app, "ttt");
        assert_eq!(ver, 1);

        let (app, ver) = parse_app_version("chess.game.2").unwrap();
        assert_eq!(app, "chess.game");
        assert_eq!(ver, 2);

        for invalid in ["ttt.0", "ttt.01", "ttt.+1", "ttt.-1", "ttt."] {
            assert!(parse_app_version(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn test_unpack_envelope_not_lrgp() {
        let fields = HashMap::new();
        assert!(unpack_envelope(&fields).unwrap().is_none());
    }

    #[test]
    fn test_unpack_envelope_valid() {
        let env = pack_envelope("ttt", 1, "challenge", "0123456789abcdef", None, None).unwrap();
        let lxmf_fields = pack_lxmf_fields(&env).unwrap();
        let result = unpack_envelope(&lxmf_fields).unwrap().unwrap();
        assert_eq!(
            value_as_str(result.get(KEY_COMMAND).unwrap()).unwrap(),
            "challenge"
        );
    }

    #[test]
    fn test_unpack_envelope_missing_key() {
        let mut lxmf = HashMap::new();
        lxmf.insert(FIELD_CUSTOM_TYPE, rmpv::Value::String(PROTOCOL_TYPE.into()));
        // FIELD_CUSTOM_META has a map missing required keys
        let bad_map = rmpv::Value::Map(vec![(
            rmpv::Value::String("a".into()),
            rmpv::Value::String("ttt.1".into()),
        )]);
        lxmf.insert(FIELD_CUSTOM_META, bad_map);
        assert!(unpack_envelope(&lxmf).is_err());
    }

    #[test]
    fn canonical_validation_rejects_non_hex_or_uppercase_session_ids() {
        for invalid in [
            "abc",
            "a1b2c3d4e5f6g7h8",
            "A1B2C3D4E5F60718",
            "a1b2c3d4e5f607189",
        ] {
            let mut envelope =
                pack_envelope("ttt", 1, "move", "a1b2c3d4e5f60718", None, None).unwrap();
            envelope.insert(KEY_SESSION.into(), rmpv::Value::String(invalid.into()));
            assert!(matches!(
                validate_envelope(&envelope),
                Err(LrgpError::InvalidEnvelope(_))
            ));
        }
    }

    #[test]
    fn canonical_validation_rejects_non_map_payload() {
        let mut envelope = pack_envelope("ttt", 1, "move", "a1b2c3d4e5f60718", None, None).unwrap();
        envelope.insert(KEY_PAYLOAD.into(), rmpv::Value::Array(Vec::new()));
        assert!(matches!(
            validate_envelope(&envelope),
            Err(LrgpError::InvalidEnvelope(_))
        ));
    }

    #[test]
    fn canonical_validation_rejects_missing_or_malformed_nonce() {
        let mut missing = pack_envelope("ttt", 1, "move", "a1b2c3d4e5f60718", None, None).unwrap();
        missing.remove(KEY_NONCE);
        assert!(validate_envelope(&missing).is_err());

        let mut malformed =
            pack_envelope("ttt", 1, "move", "a1b2c3d4e5f60718", None, None).unwrap();
        malformed.insert(KEY_NONCE.into(), rmpv::Value::Binary(vec![0; 7]));
        assert!(validate_envelope(&malformed).is_err());
    }

    #[test]
    fn canonical_validation_enforces_size_limit() {
        let mut payload = HashMap::new();
        payload.insert("data".into(), rmpv::Value::String("x".repeat(300).into()));
        let envelope = pack_envelope("ttt", 1, "move", "a1b2c3d4e5f60718", Some(payload), None);
        assert!(matches!(
            envelope,
            Err(LrgpError::EnvelopeTooLarge(_, ENVELOPE_MAX_PACKED))
        ));
    }

    #[test]
    fn canonical_validation_requires_exact_keys_and_lexical_forms() {
        let valid = pack_envelope(
            "chess.game",
            2,
            "draw_offer",
            "a1b2c3d4e5f60718",
            None,
            None,
        )
        .unwrap();

        let mut extra = valid.clone();
        extra.insert("x".into(), rmpv::Value::Nil);
        assert!(validate_envelope(&extra).is_err());

        for app_version in ["Ttt.1", "1ttt.1", "ttt!.1", "ttt.01", "ttt.0"] {
            let mut envelope = valid.clone();
            envelope.insert(KEY_APP.into(), rmpv::Value::String(app_version.into()));
            assert!(validate_envelope(&envelope).is_err(), "{app_version}");
        }

        for command in ["Move", "1move", "draw-offer", ""] {
            let mut envelope = valid.clone();
            envelope.insert(KEY_COMMAND.into(), rmpv::Value::String(command.into()));
            assert!(validate_envelope(&envelope).is_err(), "{command}");
        }
    }

    #[test]
    fn canonical_byte_unpack_rejects_trailing_data() {
        let envelope = pack_envelope("ttt", 1, CMD_MOVE, "a1b2c3d4e5f60718", None, None).unwrap();
        let mut bytes = pack_to_bytes(&envelope).unwrap();
        bytes.push(0xc0);
        assert!(matches!(
            unpack_from_bytes(&bytes),
            Err(LrgpError::InvalidEnvelope(message))
                if message.contains("trailing bytes")
        ));
    }

    #[test]
    fn canonical_unpack_rejects_duplicate_map_keys() {
        let envelope =
            pack_envelope("ttt", 1, CMD_CHALLENGE, "0123456789abcdef", None, None).unwrap();
        let mut pairs = match value_from_map(envelope) {
            rmpv::Value::Map(pairs) => pairs,
            _ => unreachable!(),
        };
        pairs.push((
            rmpv::Value::String(KEY_APP.into()),
            rmpv::Value::String("chess.1".into()),
        ));
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &rmpv::Value::Map(pairs)).unwrap();
        assert!(matches!(
            unpack_from_bytes(&bytes),
            Err(LrgpError::InvalidEnvelope(_))
        ));
    }

    #[test]
    fn test_vector_challenge() {
        let data = include_bytes!("../tests/ttt_challenge.bin");
        let env = unpack_from_bytes(data).unwrap();
        assert_eq!(value_as_str(env.get("a").unwrap()).unwrap(), "ttt.1");
        assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "challenge");
        assert_eq!(
            value_as_str(env.get("s").unwrap()).unwrap(),
            "a1b2c3d4e5f60718"
        );
    }

    #[test]
    fn test_vector_move() {
        let data = include_bytes!("../tests/ttt_move.bin");
        let env = unpack_from_bytes(data).unwrap();
        assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "move");
        let payload = map_from_value(env.get("p").unwrap()).unwrap();
        assert!(has_exact_keys(&payload, &["i", "b", "n", "t", "x"]));
        assert_eq!(value_as_u64(payload.get("i").unwrap()).unwrap(), 4);
        assert_eq!(
            value_as_str(payload.get("b").unwrap()).unwrap(),
            "____X____"
        );
        assert_eq!(value_as_u64(payload.get("n").unwrap()).unwrap(), 1);
    }

    #[test]
    fn test_vector_move_win() {
        let data = include_bytes!("../tests/ttt_move_win.bin");
        let env = unpack_from_bytes(data).unwrap();
        assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "move");
        let payload = map_from_value(env.get("p").unwrap()).unwrap();
        assert!(has_exact_keys(&payload, &["i", "b", "n", "t", "x", "w"]));
        assert_eq!(value_as_u64(payload.get("i").unwrap()).unwrap(), 2);
        assert_eq!(
            value_as_str(payload.get("b").unwrap()).unwrap(),
            "XXX_OO___"
        );
        assert_eq!(value_as_u64(payload.get("n").unwrap()).unwrap(), 5);
        assert_eq!(value_as_str(payload.get("x").unwrap()).unwrap(), "win");
        assert_eq!(
            value_as_str(payload.get("w").unwrap()).unwrap(),
            "abcdef0123456789"
        );
    }
}
