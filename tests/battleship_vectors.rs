//! Battleship vectors shared byte-for-byte with lrgp-py.
//!
//! lrgp-py's `tests/vectors/generate_battleship.py` plays the reference game
//! with fixed fleets and salts, the challenger shooting first by the shared
//! self-play policy. Here the challenger's side replays it: this engine must
//! pick the same shots, derive the same answers and concession, and accept
//! the responder's reveal, which checks every answer the responder gave.

mod support;

use std::collections::HashMap;

use lrgp::app_base::GameApp;
use lrgp::apps::battleship::{BattleshipApp, commitment};
use lrgp::constants::{CMD_ACCEPT, CMD_CHALLENGE, CMD_MOVE, STATUS_COMPLETED};
use support::*;

const CHALLENGER_FLEET: &str = "b2v d9h g3v i6v e1h";
const CHALLENGER_SALT: &str = "00112233445566778899aabbccddeeff";
const RESPONDER_FLEET: &str = "a10h c2v f5h h7v j1v";
const RESPONDER_SALT: &str = "ffeeddccbbaa99887766554433221100";
const SEED: usize = 3;
const CHALLENGE_NONCE: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
const CONCEDE_NONCE: [u8; 8] = [3, 4, 5, 6, 7, 8, 9, 10];

fn text(value: &str) -> rmpv::Value {
    rmpv::Value::String(value.into())
}

fn field(value: &str) -> &str {
    if value == "-" { "" } else { value }
}

#[test]
fn battleship_fixtures_decode() {
    let challenge = payload_of(
        include_bytes!("battleship_challenge.bin"),
        "battleship",
        CMD_CHALLENGE,
        &CHALLENGE_NONCE,
    );
    assert_exact_keys(&challenge, &["h"]);
    let commit = commitment(CHALLENGER_FLEET, CHALLENGER_SALT);
    assert_eq!(str_of(&challenge, "h"), Some(commit.as_str()));

    let accept = payload_of(
        include_bytes!("battleship_accept.bin"),
        "battleship",
        CMD_ACCEPT,
        &ACCEPT_NONCE,
    );
    assert_exact_keys(&accept, &["b", "h"]);
    assert_eq!(str_of(&accept, "b"), Some(CHALLENGER));
    let commit = commitment(RESPONDER_FLEET, RESPONDER_SALT);
    assert_eq!(str_of(&accept, "h"), Some(commit.as_str()));

    let first = payload_of(
        include_bytes!("battleship_move.bin"),
        "battleship",
        CMD_MOVE,
        &MOVE_NONCE,
    );
    assert_exact_keys(&first, &["s", "a", "n", "x"]);
    let concede = payload_of(
        include_bytes!("battleship_concede.bin"),
        "battleship",
        CMD_MOVE,
        &CONCEDE_NONCE,
    );
    assert_exact_keys(&concede, &["a", "f", "k", "n", "x"]);
    assert_eq!(str_of(&concede, "f"), Some(CHALLENGER_FLEET));
    let win = payload_of(
        include_bytes!("battleship_move_win.bin"),
        "battleship",
        CMD_MOVE,
        &WIN_NONCE,
    );
    assert_exact_keys(&win, &["f", "k", "n", "x", "w"]);
    assert_eq!(str_of(&win, "w"), Some(RESPONDER));
    assert_eq!(str_of(&win, "f"), Some(RESPONDER_FLEET));
}

#[test]
fn battleship_reference_game_matches_lrgp_py() {
    let lines = moves_of(include_str!("battleship_reference.txt"));
    assert_eq!(lines.len(), 150);
    let concede = payload_of(
        include_bytes!("battleship_concede.bin"),
        "battleship",
        CMD_MOVE,
        &CONCEDE_NONCE,
    );
    let win = payload_of(
        include_bytes!("battleship_move_win.bin"),
        "battleship",
        CMD_MOVE,
        &WIN_NONCE,
    );

    let app = BattleshipApp::new();
    let intent = HashMap::from([
        ("f".to_string(), text(CHALLENGER_FLEET)),
        ("k".to_string(), text(CHALLENGER_SALT)),
    ]);
    let challenge = app.handle_outgoing(SESSION_ID, CMD_CHALLENGE, &intent, CHALLENGER);
    let expected = payload_of(
        include_bytes!("battleship_challenge.bin"),
        "battleship",
        CMD_CHALLENGE,
        &CHALLENGE_NONCE,
    );
    assert_eq!(challenge.payload, expected);
    app.bind_session_peer(SESSION_ID, CHALLENGER, RESPONDER)
        .unwrap();
    let accept = payload_of(
        include_bytes!("battleship_accept.bin"),
        "battleship",
        CMD_ACCEPT,
        &ACCEPT_NONCE,
    );
    let accepted = app.handle_incoming(SESSION_ID, CMD_ACCEPT, &accept, RESPONDER, CHALLENGER);
    assert!(accepted.error.is_none(), "{:?}", accepted.error);

    for (ply, line) in lines.iter().enumerate() {
        let (shot, answer) = line.split_once(' ').unwrap();
        let (shot, answer) = (field(shot), field(answer));
        let meta = metadata(&app);
        if meta["turn"] == CHALLENGER {
            let wire = if shot.is_empty() {
                assert_eq!(meta["phase"], "concede", "ply {ply}");
                app.handle_outgoing(SESSION_ID, CMD_MOVE, &HashMap::new(), CHALLENGER)
                    .payload
            } else {
                let legal = meta["legal_moves"].as_array().unwrap();
                assert_eq!(legal[(ply * 7 + SEED) % legal.len()], shot, "ply {ply}");
                app.handle_outgoing(
                    SESSION_ID,
                    CMD_MOVE,
                    &HashMap::from([("s".to_string(), text(shot))]),
                    CHALLENGER,
                )
                .payload
            };
            assert_eq!(str_of(&wire, "a"), Some(answer), "ply {ply}");
            if ply == 0 {
                let first = payload_of(
                    include_bytes!("battleship_move.bin"),
                    "battleship",
                    CMD_MOVE,
                    &MOVE_NONCE,
                );
                assert_eq!(wire, first);
            }
            if shot.is_empty() {
                assert_eq!(wire, concede);
            }
        } else {
            assert_eq!(meta["turn"], RESPONDER, "ply {ply}");
            let wire = if ply + 1 == lines.len() {
                win.clone()
            } else {
                HashMap::from([
                    ("s".to_string(), text(shot)),
                    ("a".to_string(), text(answer)),
                    ("n".to_string(), rmpv::Value::from(ply as u64)),
                    ("x".to_string(), text("")),
                ])
            };
            let result = app.handle_incoming(SESSION_ID, CMD_MOVE, &wire, RESPONDER, CHALLENGER);
            assert!(result.error.is_none(), "ply {ply}: {:?}", result.error);
        }
    }
    let state = app.get_session_state(SESSION_ID, CHALLENGER);
    assert_eq!(state["status"], STATUS_COMPLETED);
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "snk");
    assert_eq!(meta["their_fleet"], RESPONDER_FLEET);
    assert_eq!(meta["my_sunk"], 5);
}
