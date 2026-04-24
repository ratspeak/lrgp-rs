/// LRGP envelope packing, unpacking, and validation.

use std::collections::HashMap;

use crate::constants::*;
use crate::errors::LrgpError;

/// An LRGP envelope — the top-level dict stored in LXMF field 0xFD.
pub type Envelope = HashMap<String, rmpv::Value>;

/// Convenience re-export of rmpv::Value for payload manipulation.
pub use rmpv::Value;

/// Generate a fresh 8-byte random nonce using the platform CSPRNG.
pub fn generate_nonce() -> [u8; NONCE_BYTES] {
    use rand::RngCore;
    let mut n = [0u8; NONCE_BYTES];
    rand::thread_rng().fill_bytes(&mut n);
    n
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
) -> Envelope {
    let mut env = Envelope::new();
    env.insert(KEY_APP.into(), rmpv::Value::String(format!("{app_id}.{version}").into()));
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
    let packed = pack_to_bytes(envelope)?;
    let size = packed.len();
    if size > ENVELOPE_MAX_PACKED {
        return Err(LrgpError::EnvelopeTooLarge(size, ENVELOPE_MAX_PACKED));
    }
    Ok(size)
}

/// Return LXMF fields dict ready for inclusion in an LxMessage.
/// Returns `{0xFB: "lrgp.v1", 0xFD: envelope}` as a HashMap<u8, ...>.
pub fn pack_lxmf_fields(envelope: &Envelope) -> HashMap<u8, rmpv::Value> {
    let mut fields = HashMap::new();
    fields.insert(
        FIELD_CUSTOM_TYPE,
        rmpv::Value::String(PROTOCOL_TYPE.into()),
    );
    fields.insert(FIELD_CUSTOM_META, value_from_map(envelope.clone()));
    fields
}

/// Extract and validate an LRGP envelope from LXMF fields.
/// Returns `None` if this is not an LRGP (or legacy RLAP) message.
pub fn unpack_envelope(fields: &HashMap<u8, rmpv::Value>) -> Result<Option<Envelope>, LrgpError> {
    let custom_type = fields.get(&FIELD_CUSTOM_TYPE);
    let is_lrgp = match custom_type {
        Some(rmpv::Value::String(s)) => {
            let marker = s.as_str().unwrap_or("");
            marker == PROTOCOL_TYPE || LEGACY_TYPES.contains(&marker)
        }
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

    // Check required keys
    for key in &[KEY_APP, KEY_COMMAND, KEY_SESSION, KEY_PAYLOAD] {
        if !envelope.contains_key(*key) {
            return Err(LrgpError::InvalidEnvelope(format!(
                "Missing envelope key: {key}"
            )));
        }
    }

    // Validate app.version format
    let app_ver = envelope
        .get(KEY_APP)
        .and_then(|v| match v {
            rmpv::Value::String(s) => s.as_str().map(|s| s.to_string()),
            _ => None,
        })
        .ok_or_else(|| LrgpError::InvalidEnvelope("KEY_APP is not a string".into()))?;

    if !app_ver.contains('.') {
        return Err(LrgpError::InvalidEnvelope(format!(
            "Invalid app.version format: {app_ver:?}"
        )));
    }

    // Validate the nonce if present; absence is accepted for backward
    // compatibility with pre-nonce peers (the caller's dedup layer logs
    // these once per session).
    if let Some(v) = envelope.get(KEY_NONCE) {
        match v {
            rmpv::Value::Binary(b) if b.len() == NONCE_BYTES => {}
            _ => {
                return Err(LrgpError::InvalidEnvelope(format!(
                    "KEY_NONCE must be {NONCE_BYTES}-byte binary; got {v:?}"
                )));
            }
        }
    }

    Ok(Some(envelope))
}

/// Split "app_id.version" into (app_id, version).
pub fn parse_app_version(app_ver_string: &str) -> Option<(&str, u32)> {
    let dot = app_ver_string.rfind('.')?;
    let app_id = &app_ver_string[..dot];
    let version: u32 = app_ver_string[dot + 1..].parse().ok()?;
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
                map.insert(key, v.clone());
            }
            Some(map)
        }
        _ => None,
    }
}

