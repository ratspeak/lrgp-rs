//! Ultimate Tic-Tac-Toe vectors shared byte-for-byte with lrgp-py.

mod support;

use lrgp::apps::ultimate_ttt::UltimateTicTacToeApp;
use support::*;

#[test]
fn ultimate_ttt_reference_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("ultimate_ttt_reference.txt"));
    assert_eq!(moves.len(), 46);
    let accept = include_bytes!("ultimate_ttt_accept.bin");
    let win = check_vectors(
        "ultimate_ttt",
        accept,
        include_bytes!("ultimate_ttt_move.bin"),
        include_bytes!("ultimate_ttt_move_win.bin"),
        &moves,
    );

    let app = UltimateTicTacToeApp::new();
    let last = replay_from_challenger(&app, "ultimate_ttt", accept, &moves, Some(4), &win);
    assert!(last.is_none(), "White wins with the last move");
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "3ib");
}

#[test]
fn ultimate_ttt_draw_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("ultimate_ttt_draw.txt"));
    assert_eq!(moves.len(), 59);
    let draw = check_draw(
        "ultimate_ttt",
        include_bytes!("ultimate_ttt_move_draw.bin"),
        &moves,
    );

    let app = UltimateTicTacToeApp::new();
    let last = replay_from_challenger(
        &app,
        "ultimate_ttt",
        include_bytes!("ultimate_ttt_accept.bin"),
        &moves,
        Some(19),
        &draw,
    );
    // Black (the challenger) finishes the last small board, so this engine
    // must derive exactly lrgp-py's drawn payload.
    assert_eq!(last.as_ref(), Some(&draw));
    let meta = metadata(&app);
    assert_eq!(meta["terminal"], "draw");
    assert_eq!(meta["terminal_reason"], "ful");
    assert!(!meta["boards"].as_str().unwrap().contains('_'));
}
