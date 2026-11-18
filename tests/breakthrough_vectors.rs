//! Breakthrough vectors shared byte-for-byte with lrgp-py.

mod support;

use lrgp::apps::breakthrough::BreakthroughApp;
use support::*;

#[test]
fn breakthrough_reference_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("breakthrough_reference.txt"));
    assert_eq!(moves.len(), 100);
    let accept = include_bytes!("breakthrough_accept.bin");
    let win = check_vectors(
        "breakthrough",
        accept,
        include_bytes!("breakthrough_move.bin"),
        include_bytes!("breakthrough_move_win.bin"),
        &moves,
    );

    let app = BreakthroughApp::new();
    let last = replay_from_challenger(&app, "breakthrough", accept, &moves, Some(18), &win);
    assert!(last.is_none(), "White reaches rank 1 with the last move");
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "gol");
}
