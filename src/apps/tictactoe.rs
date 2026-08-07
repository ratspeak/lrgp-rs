//! LRGP TicTacToe — built-in turn-based game with both-side validation.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value as JsonValue;

use crate::app_base::{AppManifest, GameApp, IncomingResult, OutgoingResult};
use crate::constants::*;
use crate::envelope::{has_exact_keys, value_as_str, value_as_u64};
use crate::errors::LrgpError;
use crate::session::{Session, SessionStateMachine};

const EMPTY_BOARD: &str = "_________";

const WIN_LINES: [(usize, usize, usize); 8] = [
    (0, 1, 2),
    (3, 4, 5),
    (6, 7, 8), // rows
    (0, 3, 6),
    (1, 4, 7),
    (2, 5, 8), // columns
    (0, 4, 8),
    (2, 4, 6), // diagonals
];

fn check_winner(board: &str) -> Option<char> {
    let b: Vec<char> = board.chars().collect();
    if b.len() != EMPTY_BOARD.len() {
        return None;
    }
    for &(a, bi, c) in &WIN_LINES {
        if b[a] != '_' && b[a] == b[bi] && b[bi] == b[c] {
            return Some(b[a]);
        }
    }
    None
}

fn check_draw(board: &str) -> bool {
    board.len() == EMPTY_BOARD.len()
        && board.bytes().all(|cell| matches!(cell, b'X' | b'O'))
        && check_winner(board).is_none()
}

fn marker_for_move(move_num: u64) -> char {
    if move_num % 2 == 1 { 'X' } else { 'O' }
}

fn gen_session_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn error_result(code: &str, msg: &str) -> IncomingResult {
    let mut err = HashMap::new();
    err.insert("code".into(), JsonValue::String(code.into()));
    err.insert("msg".into(), JsonValue::String(msg.into()));
    IncomingResult {
        session: None,
        emit: None,
        error: Some(err),
    }
}

fn emit_event(
    event_type: &str,
    session_id: &str,
    app_id: &str,
    from: &str,
) -> HashMap<String, JsonValue> {
    let mut m = HashMap::new();
    m.insert("type".into(), JsonValue::String(event_type.into()));
    m.insert("session_id".into(), JsonValue::String(session_id.into()));
    m.insert("app_id".into(), JsonValue::String(app_id.into()));
    m.insert("from".into(), JsonValue::String(from.into()));
    m
}

