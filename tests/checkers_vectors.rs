//! Canonical Checkers vectors shared byte-for-byte with lrgp-py.

use std::collections::HashMap;

use lrgp::app_base::GameApp;
use lrgp::apps::checkers::CheckersApp;
use lrgp::constants::{CMD_ACCEPT, CMD_CHALLENGE, CMD_DRAW_OFFER, CMD_MOVE, STATUS_COMPLETED};
use lrgp::envelope::{Envelope, map_from_value, unpack_from_bytes, value_as_str, value_as_u64};

const SESSION_ID: &str = "0123456789abcdef";
const CHALLENGER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RESPONDER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// The reference game from lrgp-py's `tests/vectors/generate_checkers.py`.
/// The challenger plays Black; ply 16 is a king's four-capture circuit back
/// to its origin and ply 32 wins by leaving White without pieces.
const REFERENCE_GAME: [&str; 33] = [
    "f6 g5",
    "g3 f4",
    "e7 f6",
    "e3 d4",
    "g5 e3",
    "d2 f4",
    "d6 c5",
    "h2 g3",
    "c5 e3",
    "f2 d4",
    "f6 g5",
    "a3 b4",
    "g5 e3",
    "c1 d2",
    "e3 c1",
    "e1 d2",
    "c1 e3 c5 a3 c1",
    "g3 f4",
    "c7 d6",
    "f4 e5",
    "d6 f4",
    "g1 h2",
    "f4 g3",
    "h2 f4",
    "b6 c5",
    "a1 b2",
    "c1 a3",
    "f4 e5",
    "c5 d4",
    "e5 d6",
    "d4 b2",
    "d6 c7",
    "d8 b6",
];

fn decode(bytes: &[u8]) -> Envelope {
    unpack_from_bytes(bytes).expect("canonical vector decodes")
}

fn payload_of(bytes: &[u8], command: &str, nonce: &[u8]) -> Envelope {
    let envelope = decode(bytes);
    assert_eq!(value_as_str(envelope.get("a").unwrap()), Some("checkers.1"));
    assert_eq!(value_as_str(envelope.get("c").unwrap()), Some(command));
    assert_eq!(value_as_str(envelope.get("s").unwrap()), Some(SESSION_ID));
    assert_eq!(
        envelope.get("n").and_then(|value| value.as_slice()),
        Some(nonce)
    );
    map_from_value(envelope.get("p").unwrap()).unwrap()
}

fn assert_exact_keys(payload: &Envelope, expected: &[&str]) {
    assert_eq!(payload.len(), expected.len());
    assert!(payload.keys().all(|key| expected.contains(&key.as_str())));
}

fn str_of<'a>(payload: &'a Envelope, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(value_as_str)
}

#[test]
fn vector_checkers_challenge() {
    // SHA-256 88c9054a80899e07da974e18947849c1640d76b97198059cd2fac8ec9495489a
    let bytes = include_bytes!("checkers_challenge.bin");
    assert_eq!(bytes.len(), 60);
    let payload = payload_of(bytes, "challenge", &[0, 1, 2, 3, 4, 5, 6, 7]);
    assert!(payload.is_empty());
}

