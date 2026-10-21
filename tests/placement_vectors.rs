//! Canonical Hex, Gomoku, Reversi, Dots and Boxes, and Mancala vectors
//! shared byte-for-byte with lrgp-py.
//!
//! lrgp-py's `tests/vectors/generate_placement.py` writes these fixtures. Each
//! reference game was played by the shared self-play policy
//! `legal[(ply * 7 + seed) % len]` with the challenger as Black, so replaying
//! it here checks that both engines list legal moves identically and derive
//! the same payloads.

use std::collections::HashMap;

use lrgp::app_base::GameApp;
use lrgp::apps::dots_and_boxes::DotsAndBoxesApp;
use lrgp::apps::gomoku::GomokuApp;
use lrgp::apps::hex::HexApp;
use lrgp::apps::mancala::MancalaApp;
use lrgp::apps::reversi::ReversiApp;
use lrgp::constants::{CMD_ACCEPT, CMD_CHALLENGE, CMD_MOVE, STATUS_COMPLETED};
use lrgp::envelope::{Envelope, map_from_value, unpack_from_bytes, value_as_str, value_as_u64};

const SESSION_ID: &str = "0123456789abcdef";
const CHALLENGER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RESPONDER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

const ACCEPT_NONCE: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
const MOVE_NONCE: [u8; 8] = [2, 3, 4, 5, 6, 7, 8, 9];
const SWAP_NONCE: [u8; 8] = [3, 4, 5, 6, 7, 8, 9, 10];
const WIN_NONCE: [u8; 8] = [4, 5, 6, 7, 8, 9, 10, 11];
const DRAW_NONCE: [u8; 8] = [5, 6, 7, 8, 9, 10, 11, 12];

fn payload_of(bytes: &[u8], app: &str, command: &str, nonce: &[u8]) -> Envelope {
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

fn assert_exact_keys(payload: &Envelope, expected: &[&str]) {
    assert_eq!(payload.len(), expected.len());
    assert!(payload.keys().all(|key| expected.contains(&key.as_str())));
}

fn str_of<'a>(payload: &'a Envelope, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(value_as_str)
}

fn moves_of(text: &str) -> Vec<&str> {
    assert!(text.ends_with('\n') && !text.contains('\r'));
    text.lines().collect()
}

fn metadata(app: &dyn GameApp) -> serde_json::Map<String, serde_json::Value> {
    app.get_session_state(SESSION_ID, CHALLENGER)["metadata"]
        .as_object()
        .unwrap()
        .clone()
}

fn intent(notation: &str) -> HashMap<String, rmpv::Value> {
    HashMap::from([("m".to_string(), rmpv::Value::String(notation.into()))])
}