/// Helper to get a string from metadata.
fn meta_str(meta: &HashMap<String, JsonValue>, key: &str) -> String {
    meta.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn meta_i64(meta: &HashMap<String, JsonValue>, key: &str) -> i64 {
    meta.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

/// The Tic-Tac-Toe LRGP game.
pub struct TicTacToeApp {
    sessions: Mutex<HashMap<(String, String), Session>>,
}

impl TicTacToeApp {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn get_session(&self, session_id: &str, identity_id: &str) -> Option<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(&(session_id.to_string(), identity_id.to_string()))?;
        SessionStateMachine::check_expiry(session, Some(&Self::ttl_policy()), None);
        Some(session.clone())
    }

    fn save_session(&self, session: &Session) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(
            (session.session_id.clone(), session.identity_id.clone()),
            session.clone(),
        );
    }

    fn ttl_policy() -> HashMap<String, f64> {
        let mut ttl = HashMap::new();
        ttl.insert(STATUS_PENDING.into(), 86400.0);
        ttl.insert(STATUS_ACTIVE.into(), 86400.0);
        ttl
    }

    fn default_metadata(my_marker: &str, first_turn: &str) -> HashMap<String, JsonValue> {
        let mut m = HashMap::new();
        m.insert("board".into(), JsonValue::String(EMPTY_BOARD.into()));
        m.insert("turn".into(), JsonValue::String("".into()));
        m.insert("first_turn".into(), JsonValue::String(first_turn.into()));
        m.insert("my_marker".into(), JsonValue::String(my_marker.into()));
        m.insert("move_count".into(), JsonValue::Number(0.into()));
        m.insert("winner".into(), JsonValue::String("".into()));
        m.insert("terminal".into(), JsonValue::String("".into()));
        m.insert("draw_offered".into(), JsonValue::Bool(false));
        m.insert("draw_offered_by".into(), JsonValue::String("".into()));
        m
    }

    fn validate_incoming_payload(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> Result<(), String> {
        match command {
            CMD_CHALLENGE | CMD_DECLINE | CMD_RESIGN | CMD_DRAW_OFFER | CMD_DRAW_ACCEPT
            | CMD_DRAW_DECLINE => {
                if !payload.is_empty() {
                    return Err(format!("{command} payload must be empty"));
                }
            }
            CMD_ACCEPT => {
                if !has_exact_keys(payload, &["b", "t"]) {
                    return Err("accept payload must contain exactly b and t".into());
                }
                let board = payload.get("b").and_then(value_as_str).unwrap_or("");
                let turn = payload.get("t").and_then(value_as_str).unwrap_or("");
                let expected_turn = self
                    .get_session(session_id, identity_id)
                    .map(|session| meta_str(&session.metadata, "first_turn"))
                    .unwrap_or_default();
                if board != EMPTY_BOARD {
                    return Err("accept board must be empty".into());
                }
                if expected_turn.is_empty() || turn != expected_turn {
                    return Err("accept first turn does not match challenge".into());
                }
            }
            CMD_MOVE => {
                let terminal = payload
                    .get("x")
                    .and_then(value_as_str)
                    .ok_or_else(|| "move terminal marker must be a string".to_string())?;
                let expected: &[&str] = if terminal == "win" {
                    &["i", "b", "n", "t", "x", "w"]
                } else {
                    &["i", "b", "n", "t", "x"]
                };
                if !has_exact_keys(payload, expected) {
                    return Err(format!(
                        "move payload has invalid keys for terminal marker '{terminal}'"
                    ));
                }
                if !matches!(terminal, "" | "win" | "draw") {
                    return Err(format!("unsupported terminal marker '{terminal}'"));
                }
                if payload.get("i").and_then(value_as_u64).is_none()
                    || payload.get("n").and_then(value_as_u64).is_none()
                    || payload.get("b").and_then(value_as_str).is_none()
                    || payload.get("t").and_then(value_as_str).is_none()
                    || (terminal == "win" && payload.get("w").and_then(value_as_str).is_none())
                {
                    return Err("move payload contains a value with the wrong type".into());
                }
            }
            CMD_ERROR => {}
            _ => return Err(format!("unsupported command '{command}'")),
        }
        Ok(())
    }

    // --- Incoming handlers ---

    fn handle_challenge_in(
        &self,
        session_id: &str,
        _payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        if let Some(existing) = self.get_session(session_id, identity_id) {
            return IncomingResult {
                session: Some(session_to_json(&existing)),
                emit: None,
                error: None,
            };
        }
        let mut session = Session::new(session_id);
        session.identity_id = identity_id.to_string();
        session.app_id = "ttt".to_string();
        session.app_version = 1;
        session.contact_hash = sender_hash.to_string();
        session.initiator = sender_hash.to_string();
        session.status = STATUS_PENDING.to_string();
        session.metadata = Self::default_metadata("O", sender_hash);
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("challenge", session_id, "ttt", sender_hash)),
            error: None,
        }
    }

    fn handle_accept_in(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => return error_result(ERR_PROTOCOL_ERROR, "Unknown session"),
        };

        if session.contact_hash.is_empty() {
            session.contact_hash = sender_hash.to_string();
        }

        let first_turn = meta_str(&session.metadata, "first_turn");
        let board = payload.get("b").and_then(value_as_str).unwrap_or("");
        let turn = payload.get("t").and_then(value_as_str).unwrap_or("");
        if board != EMPTY_BOARD {
            return error_result(ERR_PROTOCOL_ERROR, "ACCEPT must contain an empty board");
        }
        if first_turn.is_empty() || turn != first_turn {
            return error_result(
                ERR_PROTOCOL_ERROR,
                "ACCEPT first turn does not match challenge",
            );
        }
        if let Err(e) = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false) {
            return error_result(ERR_PROTOCOL_ERROR, &e.to_string());
        }

        session
            .metadata
            .insert("board".into(), JsonValue::String(board.to_string()));
        session
            .metadata
            .insert("turn".into(), JsonValue::String(turn.to_string()));
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("accept", session_id, "ttt", sender_hash)),
            error: None,
        }
    }

    fn handle_decline_in(
        &self,
        session_id: &str,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => return error_result(ERR_PROTOCOL_ERROR, "Unknown session"),
        };

        if let Err(e) = SessionStateMachine::apply_command(&mut session, CMD_DECLINE, false) {
            return error_result(ERR_PROTOCOL_ERROR, &e.to_string());
        }

        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("decline", session_id, "ttt", sender_hash)),
            error: None,
        }
    }

    fn handle_move_in(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => return error_result(ERR_PROTOCOL_ERROR, "Unknown session"),
        };

        let (valid, err_msg) = self.validate_move(&session, payload, sender_hash);
        if !valid {
            return IncomingResult {
                session: Some(session_to_json(&session)),
                emit: None,
                error: Some({
                    let mut m = HashMap::new();
                    m.insert("code".into(), JsonValue::String(ERR_INVALID_MOVE.into()));
                    m.insert("msg".into(), JsonValue::String(err_msg.unwrap_or_default()));
                    m.insert("ref".into(), JsonValue::String(CMD_MOVE.into()));
                    m
                }),
            };
        }

        let board = payload.get("b").and_then(value_as_str).unwrap_or("");
        let move_num = payload.get("n").and_then(value_as_u64).unwrap_or(0);
        let turn = payload.get("t").and_then(value_as_str).unwrap_or("");
        let terminal = payload.get("x").and_then(value_as_str).unwrap_or("");
        let winner = payload.get("w").and_then(value_as_str).unwrap_or("");

        session
            .metadata
            .insert("board".into(), JsonValue::String(board.to_string()));
        session.metadata.insert(
            "move_count".into(),
            JsonValue::Number((move_num as i64).into()),
        );
        session
            .metadata
            .insert("turn".into(), JsonValue::String(turn.to_string()));
        session
            .metadata
            .insert("terminal".into(), JsonValue::String(terminal.to_string()));
        session
            .metadata
            .insert("winner".into(), JsonValue::String(winner.to_string()));
        session.clear_draw_offer();

        if let Err(error) =
            SessionStateMachine::apply_command(&mut session, CMD_MOVE, !terminal.is_empty())
        {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        session.unread = 1;
        self.save_session(&session);

        let mut emit = emit_event("move", session_id, "ttt", sender_hash);
        // Include payload in emit for moves
        let payload_json: HashMap<String, JsonValue> = payload
            .iter()
            .map(|(k, v)| (k.clone(), rmpv_to_json(v)))
            .collect();
        emit.insert(
            "payload".into(),
            JsonValue::Object(payload_json.into_iter().collect()),
        );

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit),
            error: None,
        }
    }

    fn handle_resign_in(
        &self,
        session_id: &str,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => return error_result(ERR_PROTOCOL_ERROR, "Unknown session"),
        };

        if let Err(error) = SessionStateMachine::apply_command(&mut session, CMD_RESIGN, false) {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        session
            .metadata
            .insert("terminal".into(), JsonValue::String("resign".into()));
        let first_turn = meta_str(&session.metadata, "first_turn");
        let winner = if sender_hash == first_turn {
            identity_id.to_string()
        } else {
            first_turn
        };
        session
            .metadata
            .insert("winner".into(), JsonValue::String(winner));
        session.clear_draw_offer();
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("resign", session_id, "ttt", sender_hash)),
            error: None,
        }
    }

    fn handle_draw_offer_in(
        &self,
        session_id: &str,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => return error_result(ERR_PROTOCOL_ERROR, "Unknown session"),
        };

        if session.has_draw_offer() {
            return error_result(ERR_PROTOCOL_ERROR, "A draw offer is already outstanding");
        }
        if let Err(error) = SessionStateMachine::apply_command(&mut session, CMD_DRAW_OFFER, false)
        {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        session.set_draw_offer(sender_hash);
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("draw_offer", session_id, "ttt", sender_hash)),
            error: None,
        }
    }

    fn handle_draw_accept_in(
        &self,
        session_id: &str,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => return error_result(ERR_PROTOCOL_ERROR, "Unknown session"),
        };

        let Some(offered_by) = session.draw_offered_by() else {
            return error_result(ERR_PROTOCOL_ERROR, "No draw offer is outstanding");
        };
        if offered_by == sender_hash {
            return error_result(
                ERR_PROTOCOL_ERROR,
                "A participant cannot accept its own draw offer",
            );
        }

        if let Err(error) = SessionStateMachine::apply_command(&mut session, CMD_DRAW_ACCEPT, false)
        {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        session
            .metadata
            .insert("terminal".into(), JsonValue::String("draw".into()));
        session.clear_draw_offer();
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("draw_accept", session_id, "ttt", sender_hash)),
            error: None,
        }
    }

    fn handle_draw_decline_in(
        &self,
        session_id: &str,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => return error_result(ERR_PROTOCOL_ERROR, "Unknown session"),
        };

        let Some(offered_by) = session.draw_offered_by() else {
            return error_result(ERR_PROTOCOL_ERROR, "No draw offer is outstanding");
        };
        if offered_by == sender_hash {
            return error_result(
                ERR_PROTOCOL_ERROR,
                "A participant cannot decline its own draw offer",
            );
        }

        if let Err(error) =
            SessionStateMachine::apply_command(&mut session, CMD_DRAW_DECLINE, false)
        {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        session.clear_draw_offer();
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("draw_decline", session_id, "ttt", sender_hash)),
            error: None,
        }
    }

    // --- Outgoing handlers ---

    fn handle_challenge_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        let sid = if session_id.is_empty() {
            gen_session_id()
        } else {
            session_id.to_string()
        };

        let mut session = Session::new(&sid);
        session.identity_id = identity_id.to_string();
        session.app_id = "ttt".to_string();
        session.app_version = 1;
        session.initiator = identity_id.to_string();
        session.status = STATUS_PENDING.to_string();
        session.metadata = Self::default_metadata("X", identity_id);
        self.save_session(&session);

        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP TTT] Sent a challenge!".into(),
        }
    }

    fn handle_accept_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => {
                return OutgoingResult {
                    payload: HashMap::new(),
                    fallback_text: "[LRGP TTT] Challenge accepted".into(),
                };
            }
        };

        let _ = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false);
        let first_turn = meta_str(&session.metadata, "first_turn");
        let first = if first_turn.is_empty() {
            session.initiator.clone()
        } else {
            first_turn
        };
        session
            .metadata
            .insert("board".into(), JsonValue::String(EMPTY_BOARD.into()));
        session
            .metadata
            .insert("turn".into(), JsonValue::String(first.clone()));
        self.save_session(&session);

        let mut payload = HashMap::new();
        payload.insert("b".to_string(), rmpv::Value::String(EMPTY_BOARD.into()));
        payload.insert("t".to_string(), rmpv::Value::String(first.into()));

        OutgoingResult {
            payload,
            fallback_text: "[LRGP TTT] Challenge accepted".into(),
        }
    }

    fn handle_decline_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            let _ = SessionStateMachine::apply_command(&mut session, CMD_DECLINE, false);
            self.save_session(&session);
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP TTT] Challenge declined".into(),
        }
    }

    fn handle_move_out(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> OutgoingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => {
                return OutgoingResult {
                    payload: HashMap::new(),
                    fallback_text: "[LRGP TTT] Session not found".into(),
                };
            }
        };

        let meta = &session.metadata;
        if session.status != STATUS_ACTIVE {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: format!("[LRGP TTT] Session is not active ({})", session.status),
            };
        }

        let current_turn = meta_str(meta, "turn");
        if current_turn != identity_id {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP TTT] Not your turn".into(),
            };
        }

        let old_board = meta_str(meta, "board");
        let index = match payload.get("i").and_then(value_as_u64) {
            Some(i) if i <= 8 => i as usize,
            _ => {
                return OutgoingResult {
                    payload: HashMap::new(),
                    fallback_text: "[LRGP TTT] Invalid cell index".into(),
                };
            }
        };
        let old_chars: Vec<char> = old_board.chars().collect();
        if index >= old_chars.len() || old_chars[index] != '_' {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: format!("[LRGP TTT] Cell {index} is already occupied"),
            };
        }

        let move_num = (meta_i64(meta, "move_count") + 1) as u64;
        let marker = marker_for_move(move_num);

        let mut board_chars = old_chars;
        board_chars[index] = marker;
        let new_board: String = board_chars.into_iter().collect();

        let winner = check_winner(&new_board);
        let is_draw = check_draw(&new_board);

        let (terminal, winner_hash, next_turn) = if winner.is_some() {
            ("win".to_string(), identity_id.to_string(), String::new())
        } else if is_draw {
            ("draw".to_string(), String::new(), String::new())
        } else {
            let first_turn = meta_str(meta, "first_turn");
            let nt = if identity_id == first_turn {
                session.contact_hash.clone()
            } else {
                first_turn
            };
            if nt.is_empty() {
                return OutgoingResult {
                    payload: HashMap::new(),
                    fallback_text: "[LRGP TTT] Opponent unknown".into(),
                };
            }
            (String::new(), String::new(), nt)
        };

        let mut enriched = HashMap::new();
        enriched.insert("i".to_string(), rmpv::Value::Integer((index as i64).into()));
        enriched.insert(
            "b".to_string(),
            rmpv::Value::String(new_board.clone().into()),
        );
        enriched.insert(
            "n".to_string(),
            rmpv::Value::Integer((move_num as i64).into()),
        );
        enriched.insert(
            "t".to_string(),
            rmpv::Value::String(next_turn.clone().into()),
        );
        enriched.insert(
            "x".to_string(),
            rmpv::Value::String(terminal.clone().into()),
        );
        if terminal == "win" {
            enriched.insert(
                "w".to_string(),
                rmpv::Value::String(winner_hash.clone().into()),
            );
        }

        // Update local session
        session
            .metadata
            .insert("board".into(), JsonValue::String(new_board));
        session.metadata.insert(
            "move_count".into(),
            JsonValue::Number((move_num as i64).into()),
        );
        session
            .metadata
            .insert("turn".into(), JsonValue::String(next_turn));
        session
            .metadata
            .insert("terminal".into(), JsonValue::String(terminal.clone()));
        session.metadata.insert(
            "winner".into(),
            JsonValue::String(if terminal == "win" {
                winner_hash
            } else {
                String::new()
            }),
        );
        session.clear_draw_offer();
        let _ = SessionStateMachine::apply_command(&mut session, CMD_MOVE, !terminal.is_empty());
        self.save_session(&session);

        let fallback = self.render_fallback_inner(CMD_MOVE, &enriched);
        OutgoingResult {
            payload: enriched,
            fallback_text: fallback,
        }
    }

    fn handle_resign_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            let _ = SessionStateMachine::apply_command(&mut session, CMD_RESIGN, false);
            session
                .metadata
                .insert("terminal".into(), JsonValue::String("resign".into()));
            session.metadata.insert(
                "winner".into(),
                JsonValue::String(session.contact_hash.clone()),
            );
            session.clear_draw_offer();
            self.save_session(&session);
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP TTT] Resigned.".into(),
        }
    }

    fn handle_draw_offer_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            if SessionStateMachine::apply_command(&mut session, CMD_DRAW_OFFER, false).is_ok() {
                session.set_draw_offer(identity_id);
                self.save_session(&session);
            }
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP TTT] Offered a draw".into(),
        }
    }

    fn handle_draw_accept_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            let _ = SessionStateMachine::apply_command(&mut session, CMD_DRAW_ACCEPT, false);
            session
                .metadata
                .insert("terminal".into(), JsonValue::String("draw".into()));
            session.clear_draw_offer();
            self.save_session(&session);
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP TTT] Draw accepted".into(),
        }
    }

    fn handle_draw_decline_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            if SessionStateMachine::apply_command(&mut session, CMD_DRAW_DECLINE, false).is_ok() {
                session.clear_draw_offer();
                self.save_session(&session);
            }
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP TTT] Declined draw offer".into(),
        }
    }

    // --- Validation ---

    fn validate_move(
        &self,
        session: &Session,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
    ) -> (bool, Option<String>) {
        let meta = &session.metadata;

        // 1. Session must be active
        if session.status != STATUS_ACTIVE {
            return (
                false,
                Some(format!("Session is not active (status={})", session.status)),
            );
        }

        // 2. Must be sender's turn. Empty turn on an active session is
        // invalid state — fail closed (canonical per SPEC; matches lrgp-py).
        let turn = meta_str(meta, "turn");
        if turn.is_empty() {
            return (false, Some("Turn is required before moves".into()));
        }
        if turn != sender_hash {
            return (false, Some("Not your turn".into()));
        }

        let index = match payload.get("i").and_then(value_as_u64) {
            Some(i) if i <= 8 => i as usize,
            _ => return (false, Some("Invalid cell index".into())),
        };
        let board_str = payload.get("b").and_then(value_as_str).unwrap_or("");
        let move_num = payload.get("n").and_then(value_as_u64).unwrap_or(0);
        let Some(terminal) = payload.get("x").and_then(value_as_str) else {
            return (false, Some("Terminal marker is required".into()));
        };

        // 3. Cell must be empty
        let old_board = meta_str(meta, "board");
        if old_board.len() != EMPTY_BOARD.len()
            || !old_board
                .bytes()
                .all(|cell| matches!(cell, b'_' | b'X' | b'O'))
        {
            return (false, Some("Stored board is invalid".into()));
        }
        let old_chars: Vec<char> = old_board.chars().collect();
        if index >= old_chars.len() || old_chars[index] != '_' {
            return (false, Some(format!("Cell {index} is already occupied")));
        }

        // 4. Board must match expected
        let marker = marker_for_move(move_num);
        let expected: String = old_chars
            .iter()
            .enumerate()
            .map(|(i, &c)| if i == index { marker } else { c })
            .collect();
        if board_str != expected {
            return (
                false,
                Some(format!(
                    "Board mismatch: expected {expected}, got {board_str}"
                )),
            );
        }

        // 5. Move number must be sequential
        let expected_num = (meta_i64(meta, "move_count") + 1) as u64;
        if move_num != expected_num {
            return (
                false,
                Some(format!(
                    "Move number mismatch: expected {expected_num}, got {move_num}"
                )),
            );
        }

        // 6. Terminal status must match computed result
        let winner = check_winner(board_str);
        let is_draw = check_draw(board_str);
        let claimed_winner = payload.get("w").and_then(value_as_str).unwrap_or("");

        if winner.is_some() && terminal != "win" {
            return (
                false,
                Some(format!("Board shows a win but terminal='{terminal}'")),
            );
        }
        if is_draw && terminal != "draw" {
            return (
                false,
                Some(format!("Board is full (draw) but terminal='{terminal}'")),
            );
        }
        if winner.is_none() && !is_draw && !terminal.is_empty() {
            return (
                false,
                Some(format!("No win/draw but terminal='{terminal}'")),
            );
        }
        if winner.is_some() {
            if claimed_winner != sender_hash {
                return (
                    false,
                    Some(format!(
                        "Winner mismatch: expected {sender_hash}, got {claimed_winner}"
                    )),
                );
            }
        } else if !claimed_winner.is_empty() {
            return (
                false,
                Some("Winner must be empty on a non-winning move".into()),
            );
        }

        // 7. Turn must be opponent (or empty if terminal)
        let next_turn = payload.get("t").and_then(value_as_str).unwrap_or("");
        if !terminal.is_empty() {
            if !next_turn.is_empty() {
                return (false, Some("Turn should be empty on terminal move".into()));
            }
        } else if next_turn == sender_hash {
            return (
                false,
                Some("Turn cannot be the sender after their own move".into()),
            );
        } else if next_turn.is_empty() {
            return (false, Some("Turn is required on non-terminal move".into()));
        } else {
            let first_turn = meta_str(meta, "first_turn");
            let expected_next_turn = if sender_hash == first_turn {
                session.identity_id.clone()
            } else {
                first_turn
            };
            if !expected_next_turn.is_empty() && next_turn != expected_next_turn {
                return (
                    false,
                    Some(format!(
                        "Turn mismatch: expected {expected_next_turn}, got {next_turn}"
                    )),
                );
            }
        }

        (true, None)
    }

    fn render_fallback_inner(
        &self,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
    ) -> String {
        match command {
            CMD_CHALLENGE => "[LRGP TTT] Sent a challenge!".into(),
            CMD_ACCEPT => "[LRGP TTT] Challenge accepted".into(),
            CMD_DECLINE => "[LRGP TTT] Challenge declined".into(),
            CMD_MOVE => {
                let terminal = payload.get("x").and_then(value_as_str).unwrap_or("");
                if terminal == "win" {
                    let n = payload.get("n").and_then(value_as_u64).unwrap_or(0);
                    let marker = marker_for_move(n);
                    format!("[LRGP TTT] {marker} wins!")
                } else if terminal == "draw" {
                    "[LRGP TTT] Game drawn!".into()
                } else {
                    let n = payload.get("n").and_then(value_as_u64);
                    match n {
                        Some(n) => format!("[LRGP TTT] Move {n}"),
                        None => "[LRGP TTT] Move ?".into(),
                    }
                }
            }
            CMD_RESIGN => "[LRGP TTT] Resigned.".into(),
            CMD_DRAW_OFFER => "[LRGP TTT] Offered a draw".into(),
            CMD_DRAW_ACCEPT => "[LRGP TTT] Draw accepted".into(),
            CMD_DRAW_DECLINE => "[LRGP TTT] Draw declined".into(),
            CMD_ERROR => {
                let msg = payload
                    .get("msg")
                    .and_then(value_as_str)
                    .unwrap_or("Unknown");
                format!("[LRGP TTT] Error: {msg}")
            }
            other => format!("[LRGP TTT] {other}"),
        }
    }
}

