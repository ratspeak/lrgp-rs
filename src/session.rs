//! LRGP game session state machine and lifecycle.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::constants::*;
use crate::errors::LrgpError;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// An LRGP game session record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    pub identity_id: String,
    pub app_id: String,
    pub app_version: u32,
    pub contact_hash: String,
    pub initiator: String,
    pub status: String,
    pub metadata: HashMap<String, serde_json::Value>,
    pub unread: i64,
    pub created_at: f64,
    pub updated_at: f64,
    pub last_action_at: f64,
}

impl Session {
    pub fn new(session_id: impl Into<String>) -> Self {
        let now = now();
        Self {
            session_id: session_id.into(),
            identity_id: String::new(),
            app_id: String::new(),
            app_version: 1,
            contact_hash: String::new(),
            initiator: String::new(),
            status: STATUS_PENDING.to_string(),
            metadata: HashMap::new(),
            unread: 0,
            created_at: now,
            updated_at: now,
            last_action_at: now,
        }
    }
}

/// Enforces legal game session state transitions.
pub struct SessionStateMachine;

impl SessionStateMachine {
    /// Apply a command to a session, updating its status if appropriate.
    /// If `terminal` is true, the action ends the session (e.g., winning move).
    pub fn apply_command(
        session: &mut Session,
        command: &str,
        terminal: bool,
    ) -> Result<String, LrgpError> {
        let current = session.status.as_str();
        let t = now();

        // Check for explicit transitions
        if let Some(new_status) = Self::get_transition(current, command) {
            session.status = new_status.to_string();
            session.updated_at = t;
            session.last_action_at = t;
            return Ok(session.status.clone());
        }

        // Check for same-status commands
        if Self::is_same_status_command(current, command) {
            if terminal {
                session.status = STATUS_COMPLETED.to_string();
            }
            session.updated_at = t;
            session.last_action_at = t;
            return Ok(session.status.clone());
        }

        // Challenge creates a new session (pending)
        if command == CMD_CHALLENGE && current == STATUS_PENDING {
            session.updated_at = t;
            session.last_action_at = t;
            return Ok(session.status.clone());
        }

        Err(LrgpError::IllegalTransition {
            command: command.to_string(),
            status: current.to_string(),
        })
    }

    /// Check if a session has expired based on its TTL.
    /// Returns `true` if the session expired (and updates session.status).
    pub fn check_expiry(
        session: &mut Session,
        ttl: Option<&HashMap<String, f64>>,
        now_override: Option<f64>,
    ) -> bool {
        let status = session.status.as_str();
        if matches!(status, STATUS_COMPLETED | STATUS_EXPIRED | STATUS_DECLINED) {
            return false;
        }

        let t = now_override.unwrap_or_else(now);

        let limit = match status {
            STATUS_PENDING => ttl
                .and_then(|m| m.get(STATUS_PENDING).copied())
                .unwrap_or(TTL_PENDING),
            STATUS_ACTIVE => ttl
                .and_then(|m| m.get(STATUS_ACTIVE).copied())
                .unwrap_or(TTL_ACTIVE),
            _ => return false,
        };

        let deadline = session.last_action_at + limit + TTL_GRACE_PERIOD;
        if t > deadline {
            session.status = STATUS_EXPIRED.to_string();
            session.updated_at = t;
            return true;
        }

        false
    }