/// Replay `moves` from the challenger's side. The challenger's moves are
/// derived by this engine; the responder's arrive as wire payloads, with
/// `final_wire` used for a responder move that ends the game. Returns the
/// challenger's last outgoing payload, if the challenger moved last.
fn replay_from_challenger(
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

#[test]
fn hex_vectors() {
    // SHA-256 a84ac9e5dfdeed43eff70813bdcf8ba335cb0bc118fa87f47560c441c8af68e3
    let accept = include_bytes!("hex_accept.bin");
    assert_eq!(accept.len(), 88);
    let payload = payload_of(accept, "hex", "accept", &ACCEPT_NONCE);
    assert_exact_keys(&payload, &["b"]);
    assert_eq!(str_of(&payload, "b"), Some(CHALLENGER));

    // SHA-256 b9b3f7d027c2eaae5fe88c4147541ea04cb5f2f8c37129c62324d5fd2bc210e5
    let first = include_bytes!("hex_move.bin");
    assert_eq!(first.len(), 62);
    let payload = payload_of(first, "hex", "move", &MOVE_NONCE);
    assert_exact_keys(&payload, &["m", "n", "x"]);
    assert_eq!(str_of(&payload, "m"), Some("d11"));
    assert_eq!(value_as_u64(payload.get("n").unwrap()), Some(0));

    // SHA-256 5f30e1d322c4b0aae5879c8f5b9c28f1726f5a701783c6c7e9e1aaf6504dadc7
    let swap = include_bytes!("hex_swap.bin");
    assert_eq!(swap.len(), 63);
    let payload = payload_of(swap, "hex", "move", &SWAP_NONCE);
    assert_exact_keys(&payload, &["m", "n", "x"]);
    assert_eq!(str_of(&payload, "m"), Some("swap"));
    assert_eq!(value_as_u64(payload.get("n").unwrap()), Some(1));

    // SHA-256 42a854e2f5fdcebf18fefd2981d64d1a6224f44377b5af78bbac6bcaf682c60c
    let win = include_bytes!("hex_move_win.bin");
    assert_eq!(win.len(), 101);
    let win = payload_of(win, "hex", "move", &WIN_NONCE);
    assert_exact_keys(&win, &["m", "n", "x", "w"]);
    assert_eq!(str_of(&win, "x"), Some("win"));
    assert_eq!(str_of(&win, "w"), Some(RESPONDER));
}

#[test]
fn gomoku_vectors() {
    // SHA-256 18f9e8c1b3da95e93d608a3cbbd343878ce7bb090339b1fd13d03b96b292fc07
    let accept = include_bytes!("gomoku_accept.bin");
    assert_eq!(accept.len(), 91);
    let payload = payload_of(accept, "gomoku", "accept", &ACCEPT_NONCE);
    assert_exact_keys(&payload, &["b"]);

    // SHA-256 483efc467746aaea9dd94916a615975e88251c9487155e7211c5174aec4c5c95
    let first = include_bytes!("gomoku_move.bin");
    assert_eq!(first.len(), 65);
    let payload = payload_of(first, "gomoku", "move", &MOVE_NONCE);
    assert_exact_keys(&payload, &["m", "n", "x"]);

    // SHA-256 00c284b09a24c5624daae7f76369397c266f41f427799763707b43e3a3ef7fb7
    let win = include_bytes!("gomoku_move_win.bin");
    assert_eq!(win.len(), 103);
    let win = payload_of(win, "gomoku", "move", &WIN_NONCE);
    assert_exact_keys(&win, &["m", "n", "x", "w"]);
    assert_eq!(value_as_u64(win.get("n").unwrap()), Some(89));
    assert_eq!(str_of(&win, "w"), Some(RESPONDER));

    // SHA-256 c4fc709f7738291b6e109d20efeb0e555aabf4e644180537859826e761bcb2be
    let draw = include_bytes!("gomoku_move_draw.bin");
    assert_eq!(draw.len(), 70);
    let draw = payload_of(draw, "gomoku", "move", &DRAW_NONCE);
    assert_exact_keys(&draw, &["m", "n", "x"]);
    assert_eq!(str_of(&draw, "x"), Some("draw"));
    assert_eq!(value_as_u64(draw.get("n").unwrap()), Some(224));
}

#[test]
fn reversi_vectors() {
    // SHA-256 25658d4ef22060b7bbb5c08da4b1e07f8827ef87bdf5adc2713a1cd34a320742
    let accept = include_bytes!("reversi_accept.bin");
    assert_eq!(accept.len(), 92);
    let payload = payload_of(accept, "reversi", "accept", &ACCEPT_NONCE);
    assert_exact_keys(&payload, &["b"]);

    // SHA-256 c3fc956775a142132a85863146e0fd12bc26f14c345d05fcea86419d8bf32f9d
    let first = include_bytes!("reversi_move.bin");
    assert_eq!(first.len(), 65);
    let payload = payload_of(first, "reversi", "move", &MOVE_NONCE);
    assert_exact_keys(&payload, &["m", "n", "x"]);

    // SHA-256 c09e2a32f78823d3a4859b405ed49bdb18369107d583d77d173a15c326cf33d8
    let win = include_bytes!("reversi_move_win.bin");
    assert_eq!(win.len(), 104);
    let win = payload_of(win, "reversi", "move", &WIN_NONCE);
    assert_exact_keys(&win, &["m", "n", "x", "w"]);
    assert_eq!(value_as_u64(win.get("n").unwrap()), Some(59));
    assert_eq!(str_of(&win, "w"), Some(RESPONDER));
}

#[test]
fn hex_reference_game_matches_lrgp_py() {
    // SHA-256 163b3acc3f5a3a707b76419173024cd964064bba11e95e912d29791bb8a42328
    let moves = moves_of(include_str!("hex_reference.txt"));
    assert_eq!(moves.len(), 112);
    assert_eq!(moves[1], "swap");
    let win = payload_of(
        include_bytes!("hex_move_win.bin"),
        "hex",
        "move",
        &WIN_NONCE,
    );
    assert_eq!(str_of(&win, "m"), moves.last().copied());

    let app = HexApp::new();
    let last = replay_from_challenger(
        &app,
        "hex",
        include_bytes!("hex_accept.bin"),
        &moves,
        Some(113),
        &win,
    );
    assert!(last.is_none(), "White moves last in the Hex reference game");
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "con");
}

