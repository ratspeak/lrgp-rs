//! Simulates a full TicTacToe game through the LrgpRouter.

use std::collections::HashMap;

use lrgp::app_base::IncomingDispatch;
use lrgp::apps::tictactoe::TicTacToeApp;
use lrgp::protocol::value_as_str;
use lrgp::router::LrgpRouter;

fn main() {
    let router = LrgpRouter::new();
    router.register(Box::new(TicTacToeApp::new()));

    let player_a = "aaaa1111bbbb2222";
    let player_b = "cccc3333dddd4444";

    // Player A sends a challenge
    println!("=== Player A challenges Player B ===");
    let prepared = router
        .dispatch_outgoing_to(
            "ttt",
            1,
            "challenge",
            "",
            &HashMap::new(),
            player_a,
            player_b,
        )
        .unwrap();
    let env = prepared.envelope;
    let fallback = prepared.fallback_text;
    let session_id = value_as_str(env.get("s").unwrap()).unwrap().to_string();
    println!("Fallback: {fallback}");
    println!("Session ID: {session_id}");

    // Player B receives the challenge
    println!("\n=== Player B receives challenge ===");
    let result = router.dispatch_incoming(&env, player_a, player_b).unwrap();
    if let IncomingDispatch::Applied(result) = result {
        if let Some(emit) = &result.emit {
            println!("Event: {:?}", emit.get("type"));
        }
    }

    // Player B accepts
    println!("\n=== Player B accepts ===");
    let prepared = router
        .dispatch_outgoing_to(
            "ttt",
            1,
            "accept",
            &session_id,
            &HashMap::new(),
            player_b,
            player_a,
        )
        .unwrap();
    let accept_env = prepared.envelope;
    let fallback = prepared.fallback_text;
    println!("Fallback: {fallback}");

    // Player A receives accept
    let result = router
        .dispatch_incoming(&accept_env, player_b, player_a)
        .unwrap();
    if let IncomingDispatch::Applied(result) = result {
        if let Some(emit) = &result.emit {
            println!("Event: {:?}", emit.get("type"));
        }
    }

    // Play some moves
    let moves = [
        (player_a, 4),
        (player_b, 0),
        (player_a, 2),
        (player_b, 6),
        (player_a, 8),
    ];

    for (i, (player, cell)) in moves.iter().enumerate() {
        let move_num = i + 1;
        println!(
            "\n=== Move {move_num}: Player {} plays cell {cell} ===",
            if *player == player_a { "A" } else { "B" }
        );

        let mut payload = HashMap::new();
        payload.insert("i".to_string(), rmpv::Value::Integer((*cell as i64).into()));
        let other = if *player == player_a {
            player_b
        } else {
            player_a
        };

        let prepared = router
            .dispatch_outgoing_to("ttt", 1, "move", &session_id, &payload, player, other)
            .unwrap();
        let move_env = prepared.envelope;
        let fallback = prepared.fallback_text;
        println!("Fallback: {fallback}");

        // Other player receives
        let result = router.dispatch_incoming(&move_env, player, other).unwrap();

        if let IncomingDispatch::Applied(result) = result {
            if let Some(emit) = &result.emit {
                if let Some(payload_val) = emit.get("payload") {
                    if let Some(board_val) = payload_val.get("b") {
                        println!("Board: {}", board_val.as_str().unwrap_or("?"));
                    }
                }
            }
        }
    }

    // List registered games
    println!("\n=== Registered Games ===");
    for manifest in router.list_apps() {
        println!(
            "  {}.{} — {} ({})",
            manifest.app_id, manifest.version, manifest.display_name, manifest.session_type
        );
    }

    println!("\nGame simulation complete.");
}