    fn get_transition(current: &str, command: &str) -> Option<&'static str> {
        match (current, command) {
            (STATUS_PENDING, CMD_ACCEPT) => Some(STATUS_ACTIVE),
            (STATUS_PENDING, CMD_DECLINE) => Some(STATUS_DECLINED),
            (STATUS_ACTIVE, CMD_RESIGN) => Some(STATUS_COMPLETED),
            (STATUS_ACTIVE, CMD_DRAW_ACCEPT) => Some(STATUS_COMPLETED),
            _ => None,
        }
    }

    fn is_same_status_command(current: &str, command: &str) -> bool {
        if current == STATUS_ACTIVE {
            matches!(
                command,
                CMD_MOVE | CMD_DRAW_OFFER | CMD_DRAW_DECLINE | CMD_ERROR
            )
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(status: &str) -> Session {
        let mut s = Session::new("test-session");
        s.status = status.to_string();
        s.last_action_at = now();
        s
    }

    #[test]
    fn test_pending_accept_to_active() {
        let mut s = make_session(STATUS_PENDING);
        let result = SessionStateMachine::apply_command(&mut s, CMD_ACCEPT, false).unwrap();
        assert_eq!(result, STATUS_ACTIVE);
    }

    #[test]
    fn test_pending_decline_to_declined() {
        let mut s = make_session(STATUS_PENDING);
        let result = SessionStateMachine::apply_command(&mut s, CMD_DECLINE, false).unwrap();
        assert_eq!(result, STATUS_DECLINED);
    }

    #[test]
    fn test_active_resign_to_completed() {
        let mut s = make_session(STATUS_ACTIVE);
        let result = SessionStateMachine::apply_command(&mut s, CMD_RESIGN, false).unwrap();
        assert_eq!(result, STATUS_COMPLETED);
    }

    #[test]
    fn test_active_draw_accept_to_completed() {
        let mut s = make_session(STATUS_ACTIVE);
        let result = SessionStateMachine::apply_command(&mut s, CMD_DRAW_ACCEPT, false).unwrap();
        assert_eq!(result, STATUS_COMPLETED);
    }

    #[test]
    fn test_active_move_stays_active() {
        let mut s = make_session(STATUS_ACTIVE);
        let result = SessionStateMachine::apply_command(&mut s, CMD_MOVE, false).unwrap();
        assert_eq!(result, STATUS_ACTIVE);
    }

    #[test]
    fn test_active_move_terminal_completes() {
        let mut s = make_session(STATUS_ACTIVE);
        let result = SessionStateMachine::apply_command(&mut s, CMD_MOVE, true).unwrap();
        assert_eq!(result, STATUS_COMPLETED);
    }

    #[test]
    fn test_challenge_on_pending_stays_pending() {
        let mut s = make_session(STATUS_PENDING);
        let result = SessionStateMachine::apply_command(&mut s, CMD_CHALLENGE, false).unwrap();
        assert_eq!(result, STATUS_PENDING);
    }

    #[test]
    fn test_illegal_transition() {
        let mut s = make_session(STATUS_COMPLETED);
        let result = SessionStateMachine::apply_command(&mut s, CMD_MOVE, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_check_expiry_pending() {
        let mut s = make_session(STATUS_PENDING);
        s.last_action_at = 1000.0; // Far in the past
        let expired = SessionStateMachine::check_expiry(&mut s, None, Some(1_000_000.0));
        assert!(expired);
        assert_eq!(s.status, STATUS_EXPIRED);
    }

    #[test]
    fn test_check_expiry_active_not_expired() {
        let mut s = make_session(STATUS_ACTIVE);
        let t = now();
        s.last_action_at = t;
        let expired = SessionStateMachine::check_expiry(&mut s, None, Some(t + 100.0));
        assert!(!expired);
        assert_eq!(s.status, STATUS_ACTIVE);
    }

    #[test]
    fn test_check_expiry_completed_ignored() {
        let mut s = make_session(STATUS_COMPLETED);
        s.last_action_at = 0.0;
        let expired = SessionStateMachine::check_expiry(&mut s, None, Some(1_000_000.0));
        assert!(!expired);
    }

    #[test]
    fn test_check_expiry_custom_ttl() {
        let mut s = make_session(STATUS_PENDING);
        s.last_action_at = 1000.0;
        let mut ttl = HashMap::new();
        ttl.insert(STATUS_PENDING.to_string(), 100.0);
        // now = 1000 + 100 + 3600(grace) + 1 = 4701
        let expired = SessionStateMachine::check_expiry(&mut s, Some(&ttl), Some(4701.0));
        assert!(expired);
    }
}
