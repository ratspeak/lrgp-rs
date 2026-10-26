//! Nine Men's Morris vectors shared byte-for-byte with lrgp-py.

mod support;

use lrgp::apps::morris::MorrisApp;
use support::*;

#[test]
fn morris_reference_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("morris_reference.txt"));
    assert_eq!(moves.len(), 90);
    let accept = include_bytes!("morris_accept.bin");
    let win = check_vectors(
        "morris",
        accept,
        include_bytes!("morris_move.bin"),
        include_bytes!("morris_move_win.bin"),
        &moves,
    );

    let app = MorrisApp::new();
    let last = replay_from_challenger(&app, "morris", accept, &moves, Some(30), &win);
    assert!(
        last.is_none(),
        "White takes Black's third-last piece with the last move"
    );
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "np");
}

#[test]
fn morris_draw_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("morris_draw.txt"));
    assert_eq!(moves.len(), 204);
    let draw = check_draw("morris", include_bytes!("morris_move_draw.bin"), &moves);

    let app = MorrisApp::new();
    let last = replay_from_challenger(
        &app,
        "morris",
        include_bytes!("morris_accept.bin"),
        &moves,
        Some(22),
        &draw,
    );
    assert!(last.is_none(), "White makes the drawing move");
    let meta = metadata(&app);
    assert_eq!(meta["terminal"], "draw");
    assert_eq!(meta["terminal_reason"], "50m");
}