impl Default for TicTacToeApp {
    fn default() -> Self {
        Self::new()
    }
}

impl GameApp for TicTacToeApp {
    fn app_id(&self) -> &str {
        "ttt"
    }

    fn version(&self) -> u32 {
        1
    }

    fn manifest(&self) -> AppManifest {
        let mut preferred_delivery = HashMap::new();
        preferred_delivery.insert(CMD_CHALLENGE.into(), "opportunistic".into());
        preferred_delivery.insert(CMD_ACCEPT.into(), "opportunistic".into());
        preferred_delivery.insert(CMD_DECLINE.into(), "opportunistic".into());
        preferred_delivery.insert(CMD_MOVE.into(), "opportunistic".into());
        preferred_delivery.insert(CMD_RESIGN.into(), "direct".into());
        preferred_delivery.insert(CMD_DRAW_OFFER.into(), "opportunistic".into());
        preferred_delivery.insert(CMD_DRAW_ACCEPT.into(), "direct".into());
        preferred_delivery.insert(CMD_DRAW_DECLINE.into(), "direct".into());
        preferred_delivery.insert(CMD_ERROR.into(), "opportunistic".into());

        let ttl = Self::ttl_policy();

        AppManifest {
            app_id: "ttt".into(),
            version: 1,
            display_name: "Tic-Tac-Toe".into(),
            icon: "ttt".into(),
            session_type: SESSION_TURN_BASED.into(),
            max_players: 2,
            validation: VALIDATION_BOTH.into(),
            actions: vec![
                CMD_CHALLENGE.into(),
                CMD_ACCEPT.into(),
                CMD_DECLINE.into(),
                CMD_MOVE.into(),
                CMD_RESIGN.into(),
                CMD_DRAW_OFFER.into(),
                CMD_DRAW_ACCEPT.into(),
                CMD_DRAW_DECLINE.into(),
                CMD_ERROR.into(),
            ],
            preferred_delivery,
            ttl,
        }
    }