#[test]
fn vector_checkers_accept() {
    // SHA-256 e1e4374c8d447fe0b25fda5f3f1b3661f9a1dc131b67219f513feeab7fd3653b
    let bytes = include_bytes!("checkers_accept.bin");
    assert_eq!(bytes.len(), 93);
    let payload = payload_of(bytes, "accept", &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_exact_keys(&payload, &["b"]);
    assert_eq!(str_of(&payload, "b"), Some(CHALLENGER));
}

#[test]
fn vector_checkers_move() {
    // SHA-256 a9343378454b60989c2a75ac62a06186be44c705aa2f9c5888d79cea2554f298
    let bytes = include_bytes!("checkers_move.bin");
    assert_eq!(bytes.len(), 69);
    let payload = payload_of(bytes, "move", &[2, 3, 4, 5, 6, 7, 8, 9]);
    assert_exact_keys(&payload, &["m", "n", "x"]);
    assert_eq!(str_of(&payload, "m"), Some(REFERENCE_GAME[0]));
    assert_eq!(value_as_u64(payload.get("n").unwrap()), Some(0));
    assert_eq!(str_of(&payload, "x"), Some(""));
}

#[test]
fn vector_checkers_multi_jump_move() {
    // SHA-256 f916e2fc405779c92c6d35bb353f3a2e79c0444d9c030b79ebb9624d87561d03
    let bytes = include_bytes!("checkers_move_jump.bin");
    assert_eq!(bytes.len(), 78);
    let payload = payload_of(bytes, "move", &[3, 4, 5, 6, 7, 8, 9, 10]);
    assert_exact_keys(&payload, &["m", "n", "x"]);
    assert_eq!(str_of(&payload, "m"), Some("c1 e3 c5 a3 c1"));
    assert_eq!(value_as_u64(payload.get("n").unwrap()), Some(16));
    assert_eq!(str_of(&payload, "x"), Some(""));
}

#[test]
fn vector_checkers_winning_move() {
    // SHA-256 a35c3ef56db3a84139eec4bd00efd7c83acb3b18b7483082009a8f46452131a1
    let bytes = include_bytes!("checkers_move_win.bin");
    assert_eq!(bytes.len(), 113);
    let payload = payload_of(bytes, "move", &[4, 5, 6, 7, 8, 9, 10, 11]);
    assert_exact_keys(&payload, &["m", "n", "x", "r", "w"]);
    assert_eq!(str_of(&payload, "m"), Some(REFERENCE_GAME[32]));
    assert_eq!(value_as_u64(payload.get("n").unwrap()), Some(32));
    assert_eq!(str_of(&payload, "x"), Some("win"));
    assert_eq!(str_of(&payload, "r"), Some("np"));
    assert_eq!(str_of(&payload, "w"), Some(CHALLENGER));
}

#[test]
fn vector_checkers_draw_claim() {
    // SHA-256 170b41bcfa186d1c74609dc2a82822dbe12254b9bc6218996cf6579c076d7f0d
    let bytes = include_bytes!("checkers_draw_claim.bin");
    assert_eq!(bytes.len(), 67);
    let payload = payload_of(bytes, "draw_offer", &[5, 6, 7, 8, 9, 10, 11, 12]);
    assert_exact_keys(&payload, &["r"]);
    assert_eq!(str_of(&payload, "r"), Some("3fr"));
}

fn metadata(app: &CheckersApp) -> serde_json::Map<String, serde_json::Value> {
    app.get_session_state(SESSION_ID, CHALLENGER)["metadata"]
        .as_object()
        .unwrap()
        .clone()
}

/// Replays the reference game from the challenger's side. Black's moves are
/// derived by this engine and must equal lrgp-py's payloads byte-for-value;
/// White's moves arrive as wire payloads and must be accepted. Both engines
/// must also expose the same legal moves in the same order, which the
/// shared self-play policy observes.
#[test]
fn reference_game_matches_lrgp_py() {
    let app = CheckersApp::new();
    app.handle_outgoing(SESSION_ID, CMD_CHALLENGE, &HashMap::new(), CHALLENGER);
    app.bind_session_peer(SESSION_ID, CHALLENGER, RESPONDER)
        .unwrap();
    let accept = payload_of(
        include_bytes!("checkers_accept.bin"),
        CMD_ACCEPT,
        &[1, 2, 3, 4, 5, 6, 7, 8],
    );
    let accepted = app.handle_incoming(SESSION_ID, CMD_ACCEPT, &accept, RESPONDER, CHALLENGER);
    assert!(accepted.error.is_none(), "{:?}", accepted.error);

    let mut last = HashMap::new();
    for (ply, expected) in REFERENCE_GAME.iter().enumerate() {
        let meta = metadata(&app);
        let legal: Vec<&str> = meta["legal_moves"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        let longest = legal.iter().map(|m| m.len()).max().unwrap();
        let candidates: Vec<&str> = legal.into_iter().filter(|m| m.len() == longest).collect();
        assert_eq!(candidates[(ply * 7 + 5) % candidates.len()], *expected);

        let intent = HashMap::from([("m".to_string(), rmpv::Value::String((*expected).into()))]);
        if ply % 2 == 0 {
            assert_eq!(meta["turn"], CHALLENGER);
            let (valid, message) =
                app.validate_outgoing_action(SESSION_ID, CMD_MOVE, &intent, CHALLENGER);
            assert!(valid, "ply {ply}: {message:?}");
            last = app
                .handle_outgoing(SESSION_ID, CMD_MOVE, &intent, CHALLENGER)
                .payload;
            assert_eq!(str_of(&last, "m"), Some(*expected));
            assert_eq!(value_as_u64(last.get("n").unwrap()), Some(ply as u64));
        } else {
            assert_eq!(meta["turn"], RESPONDER);
            let wire = HashMap::from([
                ("m".to_string(), rmpv::Value::String((*expected).into())),
                ("n".to_string(), rmpv::Value::Integer((ply as u64).into())),
                ("x".to_string(), rmpv::Value::String("".into())),
            ]);
            let result = app.handle_incoming(SESSION_ID, CMD_MOVE, &wire, RESPONDER, CHALLENGER);
            assert!(result.error.is_none(), "ply {ply}: {:?}", result.error);
        }
    }

    let expected_ply16 = payload_of(
        include_bytes!("checkers_move_jump.bin"),
        CMD_MOVE,
        &[3, 4, 5, 6, 7, 8, 9, 10],
    );
    assert_eq!(str_of(&expected_ply16, "m"), Some(REFERENCE_GAME[16]));
    let expected_win = payload_of(
        include_bytes!("checkers_move_win.bin"),
        CMD_MOVE,
        &[4, 5, 6, 7, 8, 9, 10, 11],
    );
    assert_eq!(last, expected_win);

    let state = app.get_session_state(SESSION_ID, CHALLENGER);
    assert_eq!(state["status"], STATUS_COMPLETED);
    let meta = metadata(&app);
    assert_eq!(meta["terminal"], "win");
    assert_eq!(meta["terminal_reason"], "np");
    assert_eq!(meta["winner"], CHALLENGER);

    // A claim after completion is a state-machine violation, not a draw.
    let claim = payload_of(
        include_bytes!("checkers_draw_claim.bin"),
        CMD_DRAW_OFFER,
        &[5, 6, 7, 8, 9, 10, 11, 12],
    );
    let late = app.handle_incoming(SESSION_ID, CMD_DRAW_OFFER, &claim, RESPONDER, CHALLENGER);
    assert!(late.error.is_some());
}
