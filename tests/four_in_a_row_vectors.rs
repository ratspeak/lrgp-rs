//! Canonical Four in a Row vectors shared byte-for-byte with lrgp-py.

use lrgp::envelope::{Envelope, map_from_value, unpack_from_bytes, value_as_str, value_as_u64};

const SESSION_ID: &str = "0123456789abcdef";
const CHALLENGER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn decode(bytes: &[u8]) -> Envelope {
    unpack_from_bytes(bytes).expect("canonical vector decodes")
}

fn assert_common(envelope: &Envelope, command: &str, nonce: &[u8]) {
    assert_eq!(
        value_as_str(envelope.get("a").unwrap()),
        Some("four_in_a_row.1")
    );
    assert_eq!(value_as_str(envelope.get("c").unwrap()), Some(command));
    assert_eq!(value_as_str(envelope.get("s").unwrap()), Some(SESSION_ID));
    assert_eq!(
        envelope.get("n").and_then(|value| value.as_slice()),
        Some(nonce)
    );
}

fn assert_exact_keys(payload: &Envelope, expected: &[&str]) {
    assert_eq!(payload.len(), expected.len());
    assert!(payload.keys().all(|key| expected.contains(&key.as_str())));
}

#[test]
fn vector_four_in_a_row_challenge() {
    // SHA-256 b5f7610b897ebefe5f284b48ef80167258bbc78482d47b8c723ef60766a7ed3e
    let bytes = include_bytes!("four_in_a_row_challenge.bin");
    assert_eq!(bytes.len(), 65);
    let envelope = decode(bytes);
    assert_common(&envelope, "challenge", &[0, 1, 2, 3, 4, 5, 6, 7]);
    assert!(
        map_from_value(envelope.get("p").unwrap())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn vector_four_in_a_row_accept() {
    // SHA-256 ca118a7542eb5df799585d4ad4770f1a7e6e2fb848604342c4a124e119fcb5a4
    let bytes = include_bytes!("four_in_a_row_accept.bin");
    assert_eq!(bytes.len(), 98);
    let envelope = decode(bytes);
    assert_common(&envelope, "accept", &[1, 2, 3, 4, 5, 6, 7, 8]);
    let payload = map_from_value(envelope.get("p").unwrap()).unwrap();
    assert_exact_keys(&payload, &["t"]);
    assert_eq!(value_as_str(payload.get("t").unwrap()), Some(CHALLENGER));
}

#[test]
fn vector_four_in_a_row_move() {
    // SHA-256 e60e0241bd0602f19db1a69b0ea35fa42adaa509b69eccd00f76507c05ff64fa
    let bytes = include_bytes!("four_in_a_row_move.bin");
    assert_eq!(bytes.len(), 69);
    let envelope = decode(bytes);
    assert_common(&envelope, "move", &[2, 3, 4, 5, 6, 7, 8, 9]);
    let payload = map_from_value(envelope.get("p").unwrap()).unwrap();
    assert_exact_keys(&payload, &["c", "n", "x"]);
    assert_eq!(value_as_u64(payload.get("c").unwrap()), Some(3));
    assert_eq!(value_as_u64(payload.get("n").unwrap()), Some(1));
    assert_eq!(value_as_str(payload.get("x").unwrap()), Some(""));
}

#[test]
fn vector_four_in_a_row_winning_move() {
    // SHA-256 1880f4286f9708b17ee5b2aaae1bbdbebff874effe3a70118857dca0de16955a
    let bytes = include_bytes!("four_in_a_row_move_win.bin");
    assert_eq!(bytes.len(), 108);
    let envelope = decode(bytes);
    assert_common(&envelope, "move", &[3, 4, 5, 6, 7, 8, 9, 10]);
    let payload = map_from_value(envelope.get("p").unwrap()).unwrap();
    assert_exact_keys(&payload, &["c", "n", "x", "w"]);
    assert_eq!(value_as_u64(payload.get("c").unwrap()), Some(0));
    assert_eq!(value_as_u64(payload.get("n").unwrap()), Some(7));
    assert_eq!(value_as_str(payload.get("x").unwrap()), Some("win"));
    assert_eq!(value_as_str(payload.get("w").unwrap()), Some(CHALLENGER));
}
