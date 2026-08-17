//! Demonstrates Chess envelope packing over the LrgpRouter.
//!
//! Walks challenge -> accept -> White's first move (1.e4) and prints each
//! envelope's fallback text + size. A full game requires a richer two-peer
//! state-sync harness than this single-process demo provides; the binary
//! test vectors in `tests/chess_*.bin` are the canonical wire fixture.
//!
//! Run with `--features test-helpers` to pin the coin flip so White is
//! always the challenger.

use std::collections::HashMap;

use lrgp::app_base::IncomingDispatch;
use lrgp::apps::chess::ChessApp;
#[cfg(feature = "test-helpers")]
use lrgp::apps::chess::force_coin;
use lrgp::protocol::{pack_to_bytes, value_as_str};
use lrgp::router::LrgpRouter;

fn router() -> LrgpRouter {
    let r = LrgpRouter::new();
    r.register(Box::new(ChessApp::new()));
    r
}

fn main() {
    #[cfg(feature = "test-helpers")]
    force_coin(Some(true));

    let router_a = router();
    let router_b = router();

    let player_a = "aaaa1111bbbb2222";
    let player_b = "cccc3333dddd4444";

    println!("=== A challenges B to Chess ===");
    let prepared = router_a
        .dispatch_outgoing_to(
            "chess",
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
    let bytes = pack_to_bytes(&env).unwrap();
    println!("Fallback: {fallback}");
    println!("Session ID: {session_id}");
    println!("Envelope: {} bytes", bytes.len());

    println!("\n=== B receives challenge ===");
    router_b
        .dispatch_incoming(&env, player_a, player_b)
        .unwrap();

    println!("\n=== B accepts ===");
    let prepared = router_b
        .dispatch_outgoing_to(
            "chess",
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
    let bytes = pack_to_bytes(&accept_env).unwrap();
    println!("Fallback: {fallback}");
    println!("Envelope: {} bytes", bytes.len());
    router_a
        .dispatch_incoming(&accept_env, player_b, player_a)
        .unwrap();

    println!("\n=== A plays 1.e4 ===");
    let mut payload = HashMap::new();
    payload.insert("m".to_string(), rmpv::Value::String("e2e4".into()));
    let prepared = router_a
        .dispatch_outgoing_to(
            "chess",
            1,
            "move",
            &session_id,
            &payload,
            player_a,
            player_b,
        )
        .unwrap();
    let move_env = prepared.envelope;
    let fallback = prepared.fallback_text;
    let bytes = pack_to_bytes(&move_env).unwrap();
    println!("Fallback: {fallback}");
    println!("Envelope: {} bytes", bytes.len());

    let result = router_b
        .dispatch_incoming(&move_env, player_a, player_b)
        .unwrap();
    if let IncomingDispatch::Applied(result) = result {
        if let Some(emit) = &result.emit {
            if let Some(ev_type) = emit.get("type") {
                println!("B inbound event: {ev_type:?}");
            }
        }
    }

    println!("\n=== Registered Games ===");
    for manifest in router_a.list_apps() {
        println!(
            "  {}.{} — {} ({})",
            manifest.app_id, manifest.version, manifest.display_name, manifest.session_type
        );
    }
}