    fn handle_incoming(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        if command != CMD_ERROR {
            if let Err(message) =
                self.validate_incoming_payload(session_id, command, payload, identity_id)
            {
                return error_result(ERR_PROTOCOL_ERROR, &message);
            }
        }
        match command {
            CMD_CHALLENGE => {
                self.handle_challenge_in(session_id, payload, sender_hash, identity_id)
            }
            CMD_ACCEPT => self.handle_accept_in(session_id, payload, sender_hash, identity_id),
            CMD_DECLINE => self.handle_decline_in(session_id, sender_hash, identity_id),
            CMD_MOVE => self.handle_move_in(session_id, payload, sender_hash, identity_id),
            CMD_RESIGN => self.handle_resign_in(session_id, sender_hash, identity_id),
            CMD_DRAW_OFFER => self.handle_draw_offer_in(session_id, sender_hash, identity_id),
            CMD_DRAW_ACCEPT => self.handle_draw_accept_in(session_id, sender_hash, identity_id),
            CMD_DRAW_DECLINE => self.handle_draw_decline_in(session_id, sender_hash, identity_id),
            CMD_ERROR => IncomingResult {
                session: None,
                emit: None,
                error: Some(
                    payload
                        .iter()
                        .map(|(k, v)| (k.clone(), rmpv_to_json(v)))
                        .collect(),
                ),
            },
            other => error_result(ERR_PROTOCOL_ERROR, &format!("Unknown command: {other}")),
        }
    }

