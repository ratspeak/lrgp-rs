//! Demonstrates the LRGP game session state machine.

use lrgp::protocol::{CMD_ACCEPT, CMD_CHALLENGE, CMD_DECLINE, CMD_MOVE};
use lrgp::session::{Session, SessionStateMachine};

fn main() {
    // Create a new session (starts in "pending" state)
    let mut session = Session::new("demo-session-001");
    session.app_id = "ttt".to_string();
    session.contact_hash = "abcdef0123456789".to_string();
    session.initiator = "abcdef0123456789".to_string();
    println!("Created session: status={}", session.status);

    // Challenge on a pending session stays pending
    let status = SessionStateMachine::apply_command(&mut session, CMD_CHALLENGE, false).unwrap();
    println!("After challenge: status={status}");

    // Accept transitions pending -> active
    let status = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false).unwrap();
    println!("After accept: status={status}");

    // Move keeps session active (non-terminal)
    let status = SessionStateMachine::apply_command(&mut session, CMD_MOVE, false).unwrap();
    println!("After move (non-terminal): status={status}");

    // Another move, still active
    let status = SessionStateMachine::apply_command(&mut session, CMD_MOVE, false).unwrap();
    println!("After move (non-terminal): status={status}");

    // Terminal move completes the session
    let status = SessionStateMachine::apply_command(&mut session, CMD_MOVE, true).unwrap();
    println!("After move (terminal): status={status}");

    // Trying to move on a completed session fails
    let result = SessionStateMachine::apply_command(&mut session, CMD_MOVE, false);
    println!("Move on completed session: {result:?}");

    // Demonstrate expiry
    println!("\n--- Expiry demo ---");
    let mut pending = Session::new("expiry-demo");
    pending.last_action_at = 1000.0; // far in the past
    let expired = SessionStateMachine::check_expiry(&mut pending, None, Some(1_000_000.0));
    println!(
        "Pending session expired: {expired} (status={})",
        pending.status
    );

    // Demonstrate decline
    println!("\n--- Decline demo ---");
    let mut challenged = Session::new("decline-demo");
    let status = SessionStateMachine::apply_command(&mut challenged, CMD_DECLINE, false).unwrap();
    println!("After decline: status={status}");

    println!("\nAll lifecycle transitions demonstrated.");
}
