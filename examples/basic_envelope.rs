//! Demonstrates LRGP envelope packing, unpacking, and validation.

use std::collections::HashMap;

use lrgp::constants::*;
use lrgp::envelope::*;

fn main() {
    // Pack a challenge envelope
    let env = pack_envelope("ttt", 1, "challenge", "a1b2c3d4e5f6g7h8", None, None);
    println!("Challenge envelope: {env:?}");

    // Validate size fits OPPORTUNISTIC delivery
    let size = validate_envelope_size(&env).unwrap();
    println!("Packed size: {size} bytes (max {ENVELOPE_MAX_PACKED})");

    // Serialize to msgpack bytes
    let bytes = pack_to_bytes(&env).unwrap();
    println!(
        "Wire bytes ({} bytes): {}",
        bytes.len(),
        hex::encode(&bytes)
    );

    // Deserialize back
    let recovered = unpack_from_bytes(&bytes).unwrap();
    let app = value_as_str(recovered.get(KEY_APP).unwrap()).unwrap();
    let cmd = value_as_str(recovered.get(KEY_COMMAND).unwrap()).unwrap();
    let sid = value_as_str(recovered.get(KEY_SESSION).unwrap()).unwrap();
    println!("Recovered: app={app}, command={cmd}, session={sid}");

    // Pack a move with payload
    let mut payload = HashMap::new();
    payload.insert("i".to_string(), rmpv::Value::Integer(4.into()));
    payload.insert("b".to_string(), rmpv::Value::String("____X____".into()));
    payload.insert("n".to_string(), rmpv::Value::Integer(1.into()));

    let move_env = pack_envelope("ttt", 1, "move", "a1b2c3d4e5f6g7h8", Some(payload), None);
    let move_size = validate_envelope_size(&move_env).unwrap();
    println!("\nMove envelope size: {move_size} bytes");

    // Pack into LXMF fields
    let lxmf_fields = pack_lxmf_fields(&move_env);
    println!("LXMF fields: type=0x{FIELD_CUSTOM_TYPE:02X}, meta=0x{FIELD_CUSTOM_META:02X}");

    // Extract back from LXMF fields
    let extracted = unpack_envelope(&lxmf_fields).unwrap().unwrap();
    println!(
        "Extracted command: {}",
        value_as_str(extracted.get(KEY_COMMAND).unwrap()).unwrap()
    );

    println!("\nAll operations successful.");
}
