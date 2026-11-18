//! Go vectors shared byte-for-byte with lrgp-py.

mod support;

use lrgp::apps::go::GoApp;
use support::*;

#[test]
fn go_reference_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("go_reference.txt"));
    assert_eq!(moves.len(), 135);
    assert_eq!(moves[moves.len() - 2..], ["pass", "pass"]);
    let accept = include_bytes!("go_accept.bin");
    let win = check_vectors(
        "go",
        accept,
        include_bytes!("go_move.bin"),
        include_bytes!("go_move_win.bin"),
        &moves,
    );

    let app = GoApp::new();
    let last = replay_from_challenger(&app, "go", accept, &moves, Some(12), &win);
    // Black (the challenger) passes last and holds more area, yet White wins
    // on komi, so this engine must name the responder exactly as lrgp-py does.
    assert_eq!(last.as_ref(), Some(&win));
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "cnt");
    assert_eq!(
        (meta["black_count"].as_u64(), meta["white_count"].as_u64()),
        (Some(42), Some(39))
    );
}