    fn handle_outgoing(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> OutgoingResult {
        match command {
            CMD_CHALLENGE => self.handle_challenge_out(session_id, identity_id),
            CMD_ACCEPT => self.handle_accept_out(session_id, identity_id),
            CMD_DECLINE => self.handle_decline_out(session_id, identity_id),
            CMD_MOVE => self.handle_move_out(session_id, payload, identity_id),
            CMD_RESIGN => self.handle_resign_out(session_id, identity_id),
            CMD_DRAW_OFFER => self.handle_draw_offer_out(session_id, identity_id),
            CMD_DRAW_ACCEPT => self.handle_draw_accept_out(session_id, identity_id),
            CMD_DRAW_DECLINE => self.handle_draw_decline_out(session_id, identity_id),
            _ => OutgoingResult {
                payload: payload.clone(),
                fallback_text: format!("[LRGP TTT] {command}"),
            },
        }
    }

    fn validate_action(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> (bool, Option<String>) {
        let session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => {
                return if command == CMD_CHALLENGE {
                    (true, None)
                } else {
                    (false, Some("Session not found".into()))
                };
            }
        };

        let mut session = session;
        if SessionStateMachine::check_expiry(&mut session, Some(&Self::ttl_policy()), None) {
            self.save_session(&session);
            return (false, Some("Session expired".into()));
        }

        if command == CMD_MOVE {
            return self.validate_move(&session, payload, identity_id);
        }

        (true, None)
    }

    fn validate_outgoing_action(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> (bool, Option<String>) {
        match command {
            CMD_CHALLENGE | CMD_ACCEPT | CMD_DECLINE | CMD_RESIGN | CMD_DRAW_OFFER
            | CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE => {
                if !payload.is_empty() {
                    return (false, Some(format!("{command} payload must be empty")));
                }
            }
            CMD_MOVE => {
                if !has_exact_keys(payload, &["i"])
                    || payload.get("i").and_then(value_as_u64).is_none()
                {
                    return (
                        false,
                        Some("move intent must contain exactly integer i".into()),
                    );
                }
            }
            _ => return (false, Some(format!("Unsupported action: {command}"))),
        }

        let Some(session) = self.get_session(session_id, identity_id) else {
            return if command == CMD_CHALLENGE {
                (true, None)
            } else {
                (false, Some("Session not found".into()))
            };
        };
        if session.status == STATUS_EXPIRED {
            return (false, Some("Session expired".into()));
        }
        if command == CMD_CHALLENGE {
            return (false, Some("Session already exists".into()));
        }

        if command == CMD_DRAW_OFFER && session.has_draw_offer() {
            return (false, Some("A draw offer is already outstanding".into()));
        }
        if matches!(command, CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE) {
            let Some(offered_by) = session.draw_offered_by() else {
                return (false, Some("No draw offer is outstanding".into()));
            };
            if offered_by == identity_id {
                return (
                    false,
                    Some("A participant cannot answer its own draw offer".into()),
                );
            }
        }

        if command != CMD_MOVE {
            let mut candidate = session;
            return match SessionStateMachine::apply_command(&mut candidate, command, false) {
                Ok(_) => (true, None),
                Err(error) => (false, Some(error.to_string())),
            };
        }
        if session.status != STATUS_ACTIVE {
            return (
                false,
                Some(format!("Session is not active ({})", session.status)),
            );
        }
        if meta_str(&session.metadata, "turn") != identity_id {
            return (false, Some("Not your turn".into()));
        }
        let Some(index) = payload.get("i").and_then(value_as_u64) else {
            return (false, Some("Invalid cell index".into()));
        };
        if index > 8 {
            return (false, Some("Invalid cell index".into()));
        }
        let board = meta_str(&session.metadata, "board");
        if board.as_bytes().get(index as usize) != Some(&b'_') {
            return (false, Some(format!("Cell {index} is already occupied")));
        }
        if session.contact_hash.is_empty() {
            return (false, Some("Opponent unknown".into()));
        }
        (true, None)
    }

    fn get_session_state(&self, session_id: &str, identity_id: &str) -> HashMap<String, JsonValue> {
        match self.get_session(session_id, identity_id) {
            Some(s) => session_to_json(&s),
            None => HashMap::new(),
        }
    }

    fn render_fallback(&self, command: &str, payload: &HashMap<String, rmpv::Value>) -> String {
        self.render_fallback_inner(command, payload)
    }

    fn get_delivery_method(&self, command: &str) -> String {
        match command {
            CMD_RESIGN | CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE => "direct".into(),
            _ => "opportunistic".into(),
        }
    }

    fn get_session_record(&self, session_id: &str, identity_id: &str) -> Option<Session> {
        self.get_session(session_id, identity_id)
    }

    fn upsert_session(&self, session: Session) -> Result<(), LrgpError> {
        if session.app_id != self.app_id() || session.app_version != self.version() {
            return Err(LrgpError::Validation {
                code: ERR_UNSUPPORTED_APP.into(),
                message: "session app/version does not match Tic-Tac-Toe".into(),
            });
        }
        if session.identity_id.is_empty() {
            return Err(LrgpError::Validation {
                code: ERR_PROTOCOL_ERROR.into(),
                message: "restored session must include identity_id".into(),
            });
        }
        if !crate::envelope::is_valid_session_id(&session.session_id) {
            return Err(LrgpError::InvalidEnvelope(
                "restored session id must be exactly 16 lowercase hexadecimal characters".into(),
            ));
        }
        let mut session = session;
        SessionStateMachine::check_expiry(&mut session, Some(&Self::ttl_policy()), None);
        if session.draw_offered_by().is_none() {
            // Pre-owner persisted records and stray owner metadata cannot
            // safely authorize a response.
            session.clear_draw_offer();
        } else if let Some(owner) = session.draw_offered_by() {
            if owner != session.identity_id && owner != session.contact_hash {
                return Err(LrgpError::Validation {
                    code: ERR_PROTOCOL_ERROR.into(),
                    message: "draw offer owner is not a bound participant".into(),
                });
            }
        }
        self.save_session(&session);
        Ok(())
    }

    fn remove_session(&self, session_id: &str, identity_id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .remove(&(session_id.to_string(), identity_id.to_string()))
            .is_some()
    }

    fn list_session_records(&self, identity_id: Option<&str>) -> Vec<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        let ttl = Self::ttl_policy();
        sessions
            .values_mut()
            .filter(|session| identity_id.is_none_or(|id| session.identity_id == id))
            .map(|session| {
                SessionStateMachine::check_expiry(session, Some(&ttl), None);
                session.clone()
            })
            .collect()
    }

    fn bind_session_peer(
        &self,
        session_id: &str,
        identity_id: &str,
        peer_hash: &str,
    ) -> Result<(), LrgpError> {
        if peer_hash.is_empty() {
            return Err(LrgpError::ParticipantRequired);
        }
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&(session_id.to_string(), identity_id.to_string()))
            .ok_or_else(|| LrgpError::SessionNotFound(session_id.into()))?;
        if !session.contact_hash.is_empty() && session.contact_hash != peer_hash {
            return Err(LrgpError::UnauthorizedPeer {
                session_id: session_id.into(),
            });
        }
        session.contact_hash = peer_hash.into();
        Ok(())
    }