#[test]
fn gomoku_reference_game_matches_lrgp_py() {
    // SHA-256 109dcf71eb25b990a09a09228cc9aea92f8ae5963361f9bb3a396799ce09fb46
    let moves = moves_of(include_str!("gomoku_reference.txt"));
    assert_eq!(moves.len(), 90);
    let win = payload_of(
        include_bytes!("gomoku_move_win.bin"),
        "gomoku",
        "move",
        &WIN_NONCE,
    );
    assert_eq!(str_of(&win, "m"), moves.last().copied());

    let app = GomokuApp::new();
    let last = replay_from_challenger(
        &app,
        "gomoku",
        include_bytes!("gomoku_accept.bin"),
        &moves,
        Some(219),
        &win,
    );
    assert!(
        last.is_none(),
        "White moves last in the Gomoku reference game"
    );
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "5ir");
}

#[test]
fn gomoku_draw_game_matches_lrgp_py() {
    // SHA-256 dd6cf9f3185ab8957b2530b3db38cce1094600f12c2eb2f23dac946ff6ada145
    let moves = moves_of(include_str!("gomoku_draw.txt"));
    assert_eq!(moves.len(), 225);
    let draw = payload_of(
        include_bytes!("gomoku_move_draw.bin"),
        "gomoku",
        "move",
        &DRAW_NONCE,
    );

    let app = GomokuApp::new();
    let last = replay_from_challenger(
        &app,
        "gomoku",
        include_bytes!("gomoku_accept.bin"),
        &moves,
        None,
        &draw,
    );
    // Black (the challenger) fills the last square; this engine must derive
    // exactly lrgp-py's drawn payload.
    assert_eq!(last.as_ref(), Some(&draw));
    let meta = metadata(&app);
    assert_eq!(meta["terminal"], "draw");
    assert_eq!(meta["terminal_reason"], "ful");
}

#[test]
fn reversi_reference_game_matches_lrgp_py() {
    // SHA-256 453b645dfbab6fd940121e8daff91f29a6d8bef56ea3b1413c0bd07dc42476ec
    let moves = moves_of(include_str!("reversi_reference.txt"));
    assert_eq!(moves.len(), 60);
    let win = payload_of(
        include_bytes!("reversi_move_win.bin"),
        "reversi",
        "move",
        &WIN_NONCE,
    );

    let app = ReversiApp::new();
    let last = replay_from_challenger(
        &app,
        "reversi",
        include_bytes!("reversi_accept.bin"),
        &moves,
        Some(12),
        &win,
    );
    // Black (the challenger) moves last yet White wins on count, so this
    // engine must name the responder as winner exactly as lrgp-py does.
    assert_eq!(last.as_ref(), Some(&win));
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "cnt");
}

#[test]
fn dots_and_boxes_vectors() {
    // SHA-256 168789197b211ee2bd2a5a800857d1e92704ee1db789cc5d661474c812641aad
    let accept = include_bytes!("dots_and_boxes_accept.bin");
    assert_eq!(accept.len(), 99);
    let payload = payload_of(accept, "dots_and_boxes", "accept", &ACCEPT_NONCE);
    assert_exact_keys(&payload, &["b"]);

    // SHA-256 baca0b464ddc11bb06454dada0a80af31e7979c291c798b5c08d83fb8e12b17e
    let first = include_bytes!("dots_and_boxes_move.bin");
    assert_eq!(first.len(), 73);
    let payload = payload_of(first, "dots_and_boxes", "move", &MOVE_NONCE);
    assert_exact_keys(&payload, &["m", "n", "x"]);
    assert_eq!(str_of(&payload, "m"), Some("a1v"));

    // SHA-256 1f4434a8124e72b3a3775e77cb61dddabcd1caa277b387dc6eab5239fbdfbfad
    let win = include_bytes!("dots_and_boxes_move_win.bin");
    assert_eq!(win.len(), 112);
    let win = payload_of(win, "dots_and_boxes", "move", &WIN_NONCE);
    assert_exact_keys(&win, &["m", "n", "x", "w"]);
    assert_eq!(value_as_u64(win.get("n").unwrap()), Some(59));
    assert_eq!(str_of(&win, "w"), Some(RESPONDER));
}

