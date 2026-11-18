//! Quoridor vectors shared byte-for-byte with lrgp-py.

mod support;

use lrgp::apps::quoridor::QuoridorApp;
use support::*;

#[test]
fn quoridor_reference_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("quoridor_reference.txt"));
    assert_eq!(moves.len(), 58);
    let accept = include_bytes!("quoridor_accept.bin");
    let win = check_vectors(
        "quoridor",
        accept,
        include_bytes!("quoridor_move.bin"),
        include_bytes!("quoridor_move_win.bin"),
        &moves,
    );

    let app = QuoridorApp::new();
    let last = replay_from_challenger(&app, "quoridor", accept, &moves, Some(63), &win);
    assert!(
        last.is_none(),
        "White's pawn reaches rank 1 with the last move"
    );
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "gol");
}

#[test]
fn quoridor_draw_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("quoridor_draw.txt"));
    assert_eq!(moves.len(), 37);
    let draw = check_draw("quoridor", include_bytes!("quoridor_move_draw.bin"), &moves);

    let app = QuoridorApp::new();
    let last = replay_from_challenger(
        &app,
        "quoridor",
        include_bytes!("quoridor_accept.bin"),
        &moves,
        Some(0),
        &draw,
    );
    // Black (the challenger) makes the drawing move, so this engine must
    // derive exactly lrgp-py's drawn payload.
    assert_eq!(last.as_ref(), Some(&draw));
    let meta = metadata(&app);
    assert_eq!(meta["terminal"], "draw");
    assert_eq!(meta["terminal_reason"], "3fr");
}
