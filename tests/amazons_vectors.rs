//! Amazons vectors shared byte-for-byte with lrgp-py.

mod support;

use lrgp::apps::amazons::AmazonsApp;
use support::*;

#[test]
fn amazons_reference_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("amazons_reference.txt"));
    assert_eq!(moves.len(), 74);
    let accept = include_bytes!("amazons_accept.bin");
    let win = check_vectors(
        "amazons",
        accept,
        include_bytes!("amazons_move.bin"),
        include_bytes!("amazons_move_win.bin"),
        &moves,
    );

    let app = AmazonsApp::new();
    let last = replay_from_challenger(&app, "amazons", accept, &moves, Some(15), &win);
    assert!(last.is_none(), "White leaves Black without a move");
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "nm");
}