#[test]
fn mancala_vectors() {
    // SHA-256 520effa5bfe5e3165a4c72aef4f74615f748c08b2e8add652b663d9a0adfffc2
    let accept = include_bytes!("mancala_accept.bin");
    assert_eq!(accept.len(), 92);
    let payload = payload_of(accept, "mancala", "accept", &ACCEPT_NONCE);
    assert_exact_keys(&payload, &["b"]);

    // SHA-256 ebf34d9dd6cf6b108f632fe265651a10c4576070a6fa06719089b156209a86f7
    let first = include_bytes!("mancala_move.bin");
    assert_eq!(first.len(), 64);
    let payload = payload_of(first, "mancala", "move", &MOVE_NONCE);
    assert_exact_keys(&payload, &["m", "n", "x"]);
    assert_eq!(str_of(&payload, "m"), Some("e"));

    // SHA-256 b8eae12cd558af9f23a5db345d6f147096d62eb40f9d3ee6f0b3e16fd909421f
    let win = include_bytes!("mancala_move_win.bin");
    assert_eq!(win.len(), 103);
    let win = payload_of(win, "mancala", "move", &WIN_NONCE);
    assert_exact_keys(&win, &["m", "n", "x", "w"]);
    assert_eq!(value_as_u64(win.get("n").unwrap()), Some(44));
    assert_eq!(str_of(&win, "w"), Some(RESPONDER));

    // SHA-256 07560a7ce377ecf8ae8793fbfac3eb6c94a444c462b87a2dddc634b7d5375f79
    let draw = include_bytes!("mancala_move_draw.bin");
    assert_eq!(draw.len(), 68);
    let draw = payload_of(draw, "mancala", "move", &DRAW_NONCE);
    assert_exact_keys(&draw, &["m", "n", "x"]);
    assert_eq!(str_of(&draw, "x"), Some("draw"));
    assert_eq!(value_as_u64(draw.get("n").unwrap()), Some(32));
}

#[test]
fn dots_and_boxes_reference_game_matches_lrgp_py() {
    // SHA-256 2865e9368a310f844aef9e2802c0bb6a261c5c7aea305be846a6549c2490b557
    let moves = moves_of(include_str!("dots_and_boxes_reference.txt"));
    assert_eq!(moves.len(), 60);
    let win = payload_of(
        include_bytes!("dots_and_boxes_move_win.bin"),
        "dots_and_boxes",
        "move",
        &WIN_NONCE,
    );

    let app = DotsAndBoxesApp::new();
    let last = replay_from_challenger(
        &app,
        "dots_and_boxes",
        include_bytes!("dots_and_boxes_accept.bin"),
        &moves,
        Some(1),
        &win,
    );
    // Black (the challenger) draws the last line yet White has more boxes.
    assert_eq!(last.as_ref(), Some(&win));
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "cnt");
    assert!(meta["white_boxes"].as_u64() > meta["black_boxes"].as_u64());
}

#[test]
fn mancala_reference_game_matches_lrgp_py() {
    // SHA-256 8a33c3d6305ec2ca8df58a2c1e9c435c6becd31b6f7e0a1bf8a7c8f9cf8a069a
    let moves = moves_of(include_str!("mancala_reference.txt"));
    assert_eq!(moves.len(), 45);
    let win = payload_of(
        include_bytes!("mancala_move_win.bin"),
        "mancala",
        "move",
        &WIN_NONCE,
    );

    let app = MancalaApp::new();
    let last = replay_from_challenger(
        &app,
        "mancala",
        include_bytes!("mancala_accept.bin"),
        &moves,
        Some(4),
        &win,
    );
    // Black (the challenger) empties its side, and White wins on the sweep.
    assert_eq!(last.as_ref(), Some(&win));
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "cnt");
}

#[test]
fn mancala_draw_game_matches_lrgp_py() {
    // SHA-256 f7a5bbdf0bfea348e4c05c598095768986d91b8b7616a84fbd8669994762cf15
    let moves = moves_of(include_str!("mancala_draw.txt"));
    assert_eq!(moves.len(), 33);
    let draw = payload_of(
        include_bytes!("mancala_move_draw.bin"),
        "mancala",
        "move",
        &DRAW_NONCE,
    );

    let app = MancalaApp::new();
    let last = replay_from_challenger(
        &app,
        "mancala",
        include_bytes!("mancala_accept.bin"),
        &moves,
        Some(3),
        &draw,
    );
    assert_eq!(last.as_ref(), Some(&draw));
    let meta = metadata(&app);
    assert_eq!(meta["terminal"], "draw");
    assert_eq!(meta["board"], "0,0,0,0,0,0,24,0,0,0,0,0,0,24");
}
