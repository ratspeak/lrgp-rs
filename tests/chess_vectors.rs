//! Loads the chess_*.bin test vectors and asserts decode-correctness.
//!
//! These vectors are byte-identical to the ones in lrgp-py's tests/vectors/
//! directory; the cross-language fixture is what enforces wire compatibility
//! between the two reference implementations. msgpack maps are unordered by
//! spec, so we test that decoding any implementation's bytes yields the
//! expected field values — not that re-packing produces byte-identical bytes.

use lrgp::envelope::{Envelope, map_from_value, unpack_from_bytes, value_as_str, value_as_u64};

fn decode(bytes: &[u8]) -> Envelope {
    unpack_from_bytes(bytes).expect("decode succeeds")
}

fn assert_exact_keys(payload: &Envelope, expected: &[&str]) {
    assert_eq!(payload.len(), expected.len());
    assert!(payload.keys().all(|key| expected.contains(&key.as_str())));
}

#[test]
fn vector_chess_challenge() {
    let data = include_bytes!("chess_challenge.bin");
    let env = decode(data);
    assert_eq!(value_as_str(env.get("a").unwrap()).unwrap(), "chess.1");
    assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "challenge");
}

#[test]
fn vector_chess_accept() {
    let data = include_bytes!("chess_accept.bin");
    let env = decode(data);
    assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "accept");
    let payload = map_from_value(env.get("p").unwrap()).unwrap();
    // ACCEPT carries the White-player hash under "w".
    assert_exact_keys(&payload, &["w"]);
}

#[test]
fn vector_chess_move() {
    let data = include_bytes!("chess_move.bin");
    let env = decode(data);
    assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "move");
    let payload = map_from_value(env.get("p").unwrap()).unwrap();
    assert_exact_keys(&payload, &["m", "n", "x"]);
    assert_eq!(value_as_str(payload.get("m").unwrap()).unwrap(), "e2e4");
    assert_eq!(value_as_u64(payload.get("n").unwrap()).unwrap(), 0);
}

#[test]
fn vector_chess_move_promotion() {
    let data = include_bytes!("chess_move_promotion.bin");
    let env = decode(data);
    assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "move");
    let payload = map_from_value(env.get("p").unwrap()).unwrap();
    assert_exact_keys(&payload, &["m", "n", "x"]);
    // UCI promotion notation: e7e8q means pawn-to-e8 promoting to queen.
    assert_eq!(value_as_str(payload.get("m").unwrap()).unwrap(), "e7e8q");
}

#[test]
fn vector_chess_move_checkmate() {
    let data = include_bytes!("chess_move_checkmate.bin");
    let env = decode(data);
    let payload = map_from_value(env.get("p").unwrap()).unwrap();
    assert_exact_keys(&payload, &["m", "n", "x", "r", "w"]);
    // Scholar's Mate Qxf7# — terminal=win, reason=cm (checkmate).
    assert_eq!(value_as_str(payload.get("m").unwrap()).unwrap(), "h5f7");
    assert_eq!(value_as_str(payload.get("x").unwrap()).unwrap(), "win");
    assert_eq!(value_as_str(payload.get("r").unwrap()).unwrap(), "cm");
    assert!(value_as_str(payload.get("w").unwrap()).is_some());
}

#[test]
fn vector_chess_resign() {
    let data = include_bytes!("chess_resign.bin");
    let env = decode(data);
    assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "resign");
    let payload = map_from_value(env.get("p").unwrap()).unwrap();
    assert!(payload.is_empty());
}

#[test]
fn vector_chess_draw_offer() {
    let data = include_bytes!("chess_draw_offer.bin");
    let env = decode(data);
    assert_eq!(value_as_str(env.get("c").unwrap()).unwrap(), "draw_offer");
    let payload = map_from_value(env.get("p").unwrap()).unwrap();
    assert!(payload.is_empty());
}
