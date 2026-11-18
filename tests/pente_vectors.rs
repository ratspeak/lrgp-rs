//! Pente vectors shared byte-for-byte with lrgp-py.

mod support;

use lrgp::apps::pente::PenteApp;
use support::*;

#[test]
fn pente_reference_game_matches_lrgp_py() {
    let moves = moves_of(include_str!("pente_reference.txt"));
    assert_eq!(moves.len(), 218);
    let accept = include_bytes!("pente_accept.bin");
    let win = check_vectors(
        "pente",
        accept,
        include_bytes!("pente_move.bin"),
        include_bytes!("pente_move_win.bin"),
        &moves,
    );

    let app = PenteApp::new();
    let last = replay_from_challenger(&app, "pente", accept, &moves, Some(13), &win);
    assert!(
        last.is_none(),
        "White's fifth captured pair wins with the last move"
    );
    let meta = metadata(&app);
    assert_eq!(meta["winner"], RESPONDER);
    assert_eq!(meta["terminal_reason"], "cap");
}