    fn authorize_incoming(
        &self,
        session_id: &str,
        command: &str,
        sender_hash: &str,
        identity_id: &str,
    ) -> Result<(), LrgpError> {
        let Some(session) = self.get_session(session_id, identity_id) else {
            return if command == CMD_CHALLENGE {
                Ok(())
            } else {
                Err(LrgpError::SessionNotFound(session_id.into()))
            };
        };
        if session.status == STATUS_EXPIRED {
            return Err(LrgpError::SessionExpired(session_id.into()));
        }
        if session.contact_hash.is_empty() || session.contact_hash != sender_hash {
            return Err(LrgpError::UnauthorizedPeer {
                session_id: session_id.into(),
            });
        }
        Ok(())
    }

    fn snapshot_session(&self, session_id: &str, identity_id: &str) -> Option<Session> {
        self.get_session(session_id, identity_id)
    }

    fn rollback_session(&self, session_id: &str, identity_id: &str, snapshot: Option<Session>) {
        match snapshot {
            Some(snap) => self.save_session(&snap),
            None => {
                if let Ok(mut sessions) = self.sessions.lock() {
                    sessions.remove(&(session_id.to_string(), identity_id.to_string()));
                }
            }
        }
    }
}

fn session_to_json(session: &Session) -> HashMap<String, JsonValue> {
    let mut m = HashMap::new();
    m.insert(
        "session_id".into(),
        JsonValue::String(session.session_id.clone()),
    );
    m.insert(
        "identity_id".into(),
        JsonValue::String(session.identity_id.clone()),
    );
    m.insert("app_id".into(), JsonValue::String(session.app_id.clone()));
    m.insert(
        "app_version".into(),
        JsonValue::Number((session.app_version as i64).into()),
    );
    m.insert(
        "contact_hash".into(),
        JsonValue::String(session.contact_hash.clone()),
    );
    m.insert(
        "initiator".into(),
        JsonValue::String(session.initiator.clone()),
    );
    m.insert("status".into(), JsonValue::String(session.status.clone()));
    m.insert(
        "metadata".into(),
        JsonValue::Object(session.metadata.clone().into_iter().collect()),
    );
    m.insert("unread".into(), JsonValue::Number(session.unread.into()));
    m.insert("created_at".into(), serde_json::json!(session.created_at));
    m.insert("updated_at".into(), serde_json::json!(session.updated_at));
    m.insert(
        "last_action_at".into(),
        serde_json::json!(session.last_action_at),
    );
    m
}

