//! Helpers shared by the vector tests of the newer single-token games.
//!
//! lrgp-py's `tests/vectors/generate_placement.py` writes their fixtures. Each
//! reference game was played by the shared self-play policy
//! `legal[(ply * 7 + seed) % len]` with the challenger as Black.

#![allow(dead_code)]

use std::collections::HashMap;

use lrgp::app_base::GameApp;
use lrgp::constants::{CMD_ACCEPT, CMD_CHALLENGE, CMD_MOVE, STATUS_COMPLETED};
use lrgp::envelope::{Envelope, map_from_value, unpack_from_bytes, value_as_str, value_as_u64};

pub const SESSION_ID: &str = "0123456789abcdef";
pub const CHALLENGER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const RESPONDER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub const ACCEPT_NONCE: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
pub const MOVE_NONCE: [u8; 8] = [2, 3, 4, 5, 6, 7, 8, 9];
pub const WIN_NONCE: [u8; 8] = [4, 5, 6, 7, 8, 9, 10, 11];
pub const DRAW_NONCE: [u8; 8] = [5, 6, 7, 8, 9, 10, 11, 12];

pub fn payload_of(bytes: &[u8], app: &str, command: &str, nonce: &[u8]) -> Envelope {
    let envelope = unpack_from_bytes(bytes).expect("canonical vector decodes");
    assert_eq!(
        value_as_str(envelope.get("a").unwrap()),
        Some(format!("{app}.1").as_str())
    );
    assert_eq!(value_as_str(envelope.get("c").unwrap()), Some(command));
    assert_eq!(value_as_str(envelope.get("s").unwrap()), Some(SESSION_ID));
    assert_eq!(
        envelope.get("n").and_then(|value| value.as_slice()),
        Some(nonce)
    );
    map_from_value(envelope.get("p").unwrap()).unwrap()
}

pub fn assert_exact_keys(payload: &Envelope, expected: &[&str]) {
    assert_eq!(payload.len(), expected.len());
    assert!(payload.keys().all(|key| expected.contains(&key.as_str())));
}

pub fn str_of<'a>(payload: &'a Envelope, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(value_as_str)
}

pub fn moves_of(text: &str) -> Vec<&str> {
    assert!(text.ends_with('\n') && !text.contains('\r'));
    text.lines().collect()
}

pub fn metadata(app: &dyn GameApp) -> serde_json::Map<String, serde_json::Value> {
    app.get_session_state(SESSION_ID, CHALLENGER)["metadata"]
        .as_object()
        .unwrap()
        .clone()
}

pub fn intent(notation: &str) -> HashMap<String, rmpv::Value> {
    HashMap::from([("m".to_string(), rmpv::Value::String(notation.into()))])
}

/// Replay `moves` from the challenger's side. The challenger's moves are
/// derived by this engine; the responder's arrive as wire payloads, with
/// `final_wire` used for a responder move that ends the game. Returns the
/// challenger's last outgoing payload, if the challenger moved last.
pub fn replay_from_challenger(
    app: &dyn GameApp,
    app_id: &str,
    accept: &[u8],
    moves: &[&str],
    seed: Option<usize>,
    final_wire: &Envelope,
) -> Option<Envelope> {
    app.handle_outgoing(SESSION_ID, CMD_CHALLENGE, &HashMap::new(), CHALLENGER);
    app.bind_session_peer(SESSION_ID, CHALLENGER, RESPONDER)
        .unwrap();
    let accept = payload_of(accept, app_id, CMD_ACCEPT, &ACCEPT_NONCE);
    let accepted = app.handle_incoming(SESSION_ID, CMD_ACCEPT, &accept, RESPONDER, CHALLENGER);
    assert!(accepted.error.is_none(), "{:?}", accepted.error);

    let mut last = None;
    for (ply, notation) in moves.iter().enumerate() {
        let meta = metadata(app);
        let legal: Vec<&str> = meta["legal_moves"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert!(legal.contains(notation), "{app_id} ply {ply}: {notation}");
        if let Some(seed) = seed {
            assert_eq!(legal[(ply * 7 + seed) % legal.len()], *notation);
        }

        if meta["turn"] == CHALLENGER {
            let (valid, message) =
                app.validate_outgoing_action(SESSION_ID, CMD_MOVE, &intent(notation), CHALLENGER);
            assert!(valid, "{app_id} ply {ply}: {message:?}");
            let payload = app
                .handle_outgoing(SESSION_ID, CMD_MOVE, &intent(notation), CHALLENGER)
                .payload;
            assert_eq!(str_of(&payload, "m"), Some(*notation));
            assert_eq!(value_as_u64(payload.get("n").unwrap()), Some(ply as u64));
            last = Some(payload);
        } else {
            assert_eq!(meta["turn"], RESPONDER, "{app_id} ply {ply}");
            let wire = if ply + 1 == moves.len() {
                final_wire.clone()
            } else {
                HashMap::from([
                    ("m".to_string(), rmpv::Value::String((*notation).into())),
                    ("n".to_string(), rmpv::Value::Integer((ply as u64).into())),
                    ("x".to_string(), rmpv::Value::String("".into())),
                ])
            };
            let result = app.handle_incoming(SESSION_ID, CMD_MOVE, &wire, RESPONDER, CHALLENGER);
            assert!(
                result.error.is_none(),
                "{app_id} ply {ply}: {:?}",
                result.error
            );
            last = None;
        }
    }
    let state = app.get_session_state(SESSION_ID, CHALLENGER);
    assert_eq!(state["status"], STATUS_COMPLETED);
    last
}

/// Check a game's accept, first-move, and winning-move fixtures and return
/// the winning payload. `moves` is its reference game, won by the responder.
pub fn check_vectors(
    app_id: &str,
    accept: &[u8],
    first: &[u8],
    win: &[u8],
    moves: &[&str],
) -> Envelope {
    let payload = payload_of(accept, app_id, CMD_ACCEPT, &ACCEPT_NONCE);
    assert_exact_keys(&payload, &["b"]);
    assert_eq!(str_of(&payload, "b"), Some(CHALLENGER));

    let payload = payload_of(first, app_id, CMD_MOVE, &MOVE_NONCE);
    assert_exact_keys(&payload, &["m", "n", "x"]);
    assert_eq!(str_of(&payload, "m"), moves.first().copied());
    assert_eq!(value_as_u64(payload.get("n").unwrap()), Some(0));
    assert_eq!(str_of(&payload, "x"), Some(""));

    let win = payload_of(win, app_id, CMD_MOVE, &WIN_NONCE);
    assert_exact_keys(&win, &["m", "n", "x", "w"]);
    assert_eq!(str_of(&win, "m"), moves.last().copied());
    assert_eq!(
        value_as_u64(win.get("n").unwrap()),
        Some(moves.len() as u64 - 1)
    );
    assert_eq!(str_of(&win, "x"), Some("win"));
    assert_eq!(str_of(&win, "w"), Some(RESPONDER));
    win
}

/// Check a drawn game's final-move fixture and return its payload.
pub fn check_draw(app_id: &str, draw: &[u8], moves: &[&str]) -> Envelope {
    let draw = payload_of(draw, app_id, CMD_MOVE, &DRAW_NONCE);
    assert_exact_keys(&draw, &["m", "n", "x"]);
    assert_eq!(str_of(&draw, "m"), moves.last().copied());
    assert_eq!(
        value_as_u64(draw.get("n").unwrap()),
        Some(moves.len() as u64 - 1)
    );
    assert_eq!(str_of(&draw, "x"), Some("draw"));
    draw
}