/// Serialize an Envelope to msgpack bytes using rmpv.
pub fn pack_to_bytes(envelope: &Envelope) -> Result<Vec<u8>, LrgpError> {
    let value = value_from_map(envelope.clone());
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &value)
        .map_err(|e| LrgpError::InvalidEnvelope(format!("msgpack encode error: {e}")))?;
    Ok(buf)
}

/// Deserialize msgpack bytes into an Envelope.
pub fn unpack_from_bytes(data: &[u8]) -> Result<Envelope, LrgpError> {
    let mut cursor = std::io::Cursor::new(data);
    let value = rmpv::decode::read_value(&mut cursor)
        .map_err(|e| LrgpError::InvalidEnvelope(format!("msgpack decode error: {e}")))?;
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
        payload.insert(
            "b".to_string(),
            rmpv::Value::String("____X____".into()),
        );

        let env = pack_envelope("ttt", 1, "move", "a1b2c3d4e5f6g7h8", Some(payload), None);

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
            "a1b2c3d4e5f6g7h8"
        );
    }

    #[test]
    fn test_validate_envelope_size_ok() {
        let env = pack_envelope("ttt", 1, "challenge", "a1b2c3d4e5f6g7h8", None, None);
        let size = validate_envelope_size(&env).unwrap();
        assert!(size <= ENVELOPE_MAX_PACKED);
    }

    #[test]
    fn test_validate_envelope_size_too_large() {
        let mut payload = HashMap::new();
        // Create a huge payload to exceed the limit
        let big_string = "x".repeat(300);
        payload.insert("data".to_string(), rmpv::Value::String(big_string.into()));

        let env = pack_envelope("ttt", 1, "move", "a1b2c3d4e5f6g7h8", Some(payload), None);
        assert!(matches!(
            validate_envelope_size(&env),
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
    }

    #[test]
    fn test_unpack_envelope_not_lrgp() {
        let fields = HashMap::new();
        assert!(unpack_envelope(&fields).unwrap().is_none());
    }

    #[test]
    fn test_unpack_envelope_valid() {
        let env = pack_envelope("ttt", 1, "challenge", "abc123", None, None);
        let lxmf_fields = pack_lxmf_fields(&env);
        let result = unpack_envelope(&lxmf_fields).unwrap().unwrap();
        assert_eq!(
            value_as_str(result.get(KEY_COMMAND).unwrap()).unwrap(),
            "challenge"
        );
    }

    #[test]
    fn test_unpack_envelope_legacy_rlap() {
        // Simulate a legacy rlap.v1 message — should still be recognized
        let mut lxmf = HashMap::new();
        lxmf.insert(
            FIELD_CUSTOM_TYPE,
            rmpv::Value::String("rlap.v1".into()),
        );
        let env = pack_envelope("ttt", 1, "challenge", "abc123", None, None);
        lxmf.insert(FIELD_CUSTOM_META, value_from_map(env));
        let result = unpack_envelope(&lxmf).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_unpack_envelope_missing_key() {
        let mut lxmf = HashMap::new();
        lxmf.insert(
            FIELD_CUSTOM_TYPE,
            rmpv::Value::String(PROTOCOL_TYPE.into()),
        );
        // FIELD_CUSTOM_META has a map missing required keys
        let bad_map = rmpv::Value::Map(vec![(
            rmpv::Value::String("a".into()),
            rmpv::Value::String("ttt.1".into()),
        )]);
        lxmf.insert(FIELD_CUSTOM_META, bad_map);
        assert!(unpack_envelope(&lxmf).is_err());
    }

    #[test]
    fn test_vector_challenge() {
        let data = include_bytes!("../tests/ttt_challenge.bin");
        let env = unpack_from_bytes(data).unwrap();
        assert_eq!(value_as_str(env.get("a").unwrap()).unwrap(), "ttt.1");
        assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "challenge");
        assert_eq!(
            value_as_str(env.get("s").unwrap()).unwrap(),
            "a1b2c3d4e5f6g7h8"
        );
    }

    #[test]
    fn test_vector_move() {
        let data = include_bytes!("../tests/ttt_move.bin");
        let env = unpack_from_bytes(data).unwrap();
        assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "move");
        let payload = map_from_value(env.get("p").unwrap()).unwrap();
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