fn rmpv_to_json(v: &rmpv::Value) -> JsonValue {
    match v {
        rmpv::Value::Nil => JsonValue::Null,
        rmpv::Value::Boolean(b) => JsonValue::Bool(*b),
        rmpv::Value::Integer(i) => {
            if let Some(u) = i.as_u64() {
                JsonValue::Number(u.into())
            } else if let Some(s) = i.as_i64() {
                JsonValue::Number(s.into())
            } else {
                JsonValue::Null
            }
        }
        rmpv::Value::F32(f) => serde_json::json!(*f),
        rmpv::Value::F64(f) => serde_json::json!(*f),
        rmpv::Value::String(s) => JsonValue::String(s.as_str().unwrap_or("").to_string()),
        rmpv::Value::Binary(b) => JsonValue::String(hex::encode(b)),
        rmpv::Value::Array(arr) => JsonValue::Array(arr.iter().map(rmpv_to_json).collect()),
        rmpv::Value::Map(pairs) => {
            let obj: serde_json::Map<String, JsonValue> = pairs
                .iter()
                .filter_map(|(k, v)| {
                    let key = match k {
                        rmpv::Value::String(s) => s.as_str()?.to_string(),
                        _ => return None,
                    };
                    Some((key, rmpv_to_json(v)))
                })
                .collect();
            JsonValue::Object(obj)
        }
        rmpv::Value::Ext(_, _) => JsonValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn _setup_game() -> (TicTacToeApp, String) {
        let app = TicTacToeApp::new();
        let challenger = "challenger_hash";
        let responder = "responder_hash";

        // Challenger sends challenge (outgoing)
        let out = app.handle_outgoing("sess1", CMD_CHALLENGE, &HashMap::new(), challenger);
        assert!(out.fallback_text.contains("challenge"));

        // Set contact_hash on the challenger's session
        {
            let mut sessions = app.sessions.lock().unwrap();
            if let Some(s) = sessions.get_mut(&("sess1".into(), challenger.into())) {
                s.contact_hash = responder.to_string();
            }
        }

        // Responder receives challenge (incoming)
        let result = app.handle_incoming(
            "sess1",
            CMD_CHALLENGE,
            &HashMap::new(),
            challenger,
            responder,
        );
        assert!(result.error.is_none());

        (app, "sess1".to_string())
    }

    #[test]
    fn test_check_winner() {
        assert_eq!(check_winner("XXX______"), Some('X'));
        assert_eq!(check_winner("___OOO___"), Some('O'));
        assert_eq!(check_winner("X___X___X"), Some('X'));
        assert_eq!(check_winner("__X_X_X__"), Some('X'));
        assert_eq!(check_winner("_________"), None);
        assert_eq!(check_winner("XOXOXOOXO"), None); // draw board, no winner
    }

    #[test]
    fn test_check_draw() {
        assert!(check_draw("XOXOOXXXO"));
        assert!(!check_draw("XOXOOXX_O"));
        assert!(!check_draw("XXXOO____")); // has winner
    }

    #[test]
    fn test_marker_for_move() {
        assert_eq!(marker_for_move(1), 'X');
        assert_eq!(marker_for_move(2), 'O');
        assert_eq!(marker_for_move(3), 'X');
    }

    #[test]
    fn test_challenge_flow() {
        let app = TicTacToeApp::new();

        // Outgoing challenge
        let out = app.handle_outgoing("s1", CMD_CHALLENGE, &HashMap::new(), "alice");
        assert_eq!(out.fallback_text, "[LRGP TTT] Sent a challenge!");

        let sess = app.get_session("s1", "alice").unwrap();
        assert_eq!(sess.status, STATUS_PENDING);
        assert_eq!(sess.metadata["my_marker"], "X");

        // Incoming challenge on other side
        let result = app.handle_incoming("s1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        assert!(result.error.is_none());

        let sess = app.get_session("s1", "bob").unwrap();
        assert_eq!(sess.status, STATUS_PENDING);
        assert_eq!(sess.metadata["my_marker"], "O");
    }

    #[test]
    fn test_accept_flow() {
        let app = TicTacToeApp::new();

        // Setup: challenge
        app.handle_outgoing("s1", CMD_CHALLENGE, &HashMap::new(), "alice");
        app.handle_incoming("s1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");

        // Bob accepts (outgoing)
        let out = app.handle_outgoing("s1", CMD_ACCEPT, &HashMap::new(), "bob");
        assert_eq!(out.fallback_text, "[LRGP TTT] Challenge accepted");

        let sess = app.get_session("s1", "bob").unwrap();
        assert_eq!(sess.status, STATUS_ACTIVE);

        // Alice receives accept (incoming)
        let result = app.handle_incoming("s1", CMD_ACCEPT, &out.payload, "bob", "alice");
        assert!(result.error.is_none());

        let sess = app.get_session("s1", "alice").unwrap();
        assert_eq!(sess.status, STATUS_ACTIVE);
    }

    #[test]
    fn test_challenger_first_move_sets_responder_turn_without_seeded_contact() {
        let app = TicTacToeApp::new();
        let challenger = "alice";
        let responder = "bob";

        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), challenger);
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), challenger, responder);
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), responder);
        let accept_in =
            app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, responder, challenger);
        assert!(accept_in.error.is_none());

        let challenger_session = app.get_session("g1", challenger).unwrap();
        assert_eq!(challenger_session.contact_hash, responder);
        assert_eq!(meta_str(&challenger_session.metadata, "turn"), challenger);

        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(4.into()));
        let move_out = app.handle_outgoing("g1", CMD_MOVE, &p, challenger);
        assert!(!move_out.payload.is_empty());
        assert_eq!(
            value_as_str(move_out.payload.get("t").unwrap()).unwrap(),
            responder
        );

        let move_in = app.handle_incoming("g1", CMD_MOVE, &move_out.payload, challenger, responder);
        assert!(move_in.error.is_none());
        let responder_session = app.get_session("g1", responder).unwrap();
        assert_eq!(meta_str(&responder_session.metadata, "turn"), responder);

        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(0.into()));
        let responder_move = app.handle_outgoing("g1", CMD_MOVE, &p, responder);
        assert!(!responder_move.payload.is_empty());
        assert_eq!(
            value_as_str(responder_move.payload.get("t").unwrap()).unwrap(),
            challenger
        );
    }

    #[test]
    fn test_decline_flow() {
        let app = TicTacToeApp::new();

        app.handle_outgoing("s1", CMD_CHALLENGE, &HashMap::new(), "alice");
        app.handle_incoming("s1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");

        let out = app.handle_outgoing("s1", CMD_DECLINE, &HashMap::new(), "bob");
        assert_eq!(out.fallback_text, "[LRGP TTT] Challenge declined");

        let sess = app.get_session("s1", "bob").unwrap();
        assert_eq!(sess.status, STATUS_DECLINED);
    }

    #[test]
    fn test_full_game_x_wins() {
        let app = TicTacToeApp::new();
        let x = "x_player";
        let o = "o_player";

        // Challenge + accept
        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), x);
        {
            let mut sessions = app.sessions.lock().unwrap();
            sessions
                .get_mut(&("g1".into(), x.into()))
                .unwrap()
                .contact_hash = o.to_string();
        }
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), x, o);
        let accept_out = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), o);
        app.handle_incoming("g1", CMD_ACCEPT, &accept_out.payload, o, x);

        // Move 1: X plays center (4)
        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(4.into()));
        let m1 = app.handle_outgoing("g1", CMD_MOVE, &p, x);
        assert!(
            value_as_str(m1.payload.get("x").unwrap())
                .unwrap()
                .is_empty()
        );
        app.handle_incoming("g1", CMD_MOVE, &m1.payload, x, o);

        // Move 2: O plays top-left (0)
        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(0.into()));
        let m2 = app.handle_outgoing("g1", CMD_MOVE, &p, o);
        app.handle_incoming("g1", CMD_MOVE, &m2.payload, o, x);

        // Move 3: X plays top-right (2)
        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(2.into()));
        let m3 = app.handle_outgoing("g1", CMD_MOVE, &p, x);
        app.handle_incoming("g1", CMD_MOVE, &m3.payload, x, o);

        // Move 4: O plays bottom-left (6)
        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(6.into()));
        let m4 = app.handle_outgoing("g1", CMD_MOVE, &p, o);
        app.handle_incoming("g1", CMD_MOVE, &m4.payload, o, x);

        // Move 5: X plays (5)
        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(5.into()));
        let m5 = app.handle_outgoing("g1", CMD_MOVE, &p, x);
        app.handle_incoming("g1", CMD_MOVE, &m5.payload, x, o);

        // Move 6: O plays (1)
        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(1.into()));
        let m6 = app.handle_outgoing("g1", CMD_MOVE, &p, o);
        app.handle_incoming("g1", CMD_MOVE, &m6.payload, o, x);

        // Move 7: X plays (3) → row 3,4,5 = X,X,X → WIN!
        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(3.into()));
        let m7 = app.handle_outgoing("g1", CMD_MOVE, &p, x);
        assert_eq!(value_as_str(m7.payload.get("x").unwrap()).unwrap(), "win");
        assert!(m7.fallback_text.contains("wins"));

        let sess = app.get_session("g1", x).unwrap();
        assert_eq!(sess.status, STATUS_COMPLETED);
    }

    #[test]
    fn test_out_of_turn_move_rejected() {
        let app = TicTacToeApp::new();
        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), "alice");
        {
            let mut sessions = app.sessions.lock().unwrap();
            sessions
                .get_mut(&("g1".into(), "alice".into()))
                .unwrap()
                .contact_hash = "bob".to_string();
        }
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), "bob");
        app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, "bob", "alice");

        let before = app.get_session("g1", "bob").unwrap();
        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(0.into()));
        let out = app.handle_outgoing("g1", CMD_MOVE, &p, "bob");

        assert_eq!(out.fallback_text, "[LRGP TTT] Not your turn");
        assert!(out.payload.is_empty());
        let after = app.get_session("g1", "bob").unwrap();
        assert_eq!(after.metadata["board"], before.metadata["board"]);
        assert_eq!(after.metadata["turn"], before.metadata["turn"]);
    }

    #[test]
    fn test_occupied_cell_move_rejected() {
        let app = TicTacToeApp::new();
        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), "alice");
        {
            let mut sessions = app.sessions.lock().unwrap();
            sessions
                .get_mut(&("g1".into(), "alice".into()))
                .unwrap()
                .contact_hash = "bob".to_string();
        }
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), "bob");
        app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, "bob", "alice");

        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(4.into()));
        let m1 = app.handle_outgoing("g1", CMD_MOVE, &p, "alice");
        app.handle_incoming("g1", CMD_MOVE, &m1.payload, "alice", "bob");

        let before = app.get_session("g1", "bob").unwrap();
        let mut occupied = HashMap::new();
        occupied.insert("i".into(), rmpv::Value::Integer(4.into()));
        let out = app.handle_outgoing("g1", CMD_MOVE, &occupied, "bob");

        assert!(out.fallback_text.contains("already occupied"));
        assert!(out.payload.is_empty());
        let after = app.get_session("g1", "bob").unwrap();
        assert_eq!(after.metadata["board"], before.metadata["board"]);
        assert_eq!(after.metadata["turn"], before.metadata["turn"]);
    }

    #[test]
    fn test_rollback_restores_existing_session() {
        let app = TicTacToeApp::new();
        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), "alice");
        {
            let mut sessions = app.sessions.lock().unwrap();
            sessions
                .get_mut(&("g1".into(), "alice".into()))
                .unwrap()
                .contact_hash = "bob".to_string();
        }
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), "bob");
        app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, "bob", "alice");

        let snap = app.snapshot_session("g1", "alice");
        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(4.into()));
        app.handle_outgoing("g1", CMD_MOVE, &p, "alice");
        let after_move = app.get_session("g1", "alice").unwrap();
        assert_ne!(after_move.metadata["board"], "_________");

        app.rollback_session("g1", "alice", snap);
        let restored = app.get_session("g1", "alice").unwrap();
        assert_eq!(restored.metadata["board"], "_________");
        assert_eq!(restored.metadata["turn"], "alice");
    }

    #[test]
    fn test_rollback_removes_new_session() {
        let app = TicTacToeApp::new();
        let snap = app.snapshot_session("new_sid", "alice");
        app.handle_outgoing("new_sid", CMD_CHALLENGE, &HashMap::new(), "alice");
        assert!(app.get_session("new_sid", "alice").is_some());

        app.rollback_session("new_sid", "alice", snap);
        assert!(app.get_session("new_sid", "alice").is_none());
    }

    #[test]
    fn test_resign() {
        let app = TicTacToeApp::new();

        // Setup active game
        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), "alice");
        {
            let mut sessions = app.sessions.lock().unwrap();
            sessions
                .get_mut(&("g1".into(), "alice".into()))
                .unwrap()
                .contact_hash = "bob".to_string();
        }
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), "bob");
        app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, "bob", "alice");

        // Alice resigns
        let out = app.handle_outgoing("g1", CMD_RESIGN, &HashMap::new(), "alice");
        assert_eq!(out.fallback_text, "[LRGP TTT] Resigned.");

        let sess = app.get_session("g1", "alice").unwrap();
        assert_eq!(sess.status, STATUS_COMPLETED);
        assert_eq!(sess.metadata["terminal"], "resign");
        assert_eq!(sess.metadata["winner"], "bob"); // opponent wins
    }

    #[test]
    fn test_draw_negotiation() {
        let app = TicTacToeApp::new();

        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), "alice");
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), "bob");
        app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, "bob", "alice");

        // Bob offers draw
        let result = app.handle_incoming("g1", CMD_DRAW_OFFER, &HashMap::new(), "bob", "alice");
        assert!(result.error.is_none());
        let sess = app.get_session("g1", "alice").unwrap();
        assert_eq!(sess.metadata["draw_offered"], true);

        // Alice accepts draw
        let out = app.handle_outgoing("g1", CMD_DRAW_ACCEPT, &HashMap::new(), "alice");
        assert_eq!(out.fallback_text, "[LRGP TTT] Draw accepted");
        let sess = app.get_session("g1", "alice").unwrap();
        assert_eq!(sess.status, STATUS_COMPLETED);
        assert_eq!(sess.metadata["terminal"], "draw");
    }

    #[test]
    fn test_remote_draw_response_uses_offer_owner_and_clears_it() {
        let app = TicTacToeApp::new();
        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), "alice");
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), "bob");
        app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, "bob", "alice");

        app.handle_outgoing("g1", CMD_DRAW_OFFER, &HashMap::new(), "alice");
        let offered = app.get_session("g1", "alice").unwrap();
        assert_eq!(offered.draw_offered_by(), Some("alice"));

        let accepted = app.handle_incoming("g1", CMD_DRAW_ACCEPT, &HashMap::new(), "bob", "alice");
        assert!(accepted.error.is_none());
        let completed = app.get_session("g1", "alice").unwrap();
        assert_eq!(completed.status, STATUS_COMPLETED);
        assert_eq!(completed.draw_offered_by(), None);
    }

    #[test]
    fn test_draw_offer_cannot_be_overwritten_or_answered_by_owner() {
        let app = TicTacToeApp::new();
        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), "alice");
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), "bob");
        app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, "bob", "alice");

        let offer = app.handle_incoming("g1", CMD_DRAW_OFFER, &HashMap::new(), "bob", "alice");
        assert!(offer.error.is_none());
        let duplicate = app.handle_incoming("g1", CMD_DRAW_OFFER, &HashMap::new(), "bob", "alice");
        assert!(duplicate.error.is_some());
        let self_accept =
            app.handle_incoming("g1", CMD_DRAW_ACCEPT, &HashMap::new(), "bob", "alice");
        assert!(self_accept.error.is_some());
        let session = app.get_session("g1", "alice").unwrap();
        assert_eq!(session.status, STATUS_ACTIVE);
        assert_eq!(session.draw_offered_by(), Some("bob"));
    }

    #[test]
    fn test_strict_payload_shapes_reject_before_mutation() {
        let app = TicTacToeApp::new();
        let junk = HashMap::from([("extra".into(), rmpv::Value::Boolean(true))]);
        let malformed_challenge = app.handle_incoming("g1", CMD_CHALLENGE, &junk, "alice", "bob");
        assert!(malformed_challenge.error.is_some());
        assert!(app.get_session("g1", "bob").is_none());

        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), "alice");
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), "bob");
        app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, "bob", "alice");

        let before = app.get_session("g1", "bob").unwrap();
        let intent = HashMap::from([("i".into(), rmpv::Value::from(4))]);
        let mut wire_move = app
            .handle_outgoing("g1", CMD_MOVE, &intent, "alice")
            .payload;
        wire_move.insert("extra".into(), rmpv::Value::Nil);
        let malformed = app.handle_incoming("g1", CMD_MOVE, &wire_move, "alice", "bob");
        assert!(malformed.error.is_some());
        let after = app.get_session("g1", "bob").unwrap();
        assert_eq!(after.metadata["board"], before.metadata["board"]);

        let invalid_intent = HashMap::from([
            ("i".into(), rmpv::Value::from(4)),
            ("extra".into(), rmpv::Value::Nil),
        ]);
        let (valid, _) = app.validate_outgoing_action("g1", CMD_MOVE, &invalid_intent, "alice");
        assert!(!valid);
    }

    #[test]
    fn test_render_fallback() {
        let app = TicTacToeApp::new();

        assert_eq!(
            app.render_fallback(CMD_CHALLENGE, &HashMap::new()),
            "[LRGP TTT] Sent a challenge!"
        );
        assert_eq!(
            app.render_fallback(CMD_RESIGN, &HashMap::new()),
            "[LRGP TTT] Resigned."
        );

        let mut p = HashMap::new();
        p.insert("n".to_string(), rmpv::Value::Integer(3.into()));
        p.insert("x".to_string(), rmpv::Value::String("".into()));
        assert_eq!(app.render_fallback(CMD_MOVE, &p), "[LRGP TTT] Move 3");

        let mut p = HashMap::new();
        p.insert("n".to_string(), rmpv::Value::Integer(5.into()));
        p.insert("x".to_string(), rmpv::Value::String("win".into()));
        assert_eq!(app.render_fallback(CMD_MOVE, &p), "[LRGP TTT] X wins!");
    }

    #[test]
    fn test_validate_action_no_session() {
        let app = TicTacToeApp::new();
        let (valid, _) = app.validate_action("nope", CMD_CHALLENGE, &HashMap::new(), "x");
        assert!(valid);

        let (valid, msg) = app.validate_action("nope", CMD_MOVE, &HashMap::new(), "x");
        assert!(!valid);
        assert!(msg.unwrap().contains("not found"));
    }

    /// T1-13: an active session with an empty `turn` is invalid state; a
    /// move against it must fail closed (canonical per SPEC, matches lrgp-py).
    #[test]
    fn test_validate_move_rejects_empty_turn() {
        let app = TicTacToeApp::new();
        app.handle_outgoing("s1", CMD_CHALLENGE, &HashMap::new(), "alice");
        app.handle_incoming("s1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let accept = app.handle_outgoing("s1", CMD_ACCEPT, &HashMap::new(), "bob");
        app.handle_incoming("s1", CMD_ACCEPT, &accept.payload, "bob", "alice");

        let mut session = app.get_session("s1", "alice").unwrap();
        session
            .metadata
            .insert("turn".into(), JsonValue::String("".into()));

        let mut p = HashMap::new();
        p.insert("i".into(), rmpv::Value::Integer(0.into()));
        p.insert("b".into(), rmpv::Value::String("X________".into()));
        p.insert("n".into(), rmpv::Value::Integer(1.into()));
        let (valid, msg) = app.validate_move(&session, &p, "bob");
        assert!(!valid);
        assert!(msg.unwrap().contains("Turn is required"));
    }
}
