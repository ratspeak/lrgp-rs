//! Rock Paper Scissors vectors shared byte-for-byte with lrgp-py.
//!
//! lrgp-py's `tests/vectors/generate_rps.py` plays a scripted match with the
//! challenger leading and a fixed salt for each commitment. Here the
//! challenger's side replays it: this engine must send the same commitments
//! and reveals, and accept the responder's choices and the match result.

mod support;

use std::collections::HashMap;

use lrgp::app_base::GameApp;
use lrgp::apps::rps::{RpsApp, commitment};
use lrgp::constants::{CMD_ACCEPT, CMD_CHALLENGE, CMD_MOVE, STATUS_COMPLETED};
use lrgp::envelope::value_as_u64;
use support::*;

/// (leader, follower) per round; the follower wins 3 to 2.
const ROUNDS: [(&str, &str); 6] = [
    ("r", "p"),
    ("s", "p"),
    ("p", "p"),
    ("r", "s"),
    ("s", "r"),
    ("p", "s"),
];
const CHALLENGE_NONCE: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
const REVEAL_NONCE: [u8; 8] = [3, 4, 5, 6, 7, 8, 9, 10];
const CHOOSE_NONCE: [u8; 8] = [6, 7, 8, 9, 10, 11, 12, 13];

fn text(value: &str) -> rmpv::Value {
    rmpv::Value::String(value.into())
}

/// The leader's salt for its commitment to round `index`.
fn salt(index: usize) -> String {
    format!("{index:02x}").repeat(16)
}

fn choose(choice: &str, index: usize) -> HashMap<String, rmpv::Value> {
    HashMap::from([
        ("c".to_string(), text(choice)),
        ("k".to_string(), text(&salt(index))),
    ])
}

#[test]
fn rps_fixtures_decode() {
    let challenge = payload_of(
        include_bytes!("rps_challenge.bin"),
        "rps",
        CMD_CHALLENGE,
        &CHALLENGE_NONCE,
    );
    assert!(challenge.is_empty());
    let accept = payload_of(
        include_bytes!("rps_accept.bin"),
        "rps",
        CMD_ACCEPT,
        &ACCEPT_NONCE,
    );
    assert_exact_keys(&accept, &["b"]);
    assert_eq!(str_of(&accept, "b"), Some(CHALLENGER));

    let commit = payload_of(include_bytes!("rps_move.bin"), "rps", CMD_MOVE, &MOVE_NONCE);
    assert_exact_keys(&commit, &["h", "n", "x"]);
    assert_eq!(
        str_of(&commit, "h"),
        Some(commitment("r", &salt(0)).as_str())
    );
    let chosen = payload_of(
        include_bytes!("rps_choose.bin"),
        "rps",
        CMD_MOVE,
        &CHOOSE_NONCE,
    );
    assert_exact_keys(&chosen, &["c", "n", "x"]);
    assert_eq!(str_of(&chosen, "c"), Some("p"));
    let reveal = payload_of(
        include_bytes!("rps_reveal.bin"),
        "rps",
        CMD_MOVE,
        &REVEAL_NONCE,
    );
    assert_exact_keys(&reveal, &["c", "k", "h", "n", "x"]);
    assert_eq!(
        str_of(&reveal, "h"),
        Some(commitment("s", &salt(1)).as_str())
    );
    let win = payload_of(
        include_bytes!("rps_move_win.bin"),
        "rps",
        CMD_MOVE,
        &WIN_NONCE,
    );
    assert_exact_keys(&win, &["c", "k", "n", "x", "w"]);
    assert_eq!(str_of(&win, "w"), Some(RESPONDER));
    assert_eq!(value_as_u64(win.get("n").unwrap()), Some(12));
}

#[test]
fn rps_reference_match_matches_lrgp_py() {
    let app = RpsApp::new();
    let challenge = app.handle_outgoing(SESSION_ID, CMD_CHALLENGE, &HashMap::new(), CHALLENGER);
    assert!(challenge.payload.is_empty());
    app.bind_session_peer(SESSION_ID, CHALLENGER, RESPONDER)
        .unwrap();
    let accept = payload_of(
        include_bytes!("rps_accept.bin"),
        "rps",
        CMD_ACCEPT,
        &ACCEPT_NONCE,
    );
    let accepted = app.handle_incoming(SESSION_ID, CMD_ACCEPT, &accept, RESPONDER, CHALLENGER);
    assert!(accepted.error.is_none(), "{:?}", accepted.error);

    let send = |intent: &HashMap<String, rmpv::Value>| {
        let (valid, message) =
            app.validate_outgoing_action(SESSION_ID, CMD_MOVE, intent, CHALLENGER);
        assert!(valid, "{message:?}");
        app.handle_outgoing(SESSION_ID, CMD_MOVE, intent, CHALLENGER)
            .payload
    };
    let commit = send(&choose(ROUNDS[0].0, 0));
    assert_eq!(
        commit,
        payload_of(include_bytes!("rps_move.bin"), "rps", CMD_MOVE, &MOVE_NONCE)
    );

    let mut wires = vec![commit];
    for (index, (_, follow)) in ROUNDS.iter().enumerate() {
        let ply = wires.len() as u64;
        let chosen = HashMap::from([
            ("c".to_string(), text(follow)),
            ("n".to_string(), rmpv::Value::from(ply)),
            ("x".to_string(), text("")),
        ]);
        if index == 0 {
            assert_eq!(
                chosen,
                payload_of(
                    include_bytes!("rps_choose.bin"),
                    "rps",
                    CMD_MOVE,
                    &CHOOSE_NONCE
                )
            );
        }
        let result = app.handle_incoming(SESSION_ID, CMD_MOVE, &chosen, RESPONDER, CHALLENGER);
        assert!(result.error.is_none(), "ply {ply}: {:?}", result.error);
        wires.push(chosen);
        let next = ROUNDS
            .get(index + 1)
            .map(|(lead, _)| choose(lead, index + 1))
            .unwrap_or_default();
        wires.push(send(&next));
    }
    assert_eq!(
        wires[2],
        payload_of(
            include_bytes!("rps_reveal.bin"),
            "rps",
            CMD_MOVE,
            &REVEAL_NONCE
        )
    );
    assert_eq!(
        wires.last().unwrap(),
        &payload_of(
            include_bytes!("rps_move_win.bin"),
            "rps",
            CMD_MOVE,
            &WIN_NONCE
        )
    );

    let state = app.get_session_state(SESSION_ID, CHALLENGER);
    assert_eq!(state["status"], STATUS_COMPLETED);
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "rnd");
    assert_eq!(meta["black_score"], 2);
    assert_eq!(meta["white_score"], 3);
}
