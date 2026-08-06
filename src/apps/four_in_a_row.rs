//! Four in a Row: a compact, deterministic 7x6 gravity game.
//!
//! The wire carries only the selected column, monotonic move number, and a
//! terminal claim. Each peer reconstructs and validates the canonical board.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value as JsonValue;

use crate::app_base::{AppManifest, GameApp, IncomingResult, OutgoingResult};
use crate::constants::*;
use crate::envelope::{has_exact_keys, value_as_str, value_as_u64};
use crate::errors::LrgpError;
use crate::session::{Session, SessionStateMachine};

pub const APP_ID: &str = "four_in_a_row";
pub const APP_VERSION: u32 = 1;
pub const COLUMNS: usize = 7;
pub const ROWS: usize = 6;
pub const CELL_COUNT: usize = COLUMNS * ROWS;
pub const EMPTY_BOARD: &str = "__________________________________________";

const FIRST_MARKER: u8 = b'A';
const SECOND_MARKER: u8 = b'B';

fn gen_session_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn error_result(code: &str, msg: &str) -> IncomingResult {
    IncomingResult {
        session: None,
        emit: None,
        error: Some(HashMap::from([
            ("code".into(), JsonValue::String(code.into())),
            ("msg".into(), JsonValue::String(msg.into())),
        ])),
    }
}

fn emit_event(event_type: &str, session_id: &str, from: &str) -> HashMap<String, JsonValue> {
    HashMap::from([
        ("type".into(), JsonValue::String(event_type.into())),
        ("session_id".into(), JsonValue::String(session_id.into())),
        ("app_id".into(), JsonValue::String(APP_ID.into())),
        ("from".into(), JsonValue::String(from.into())),
    ])
}

fn meta_str(meta: &HashMap<String, JsonValue>, key: &str) -> String {
    meta.get(key)
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string()
}

fn meta_i64(meta: &HashMap<String, JsonValue>, key: &str) -> i64 {
    meta.get(key).and_then(JsonValue::as_i64).unwrap_or(0)
}

fn marker_for_move(move_num: u64) -> u8 {
    if move_num % 2 == 1 {
        FIRST_MARKER
    } else {
        SECOND_MARKER
    }
}

fn is_canonical_board(board: &[u8]) -> bool {
    if board.len() != CELL_COUNT
        || !board
            .iter()
            .all(|cell| matches!(*cell, b'_' | FIRST_MARKER | SECOND_MARKER))
    {
        return false;
    }
    for column in 0..COLUMNS {
        let mut found_empty = false;
        for row in (0..ROWS).rev() {
            match board[row * COLUMNS + column] {
                b'_' => found_empty = true,
                _ if found_empty => return false,
                _ => {}
            }
        }
    }
    true
}

fn drop_cell(board: &[u8], column: usize) -> Option<(usize, usize)> {
    if column >= COLUMNS || board.len() != CELL_COUNT {
        return None;
    }
    (0..ROWS).rev().find_map(|row| {
        let cell = row * COLUMNS + column;
        (board[cell] == b'_').then_some((row, cell))
    })
}

fn board_winner(board: &[u8]) -> Option<u8> {
    if board.len() != CELL_COUNT {
        return None;
    }
    const DIRECTIONS: [(isize, isize); 4] = [(0, 1), (1, 0), (1, 1), (1, -1)];
    for row in 0..ROWS {
        for column in 0..COLUMNS {
            let marker = board[row * COLUMNS + column];
            if marker == b'_' {
                continue;
            }
            for (dr, dc) in DIRECTIONS {
                let won = (1..4).all(|step| {
                    let r = row as isize + dr * step;
                    let c = column as isize + dc * step;
                    r >= 0
                        && r < ROWS as isize
                        && c >= 0
                        && c < COLUMNS as isize
                        && board[r as usize * COLUMNS + c as usize] == marker
                });
                if won {
                    return Some(marker);
                }
            }
        }
    }
    None
}

fn winning_markers(board: &[u8]) -> Vec<u8> {
    let mut winners = Vec::new();
    for marker in [FIRST_MARKER, SECOND_MARKER] {
        let mut found = false;
        for row in 0..ROWS {
            for column in 0..COLUMNS {
                if board.get(row * COLUMNS + column) != Some(&marker) {
                    continue;
                }
                for (dr, dc) in [(0isize, 1isize), (1, 0), (1, 1), (1, -1)] {
                    if (1..4).all(|step| {
                        let r = row as isize + dr * step;
                        let c = column as isize + dc * step;
                        r >= 0
                            && r < ROWS as isize
                            && c >= 0
                            && c < COLUMNS as isize
                            && board[r as usize * COLUMNS + c as usize] == marker
                    }) {
                        found = true;
                    }
                }
            }
        }
        if found {
            winners.push(marker);
        }
    }
    winners
}

fn board_is_draw(board: &[u8]) -> bool {
    board.len() == CELL_COUNT && !board.contains(&b'_') && board_winner(board).is_none()
}

fn other_player(session: &Session, player: &str) -> Option<String> {
    if player == session.identity_id {
        (!session.contact_hash.is_empty()).then(|| session.contact_hash.clone())
    } else if player == session.contact_hash {
        (!session.identity_id.is_empty()).then(|| session.identity_id.clone())
    } else {
        None
    }
}

fn default_metadata(my_marker: &str, first_turn: &str) -> HashMap<String, JsonValue> {
    HashMap::from([
        ("board".into(), JsonValue::String(EMPTY_BOARD.into())),
        ("turn".into(), JsonValue::String(String::new())),
        ("first_turn".into(), JsonValue::String(first_turn.into())),
        ("my_marker".into(), JsonValue::String(my_marker.into())),
        ("first_marker".into(), JsonValue::String("A".into())),
        ("second_marker".into(), JsonValue::String("B".into())),
        ("move_count".into(), JsonValue::Number(0.into())),
        ("last_column".into(), JsonValue::Null),
        ("last_row".into(), JsonValue::Null),
        ("last_cell".into(), JsonValue::Null),
        ("winner".into(), JsonValue::String(String::new())),
        ("terminal".into(), JsonValue::String(String::new())),
        ("draw_offered".into(), JsonValue::Bool(false)),
        ("draw_offered_by".into(), JsonValue::String(String::new())),
    ])
}

fn set_last_move(session: &mut Session, column: usize, row: usize, cell: usize) {
    session.metadata.insert(
        "last_column".into(),
        JsonValue::Number((column as u64).into()),
    );
    session
        .metadata
        .insert("last_row".into(), JsonValue::Number((row as u64).into()));
    session
        .metadata
        .insert("last_cell".into(), JsonValue::Number((cell as u64).into()));
}

fn session_to_json(session: &Session) -> HashMap<String, JsonValue> {
    HashMap::from([
        (
            "session_id".into(),
            JsonValue::String(session.session_id.clone()),
        ),
        (
            "identity_id".into(),
            JsonValue::String(session.identity_id.clone()),
        ),
        ("app_id".into(), JsonValue::String(session.app_id.clone())),
        (
            "app_version".into(),
            JsonValue::Number((session.app_version as u64).into()),
        ),
        (
            "contact_hash".into(),
            JsonValue::String(session.contact_hash.clone()),
        ),
        (
            "initiator".into(),
            JsonValue::String(session.initiator.clone()),
        ),
        ("status".into(), JsonValue::String(session.status.clone())),
        (
            "metadata".into(),
            JsonValue::Object(session.metadata.clone().into_iter().collect()),
        ),
        ("unread".into(), JsonValue::Number(session.unread.into())),
        ("created_at".into(), serde_json::json!(session.created_at)),
        ("updated_at".into(), serde_json::json!(session.updated_at)),
        (
            "last_action_at".into(),
            serde_json::json!(session.last_action_at),
        ),
    ])
}

fn rmpv_to_json(value: &rmpv::Value) -> JsonValue {
    match value {
        rmpv::Value::Nil => JsonValue::Null,
        rmpv::Value::Boolean(v) => JsonValue::Bool(*v),
        rmpv::Value::Integer(v) => v
            .as_u64()
            .map(|n| JsonValue::Number(n.into()))
            .or_else(|| v.as_i64().map(|n| JsonValue::Number(n.into())))
            .unwrap_or(JsonValue::Null),
        rmpv::Value::F32(v) => serde_json::json!(*v),
        rmpv::Value::F64(v) => serde_json::json!(*v),
        rmpv::Value::String(v) => JsonValue::String(v.as_str().unwrap_or("").into()),
        rmpv::Value::Binary(v) => JsonValue::String(hex::encode(v)),
        rmpv::Value::Array(v) => JsonValue::Array(v.iter().map(rmpv_to_json).collect()),
        rmpv::Value::Map(v) => JsonValue::Object(
            v.iter()
                .filter_map(|(key, value)| Some((key.as_str()?.to_string(), rmpv_to_json(value))))
                .collect(),
        ),
        rmpv::Value::Ext(_, _) => JsonValue::Null,
    }
}

/// The built-in Four in a Row LRGP application.
pub struct FourInARowApp {
    sessions: Mutex<HashMap<(String, String), Session>>,
}

impl FourInARowApp {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn ttl_policy() -> HashMap<String, f64> {
        HashMap::from([
            (STATUS_PENDING.into(), TTL_PENDING),
            (STATUS_ACTIVE.into(), TTL_ACTIVE),
        ])
    }

    fn normalize_expired_session(session: &mut Session) {
        if session.status == STATUS_EXPIRED {
            session.clear_draw_offer();
        }
    }

    fn check_session_expiry(session: &mut Session, ttl: &HashMap<String, f64>) {
        SessionStateMachine::check_expiry(session, Some(ttl), None);
        Self::normalize_expired_session(session);
    }

    fn get_session(&self, session_id: &str, identity_id: &str) -> Option<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(&(session_id.into(), identity_id.into()))?;
        Self::check_session_expiry(session, &Self::ttl_policy());
        Some(session.clone())
    }

    fn save_session(&self, session: &Session) {
        self.sessions.lock().unwrap().insert(
            (session.session_id.clone(), session.identity_id.clone()),
            session.clone(),
        );
    }

    fn validate_restored_session(session: &Session) -> Result<(), LrgpError> {
        let invalid = |message: &str| LrgpError::Validation {
            code: ERR_PROTOCOL_ERROR.into(),
            message: message.into(),
        };
        let metadata = &session.metadata;
        let board = metadata
            .get("board")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| invalid("restored board must be a string"))?;
        if !is_canonical_board(board.as_bytes()) {
            return Err(invalid("restored board is not a canonical gravity board"));
        }
        let move_count = metadata
            .get("move_count")
            .and_then(JsonValue::as_u64)
            .filter(|count| *count <= CELL_COUNT as u64)
            .ok_or_else(|| invalid("restored move_count must be an integer from 0 through 42"))?;
        let occupied = board.bytes().filter(|cell| *cell != b'_').count() as u64;
        let first_count = board.bytes().filter(|cell| *cell == FIRST_MARKER).count() as u64;
        let second_count = board.bytes().filter(|cell| *cell == SECOND_MARKER).count() as u64;
        if occupied != move_count
            || first_count != move_count.div_ceil(2)
            || second_count != move_count / 2
        {
            return Err(invalid(
                "restored board counts do not match first-player alternation",
            ));
        }
        if metadata.get("first_marker").and_then(JsonValue::as_str) != Some("A")
            || metadata.get("second_marker").and_then(JsonValue::as_str) != Some("B")
        {
            return Err(invalid("restored marker metadata is invalid"));
        }
        let first_turn = metadata
            .get("first_turn")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("restored first_turn is required"))?;
        if first_turn != session.initiator {
            return Err(invalid("restored first_turn must equal the challenger"));
        }
        if first_turn != session.identity_id && first_turn != session.contact_hash {
            return Err(invalid("restored first_turn is not a bound participant"));
        }
        let expected_my_marker = if session.identity_id == first_turn {
            "A"
        } else {
            "B"
        };
        if metadata.get("my_marker").and_then(JsonValue::as_str) != Some(expected_my_marker) {
            return Err(invalid("restored my_marker does not match first_turn"));
        }

        let last_values = ["last_column", "last_row", "last_cell"]
            .map(|key| metadata.get(key).and_then(JsonValue::as_u64));
        if move_count == 0 {
            if last_values.iter().any(Option::is_some)
                || ["last_column", "last_row", "last_cell"]
                    .iter()
                    .any(|key| metadata.get(*key) != Some(&JsonValue::Null))
            {
                return Err(invalid("an empty game must not have a last move"));
            }
        } else {
            let [Some(column), Some(row), Some(cell)] = last_values else {
                return Err(invalid("a non-empty game requires a complete last move"));
            };
            if column >= COLUMNS as u64
                || row >= ROWS as u64
                || cell != row * COLUMNS as u64 + column
                || board.as_bytes().get(cell as usize) != Some(&marker_for_move(move_count))
            {
                return Err(invalid("restored last move is inconsistent with the board"));
            }
            if (0..row as usize)
                .any(|above| board.as_bytes()[above * COLUMNS + column as usize] != b'_')
            {
                return Err(invalid(
                    "restored last move is not the top disc in its column",
                ));
            }
        }

        let winners = winning_markers(board.as_bytes());
        if winners.len() > 1 {
            return Err(invalid("restored board has two winners"));
        }
        let terminal = metadata
            .get("terminal")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| invalid("restored terminal must be a string"))?;
        let winner = metadata
            .get("winner")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| invalid("restored winner must be a string"))?;
        let turn = metadata
            .get("turn")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| invalid("restored turn must be a string"))?;
        let second_turn = other_player(session, first_turn)
            .ok_or_else(|| invalid("restored session has no bound opponent"))?;
        let expected_turn = if move_count % 2 == 0 {
            first_turn
        } else {
            second_turn.as_str()
        };

        match session.status.as_str() {
            STATUS_PENDING | STATUS_DECLINED => {
                if move_count != 0 || !turn.is_empty() || !terminal.is_empty() || !winner.is_empty()
                {
                    return Err(invalid(
                        "pending or declined session contains game progress",
                    ));
                }
            }
            STATUS_ACTIVE => {
                if turn != expected_turn
                    || !terminal.is_empty()
                    || !winner.is_empty()
                    || !winners.is_empty()
                    || board_is_draw(board.as_bytes())
                {
                    return Err(invalid(
                        "active session metadata is terminal or out of turn",
                    ));
                }
            }
            STATUS_COMPLETED => {
                if !turn.is_empty() {
                    return Err(invalid("completed session must not have a turn"));
                }
                match terminal {
                    "win" => {
                        let Some(marker) = winners.first().copied() else {
                            return Err(invalid("winning session has no four-in-a-row"));
                        };
                        if marker != marker_for_move(move_count) {
                            return Err(invalid("winning marker was not the final mover"));
                        }
                        let expected_winner = if marker == FIRST_MARKER {
                            first_turn
                        } else {
                            second_turn.as_str()
                        };
                        if winner != expected_winner {
                            return Err(invalid("winning identity does not match the board"));
                        }
                        let last_cell = metadata["last_cell"].as_u64().unwrap() as usize;
                        let mut before = board.as_bytes().to_vec();
                        before[last_cell] = b'_';
                        if !winning_markers(&before).is_empty() {
                            return Err(invalid("restored game continued after a winning move"));
                        }
                    }
                    "draw" => {
                        if !winner.is_empty() || !winners.is_empty() {
                            return Err(invalid("draw session contains a winner"));
                        }
                    }
                    "resign" => {
                        if winner != session.identity_id && winner != session.contact_hash {
                            return Err(invalid("resignation winner is not a participant"));
                        }
                        if !winners.is_empty() || board_is_draw(board.as_bytes()) {
                            return Err(invalid("resignation followed a terminal board"));
                        }
                    }
                    _ => return Err(invalid("completed session has invalid terminal metadata")),
                }
            }
            STATUS_EXPIRED => {
                if !terminal.is_empty() || !winner.is_empty() || !winners.is_empty() {
                    return Err(invalid("expired session contains terminal game metadata"));
                }
                if !turn.is_empty() && turn != expected_turn {
                    return Err(invalid("expired session has an invalid turn"));
                }
            }
            _ => return Err(invalid("restored session has an unknown status")),
        }

        let draw_offered = metadata
            .get("draw_offered")
            .and_then(JsonValue::as_bool)
            .ok_or_else(|| invalid("restored draw_offered must be a boolean"))?;
        let draw_owner = metadata
            .get("draw_offered_by")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| invalid("restored draw_offered_by must be a string"))?;
        if draw_offered {
            if session.status != STATUS_ACTIVE
                || (draw_owner != session.identity_id && draw_owner != session.contact_hash)
            {
                return Err(invalid(
                    "restored draw offer has an invalid owner or status",
                ));
            }
        } else if !draw_owner.is_empty() {
            return Err(invalid("restored cleared draw offer still has an owner"));
        }
        Ok(())
    }

    fn validate_wire_payload(
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
                if !has_exact_keys(payload, &["t"]) {
                    return Err("accept payload must contain exactly t".into());
                }
                let turn = payload
                    .get("t")
                    .and_then(value_as_str)
                    .ok_or_else(|| "accept t must be a string".to_string())?;
                let expected = self
                    .get_session(session_id, identity_id)
                    .map(|session| meta_str(&session.metadata, "first_turn"))
                    .unwrap_or_default();
                if expected.is_empty() || turn != expected {
                    return Err("accept first turn does not match challenge".into());
                }
            }
            CMD_MOVE => {
                let terminal = payload
                    .get("x")
                    .and_then(value_as_str)
                    .ok_or_else(|| "move x must be a string".to_string())?;
                if !matches!(terminal, "" | "win" | "draw") {
                    return Err(format!("unsupported terminal marker '{terminal}'"));
                }
                let keys: &[&str] = if terminal == "win" {
                    &["c", "n", "x", "w"]
                } else {
                    &["c", "n", "x"]
                };
                if !has_exact_keys(payload, keys) {
                    return Err("move payload has invalid keys".into());
                }
                if payload.get("c").and_then(value_as_u64).is_none()
                    || payload.get("n").and_then(value_as_u64).is_none()
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

    fn validate_move(
        &self,
        session: &Session,
        payload: &HashMap<String, rmpv::Value>,
        sender: &str,
    ) -> Result<(String, usize, usize, usize, String), String> {
        if session.status != STATUS_ACTIVE {
            return Err(format!("Session is not active ({})", session.status));
        }
        if meta_str(&session.metadata, "turn") != sender {
            return Err("Not your turn".into());
        }
        let column = payload
            .get("c")
            .and_then(value_as_u64)
            .filter(|column| *column < COLUMNS as u64)
            .ok_or_else(|| "Invalid column".to_string())? as usize;
        let move_num = payload
            .get("n")
            .and_then(value_as_u64)
            .ok_or_else(|| "Move number is required".to_string())?;
        let expected_num = (meta_i64(&session.metadata, "move_count") + 1) as u64;
        if move_num != expected_num {
            return Err(format!(
                "Move number mismatch: expected {expected_num}, got {move_num}"
            ));
        }
        let old_board = meta_str(&session.metadata, "board");
        if !is_canonical_board(old_board.as_bytes()) {
            return Err("Stored board is invalid".into());
        }
        let (row, cell) = drop_cell(old_board.as_bytes(), column)
            .ok_or_else(|| format!("Column {column} is full"))?;
        let mut board = old_board.into_bytes();
        board[cell] = marker_for_move(move_num);
        let computed = if board_winner(&board).is_some() {
            "win"
        } else if board_is_draw(&board) {
            "draw"
        } else {
            ""
        };
        let claimed = payload.get("x").and_then(value_as_str).unwrap_or("");
        if claimed != computed {
            return Err(format!(
                "Terminal mismatch: expected '{computed}', got '{claimed}'"
            ));
        }
        let claimed_winner = payload.get("w").and_then(value_as_str).unwrap_or("");
        if computed == "win" && claimed_winner != sender {
            return Err(format!(
                "Winner mismatch: expected {sender}, got {claimed_winner}"
            ));
        }
        if computed != "win" && !claimed_winner.is_empty() {
            return Err("Winner is only valid on a winning move".into());
        }
        let next_turn = if computed.is_empty() {
            other_player(session, sender).ok_or_else(|| "Opponent unknown".to_string())?
        } else {
            String::new()
        };
        Ok((
            String::from_utf8(board).unwrap(),
            column,
            row,
            cell,
            next_turn,
        ))
    }

    fn apply_move(
        &self,
        session: &mut Session,
        payload: &HashMap<String, rmpv::Value>,
        sender: &str,
    ) -> Result<(), String> {
        let (board, column, row, cell, next_turn) = self.validate_move(session, payload, sender)?;
        let move_num = payload.get("n").and_then(value_as_u64).unwrap();
        let terminal = payload.get("x").and_then(value_as_str).unwrap();
        session
            .metadata
            .insert("board".into(), JsonValue::String(board));
        session
            .metadata
            .insert("turn".into(), JsonValue::String(next_turn));
        session
            .metadata
            .insert("move_count".into(), JsonValue::Number(move_num.into()));
        set_last_move(session, column, row, cell);
        session
            .metadata
            .insert("terminal".into(), JsonValue::String(terminal.into()));
        session.metadata.insert(
            "winner".into(),
            JsonValue::String(if terminal == "win" {
                sender.into()
            } else {
                String::new()
            }),
        );
        session.clear_draw_offer();
        SessionStateMachine::apply_command(session, CMD_MOVE, !terminal.is_empty())
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn handle_challenge_in(
        &self,
        session_id: &str,
        sender: &str,
        identity: &str,
    ) -> IncomingResult {
        if let Some(existing) = self.get_session(session_id, identity) {
            return IncomingResult {
                session: Some(session_to_json(&existing)),
                emit: None,
                error: None,
            };
        }
        let mut session = Session::new(session_id);
        session.identity_id = identity.into();
        session.app_id = APP_ID.into();
        session.app_version = APP_VERSION;
        session.contact_hash = sender.into();
        session.initiator = sender.into();
        session.metadata = default_metadata("B", sender);
        session.unread = 1;
        self.save_session(&session);
        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("challenge", session_id, sender)),
            error: None,
        }
    }

    fn handle_accept_in(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        sender: &str,
        identity: &str,
    ) -> IncomingResult {
        let Some(mut session) = self.get_session(session_id, identity) else {
            return error_result(ERR_PROTOCOL_ERROR, "Unknown session");
        };
        if session.contact_hash.is_empty() {
            session.contact_hash = sender.into();
        }
        if let Err(error) = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false) {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        let turn = payload.get("t").and_then(value_as_str).unwrap();
        session
            .metadata
            .insert("turn".into(), JsonValue::String(turn.into()));
        session.unread = 1;
        self.save_session(&session);
        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("accept", session_id, sender)),
            error: None,
        }
    }

    fn handle_simple_in(
        &self,
        session_id: &str,
        command: &str,
        sender: &str,
        identity: &str,
    ) -> IncomingResult {
        let Some(mut session) = self.get_session(session_id, identity) else {
            return error_result(ERR_PROTOCOL_ERROR, "Unknown session");
        };
        if command == CMD_DRAW_OFFER && session.has_draw_offer() {
            return error_result(ERR_PROTOCOL_ERROR, "A draw offer is already outstanding");
        }
        if matches!(command, CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE) {
            let Some(owner) = session.draw_offered_by() else {
                return error_result(ERR_PROTOCOL_ERROR, "No draw offer is outstanding");
            };
            if owner == sender {
                return error_result(ERR_PROTOCOL_ERROR, "Cannot answer your own draw offer");
            }
        }
        if let Err(error) = SessionStateMachine::apply_command(&mut session, command, false) {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        match command {
            CMD_RESIGN => {
                let winner = other_player(&session, sender).unwrap_or_default();
                session
                    .metadata
                    .insert("turn".into(), JsonValue::String(String::new()));
                session
                    .metadata
                    .insert("terminal".into(), JsonValue::String("resign".into()));
                session
                    .metadata
                    .insert("winner".into(), JsonValue::String(winner));
                session.clear_draw_offer();
            }
            CMD_DRAW_OFFER => session.set_draw_offer(sender),
            CMD_DRAW_ACCEPT => {
                session
                    .metadata
                    .insert("turn".into(), JsonValue::String(String::new()));
                session
                    .metadata
                    .insert("terminal".into(), JsonValue::String("draw".into()));
                session.clear_draw_offer();
            }
            CMD_DRAW_DECLINE => session.clear_draw_offer(),
            _ => {}
        }
        session.unread = 1;
        self.save_session(&session);
        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event(command, session_id, sender)),
            error: None,
        }
    }

    fn handle_move_in(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        sender: &str,
        identity: &str,
    ) -> IncomingResult {
        let Some(mut session) = self.get_session(session_id, identity) else {
            return error_result(ERR_PROTOCOL_ERROR, "Unknown session");
        };
        if let Err(message) = self.apply_move(&mut session, payload, sender) {
            let mut result = error_result(ERR_INVALID_MOVE, &message);
            result.session = Some(session_to_json(&session));
            return result;
        }
        session.unread = 1;
        self.save_session(&session);
        let mut emit = emit_event("move", session_id, sender);
        emit.insert(
            "payload".into(),
            JsonValue::Object(
                payload
                    .iter()
                    .map(|(key, value)| (key.clone(), rmpv_to_json(value)))
                    .collect(),
            ),
        );
        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit),
            error: None,
        }
    }

    fn handle_challenge_out(&self, session_id: &str, identity: &str) -> OutgoingResult {
        let sid = if session_id.is_empty() {
            gen_session_id()
        } else {
            session_id.into()
        };
        let mut session = Session::new(sid);
        session.identity_id = identity.into();
        session.app_id = APP_ID.into();
        session.app_version = APP_VERSION;
        session.initiator = identity.into();
        session.metadata = default_metadata("A", identity);
        self.save_session(&session);
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP Four in a Row] Sent a challenge!".into(),
        }
    }

    fn handle_accept_out(&self, session_id: &str, identity: &str) -> OutgoingResult {
        let (valid, message) =
            self.validate_outgoing_action(session_id, CMD_ACCEPT, &HashMap::new(), identity);
        if !valid {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: format!(
                    "[LRGP Four in a Row] {}",
                    message.unwrap_or_else(|| "Cannot accept challenge".into())
                ),
            };
        }
        let Some(mut session) = self.get_session(session_id, identity) else {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP Four in a Row] Session not found".into(),
            };
        };
        let _ = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false);
        let first_turn = meta_str(&session.metadata, "first_turn");
        session
            .metadata
            .insert("turn".into(), JsonValue::String(first_turn.clone()));
        self.save_session(&session);
        OutgoingResult {
            payload: HashMap::from([("t".into(), rmpv::Value::String(first_turn.into()))]),
            fallback_text: "[LRGP Four in a Row] Challenge accepted".into(),
        }
    }

    fn handle_move_out(
        &self,
        session_id: &str,
        intent: &HashMap<String, rmpv::Value>,
        identity: &str,
    ) -> OutgoingResult {
        let Some(mut session) = self.get_session(session_id, identity) else {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP Four in a Row] Session not found".into(),
            };
        };
        if !has_exact_keys(intent, &["c"]) {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP Four in a Row] Invalid move payload".into(),
            };
        }
        let Some(column) = intent
            .get("c")
            .and_then(value_as_u64)
            .filter(|column| *column < COLUMNS as u64)
        else {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP Four in a Row] Invalid column".into(),
            };
        };
        if session.status != STATUS_ACTIVE {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: format!(
                    "[LRGP Four in a Row] Session is not active ({})",
                    session.status
                ),
            };
        }
        if meta_str(&session.metadata, "turn") != identity {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP Four in a Row] Not your turn".into(),
            };
        }
        let move_num = (meta_i64(&session.metadata, "move_count") + 1) as u64;
        let old_board = meta_str(&session.metadata, "board");
        if !is_canonical_board(old_board.as_bytes()) {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP Four in a Row] Stored board is invalid".into(),
            };
        }
        let Some((_, cell)) = drop_cell(old_board.as_bytes(), column as usize) else {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: format!("[LRGP Four in a Row] Column {column} is full"),
            };
        };
        if session.contact_hash.is_empty() {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP Four in a Row] Opponent unknown".into(),
            };
        }
        let mut board = old_board.into_bytes();
        board[cell] = marker_for_move(move_num);
        let terminal = if board_winner(&board).is_some() {
            "win"
        } else if board_is_draw(&board) {
            "draw"
        } else {
            ""
        };
        let mut payload = HashMap::from([
            ("c".into(), rmpv::Value::Integer(column.into())),
            ("n".into(), rmpv::Value::Integer(move_num.into())),
            ("x".into(), rmpv::Value::String(terminal.into())),
        ]);
        if terminal == "win" {
            payload.insert("w".into(), rmpv::Value::String(identity.into()));
        }
        let _ = self.apply_move(&mut session, &payload, identity);
        self.save_session(&session);
        OutgoingResult {
            fallback_text: self.render_fallback_inner(CMD_MOVE, &payload),
            payload,
        }
    }

    fn handle_simple_out(&self, session_id: &str, command: &str, identity: &str) -> OutgoingResult {
        let (valid, message) =
            self.validate_outgoing_action(session_id, command, &HashMap::new(), identity);
        if !valid {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: format!(
                    "[LRGP Four in a Row] {}",
                    message.unwrap_or_else(|| "Action rejected".into())
                ),
            };
        }
        if let Some(mut session) = self.get_session(session_id, identity) {
            let _ = SessionStateMachine::apply_command(&mut session, command, false);
            match command {
                CMD_RESIGN => {
                    session
                        .metadata
                        .insert("turn".into(), JsonValue::String(String::new()));
                    session
                        .metadata
                        .insert("terminal".into(), JsonValue::String("resign".into()));
                    session.metadata.insert(
                        "winner".into(),
                        JsonValue::String(session.contact_hash.clone()),
                    );
                    session.clear_draw_offer();
                }
                CMD_DRAW_OFFER => session.set_draw_offer(identity),
                CMD_DRAW_ACCEPT => {
                    session
                        .metadata
                        .insert("turn".into(), JsonValue::String(String::new()));
                    session
                        .metadata
                        .insert("terminal".into(), JsonValue::String("draw".into()));
                    session.clear_draw_offer();
                }
                CMD_DRAW_DECLINE => session.clear_draw_offer(),
                _ => {}
            }
            self.save_session(&session);
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: self.render_fallback_inner(command, &HashMap::new()),
        }
    }

    fn render_fallback_inner(
        &self,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
    ) -> String {
        match command {
            CMD_CHALLENGE => "[LRGP Four in a Row] Sent a challenge!".into(),
            CMD_ACCEPT => "[LRGP Four in a Row] Challenge accepted".into(),
            CMD_DECLINE => "[LRGP Four in a Row] Challenge declined".into(),
            CMD_MOVE if payload.get("x").and_then(value_as_str) == Some("win") => {
                "[LRGP Four in a Row] Four in a row!".into()
            }
            CMD_MOVE if payload.get("x").and_then(value_as_str) == Some("draw") => {
                "[LRGP Four in a Row] Game drawn!".into()
            }
            CMD_MOVE => format!(
                "[LRGP Four in a Row] Move {}",
                payload.get("n").and_then(value_as_u64).unwrap_or(0)
            ),
            CMD_RESIGN => "[LRGP Four in a Row] Resigned.".into(),
            CMD_DRAW_OFFER => "[LRGP Four in a Row] Offered a draw".into(),
            CMD_DRAW_ACCEPT => "[LRGP Four in a Row] Draw accepted".into(),
            CMD_DRAW_DECLINE => "[LRGP Four in a Row] Draw declined".into(),
            other => format!("[LRGP Four in a Row] {other}"),
        }
    }
}

impl Default for FourInARowApp {
    fn default() -> Self {
        Self::new()
    }
}

impl GameApp for FourInARowApp {
    fn app_id(&self) -> &str {
        APP_ID
    }

    fn version(&self) -> u32 {
        APP_VERSION
    }

    fn manifest(&self) -> AppManifest {
        let actions: Vec<String> = vec![
            CMD_CHALLENGE.into(),
            CMD_ACCEPT.into(),
            CMD_DECLINE.into(),
            CMD_MOVE.into(),
            CMD_RESIGN.into(),
            CMD_DRAW_OFFER.into(),
            CMD_DRAW_ACCEPT.into(),
            CMD_DRAW_DECLINE.into(),
            CMD_ERROR.into(),
        ];
        let mut preferred_delivery = HashMap::new();
        for command in &actions {
            preferred_delivery.insert(command.clone(), "opportunistic".into());
        }
        for command in [CMD_RESIGN, CMD_DRAW_ACCEPT, CMD_DRAW_DECLINE] {
            preferred_delivery.insert(command.into(), "direct".into());
        }
        AppManifest {
            app_id: APP_ID.into(),
            version: APP_VERSION,
            display_name: "Four in a Row".into(),
            icon: APP_ID.into(),
            session_type: SESSION_TURN_BASED.into(),
            max_players: 2,
            validation: VALIDATION_BOTH.into(),
            actions,
            preferred_delivery,
            ttl: Self::ttl_policy(),
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
        if command != CMD_ERROR
            && let Err(message) =
                self.validate_wire_payload(session_id, command, payload, identity_id)
        {
            return error_result(ERR_PROTOCOL_ERROR, &message);
        }
        match command {
            CMD_CHALLENGE => self.handle_challenge_in(session_id, sender_hash, identity_id),
            CMD_ACCEPT => self.handle_accept_in(session_id, payload, sender_hash, identity_id),
            CMD_MOVE => self.handle_move_in(session_id, payload, sender_hash, identity_id),
            CMD_DECLINE | CMD_RESIGN | CMD_DRAW_OFFER | CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE => {
                self.handle_simple_in(session_id, command, sender_hash, identity_id)
            }
            CMD_ERROR => IncomingResult {
                session: None,
                emit: None,
                error: Some(
                    payload
                        .iter()
                        .map(|(key, value)| (key.clone(), rmpv_to_json(value)))
                        .collect(),
                ),
            },
            _ => error_result(ERR_PROTOCOL_ERROR, "Unknown command"),
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
            CMD_MOVE => self.handle_move_out(session_id, payload, identity_id),
            CMD_DECLINE | CMD_RESIGN | CMD_DRAW_OFFER | CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE => {
                self.handle_simple_out(session_id, command, identity_id)
            }
            _ => OutgoingResult {
                payload: payload.clone(),
                fallback_text: self.render_fallback_inner(command, payload),
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
        if let Err(message) = self.validate_wire_payload(session_id, command, payload, identity_id)
        {
            return (false, Some(message));
        }
        let Some(session) = self.get_session(session_id, identity_id) else {
            return if command == CMD_CHALLENGE {
                (true, None)
            } else {
                (false, Some("Session not found".into()))
            };
        };
        if command == CMD_MOVE {
            return match self.validate_move(&session, payload, identity_id) {
                Ok(_) => (true, None),
                Err(message) => (false, Some(message)),
            };
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
                if !has_exact_keys(payload, &["c"])
                    || payload.get("c").and_then(value_as_u64).is_none()
                {
                    return (
                        false,
                        Some("move intent must contain exactly integer c".into()),
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
            let Some(owner) = session.draw_offered_by() else {
                return (false, Some("No draw offer is outstanding".into()));
            };
            if owner == identity_id {
                return (false, Some("Cannot answer your own draw offer".into()));
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
        let column = payload.get("c").and_then(value_as_u64).unwrap();
        if column >= COLUMNS as u64 {
            return (false, Some("Invalid column".into()));
        }
        let board = meta_str(&session.metadata, "board");
        if !is_canonical_board(board.as_bytes()) {
            return (false, Some("Stored board is invalid".into()));
        }
        if drop_cell(board.as_bytes(), column as usize).is_none() {
            return (false, Some(format!("Column {column} is full")));
        }
        if session.contact_hash.is_empty() {
            return (false, Some("Opponent unknown".into()));
        }
        (true, None)
    }

    fn get_session_state(&self, session_id: &str, identity_id: &str) -> HashMap<String, JsonValue> {
        self.get_session(session_id, identity_id)
            .map(|session| session_to_json(&session))
            .unwrap_or_default()
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

    fn upsert_session(&self, mut session: Session) -> Result<(), LrgpError> {
        if session.app_id != APP_ID || session.app_version != APP_VERSION {
            return Err(LrgpError::Validation {
                code: ERR_UNSUPPORTED_APP.into(),
                message: "session app/version does not match Four in a Row".into(),
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
        // Older persisted records may have expired before the Four-specific
        // draw-offer metadata was normalized. Clear only that stale offer
        // before validation; non-expired records must still satisfy the
        // invariants of their recorded state before an expiry transition.
        Self::normalize_expired_session(&mut session);
        Self::validate_restored_session(&session)?;
        Self::check_session_expiry(&mut session, &Self::ttl_policy());
        self.save_session(&session);
        Ok(())
    }

    fn remove_session(&self, session_id: &str, identity_id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .remove(&(session_id.into(), identity_id.into()))
            .is_some()
    }

    fn list_session_records(&self, identity_id: Option<&str>) -> Vec<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        let ttl = Self::ttl_policy();
        sessions
            .values_mut()
            .filter(|session| identity_id.is_none_or(|id| session.identity_id == id))
            .map(|session| {
                Self::check_session_expiry(session, &ttl);
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
            .get_mut(&(session_id.into(), identity_id.into()))
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
            Some(session) => self.save_session(&session),
            None => {
                self.sessions
                    .lock()
                    .unwrap()
                    .remove(&(session_id.into(), identity_id.into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{pack_envelope, pack_to_bytes};

    const ALICE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn intent(column: u64) -> HashMap<String, rmpv::Value> {
        HashMap::from([("c".into(), rmpv::Value::Integer(column.into()))])
    }

    fn setup_active(session_id: &str) -> FourInARowApp {
        let app = FourInARowApp::new();
        app.handle_outgoing(session_id, CMD_CHALLENGE, &HashMap::new(), ALICE);
        app.bind_session_peer(session_id, ALICE, BOB).unwrap();
        let challenge = app.handle_incoming(session_id, CMD_CHALLENGE, &HashMap::new(), ALICE, BOB);
        assert!(challenge.error.is_none());
        let accept = app.handle_outgoing(session_id, CMD_ACCEPT, &HashMap::new(), BOB);
        assert_eq!(accept.payload.keys().collect::<Vec<_>>(), vec!["t"]);
        let accepted = app.handle_incoming(session_id, CMD_ACCEPT, &accept.payload, BOB, ALICE);
        assert!(accepted.error.is_none());
        app
    }

    fn play(
        app: &FourInARowApp,
        session_id: &str,
        mover: &str,
        receiver: &str,
        column: u64,
    ) -> HashMap<String, rmpv::Value> {
        let outgoing = app.handle_outgoing(session_id, CMD_MOVE, &intent(column), mover);
        assert!(
            !outgoing.payload.is_empty(),
            "outgoing move rejected: {}",
            outgoing.fallback_text
        );
        let incoming =
            app.handle_incoming(session_id, CMD_MOVE, &outgoing.payload, mover, receiver);
        assert!(incoming.error.is_none(), "{:?}", incoming.error);
        outgoing.payload
    }

    fn board_with(cells: &[(usize, usize, u8)]) -> Vec<u8> {
        let mut board = EMPTY_BOARD.as_bytes().to_vec();
        for &(row, column, marker) in cells {
            board[row * COLUMNS + column] = marker;
        }
        board
    }

    fn assert_shared_state(left: &Session, right: &Session) {
        let mut left = left.metadata.clone();
        let mut right = right.metadata.clone();
        left.remove("my_marker");
        right.remove("my_marker");
        assert_eq!(left, right);
    }

    const DRAW_SEQUENCE: [u64; 42] = [
        2, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 6, 4, 4, 4, 4, 4,
        4, 5, 5, 5, 5, 5, 5, 6, 6, 6, 6, 6,
    ];

    #[test]
    fn manifest_is_complete_and_theme_neutral() {
        let manifest = FourInARowApp::new().manifest();
        assert_eq!(manifest.app_id, APP_ID);
        assert_eq!(manifest.version, 1);
        assert_eq!(manifest.display_name, "Four in a Row");
        assert_eq!(manifest.icon, APP_ID);
        assert_eq!(manifest.session_type, SESSION_TURN_BASED);
        assert_eq!(manifest.validation, VALIDATION_BOTH);
        assert_eq!(manifest.max_players, 2);
        for action in [
            CMD_CHALLENGE,
            CMD_ACCEPT,
            CMD_DECLINE,
            CMD_MOVE,
            CMD_RESIGN,
            CMD_DRAW_OFFER,
            CMD_DRAW_ACCEPT,
            CMD_DRAW_DECLINE,
            CMD_ERROR,
        ] {
            assert!(manifest.actions.iter().any(|item| item == action));
            assert!(manifest.preferred_delivery.contains_key(action));
        }
        assert_eq!(manifest.ttl[STATUS_PENDING], TTL_PENDING);
        assert_eq!(manifest.ttl[STATUS_ACTIVE], TTL_ACTIVE);
    }

    #[test]
    fn detects_all_four_win_directions() {
        let horizontal = board_with(&[(5, 0, b'A'), (5, 1, b'A'), (5, 2, b'A'), (5, 3, b'A')]);
        let vertical = board_with(&[(2, 0, b'B'), (3, 0, b'B'), (4, 0, b'B'), (5, 0, b'B')]);
        let down_right = board_with(&[(2, 0, b'A'), (3, 1, b'A'), (4, 2, b'A'), (5, 3, b'A')]);
        let up_right = board_with(&[(5, 0, b'B'), (4, 1, b'B'), (3, 2, b'B'), (2, 3, b'B')]);
        assert_eq!(board_winner(&horizontal), Some(b'A'));
        assert_eq!(board_winner(&vertical), Some(b'B'));
        assert_eq!(board_winner(&down_right), Some(b'A'));
        assert_eq!(board_winner(&up_right), Some(b'B'));
        assert_eq!(board_winner(EMPTY_BOARD.as_bytes()), None);
    }

    #[test]
    fn gravity_selects_lowest_empty_cell_and_rejects_full_column() {
        let mut board = EMPTY_BOARD.as_bytes().to_vec();
        assert_eq!(drop_cell(&board, 3), Some((5, 38)));
        board[38] = b'A';
        assert_eq!(drop_cell(&board, 3), Some((4, 31)));
        for row in 0..ROWS {
            board[row * COLUMNS + 3] = if row % 2 == 0 { b'A' } else { b'B' };
        }
        assert_eq!(drop_cell(&board, 3), None);
        assert_eq!(drop_cell(&board, COLUMNS), None);
    }

    #[test]
    fn seven_moves_converge_and_complete_a_vertical_win() {
        let app = setup_active("1111111111111111");
        for (index, column) in [0, 1, 0, 1, 0, 1, 0].into_iter().enumerate() {
            let (mover, receiver) = if index % 2 == 0 {
                (ALICE, BOB)
            } else {
                (BOB, ALICE)
            };
            let payload = play(&app, "1111111111111111", mover, receiver, column);
            assert_eq!(
                value_as_u64(payload.get("n").unwrap()),
                Some(index as u64 + 1)
            );
            if index < 6 {
                assert_eq!(value_as_str(payload.get("x").unwrap()), Some(""));
                assert_eq!(payload.len(), 3);
            } else {
                assert_eq!(value_as_str(payload.get("x").unwrap()), Some("win"));
                assert_eq!(value_as_str(payload.get("w").unwrap()), Some(ALICE));
                assert_eq!(payload.len(), 4);
            }
        }
        let alice = app.get_session("1111111111111111", ALICE).unwrap();
        let bob = app.get_session("1111111111111111", BOB).unwrap();
        assert_eq!(alice.status, STATUS_COMPLETED);
        assert_shared_state(&alice, &bob);
        assert_eq!(
            meta_str(&alice.metadata, "board"),
            "______________A______AB_____AB_____AB_____"
        );
        assert_eq!(alice.metadata["last_column"], 0);
        assert_eq!(alice.metadata["last_row"], 2);
        assert_eq!(alice.metadata["last_cell"], 14);
        assert_eq!(meta_str(&alice.metadata, "winner"), ALICE);
        assert_eq!(meta_str(&alice.metadata, "terminal"), "win");
        assert!(meta_str(&alice.metadata, "turn").is_empty());
    }

    #[test]
    fn rejects_out_of_turn_invalid_and_full_column_intents_without_panicking() {
        let app = setup_active("2222222222222222");
        let wrong_turn = app.handle_outgoing("2222222222222222", CMD_MOVE, &intent(1), BOB);
        assert!(wrong_turn.payload.is_empty());
        assert!(wrong_turn.fallback_text.contains("Not your turn"));
        for index in 0..6 {
            let (mover, receiver) = if index % 2 == 0 {
                (ALICE, BOB)
            } else {
                (BOB, ALICE)
            };
            play(&app, "2222222222222222", mover, receiver, 2);
        }
        let full = app.handle_outgoing("2222222222222222", CMD_MOVE, &intent(2), ALICE);
        assert!(full.payload.is_empty());
        assert!(full.fallback_text.contains("full"));
        let invalid = app.handle_outgoing("2222222222222222", CMD_MOVE, &intent(7), ALICE);
        assert!(invalid.payload.is_empty());
        let wrong_type = HashMap::from([("c".into(), rmpv::Value::String("0".into()))]);
        assert!(
            app.handle_outgoing("2222222222222222", CMD_MOVE, &wrong_type, ALICE)
                .payload
                .is_empty()
        );
    }

    #[test]
    fn strict_wire_shapes_and_sequence_mismatches_do_not_mutate() {
        let app = setup_active("3333333333333333");
        let before = app.get_session("3333333333333333", BOB).unwrap();
        let cases = [
            HashMap::from([
                ("c".into(), rmpv::Value::Integer(0.into())),
                ("n".into(), rmpv::Value::Integer(1.into())),
            ]),
            HashMap::from([
                ("c".into(), rmpv::Value::String("0".into())),
                ("n".into(), rmpv::Value::Integer(1.into())),
                ("x".into(), rmpv::Value::String("".into())),
            ]),
            HashMap::from([
                ("c".into(), rmpv::Value::Integer(7.into())),
                ("n".into(), rmpv::Value::Integer(1.into())),
                ("x".into(), rmpv::Value::String("".into())),
            ]),
            HashMap::from([
                ("c".into(), rmpv::Value::Integer(0.into())),
                ("n".into(), rmpv::Value::Integer(1.into())),
                ("x".into(), rmpv::Value::String("victory".into())),
            ]),
            HashMap::from([
                ("c".into(), rmpv::Value::Integer(0.into())),
                ("n".into(), rmpv::Value::Integer(1.into())),
                ("x".into(), rmpv::Value::String("".into())),
                ("extra".into(), rmpv::Value::Nil),
            ]),
            HashMap::from([
                ("c".into(), rmpv::Value::Integer(0.into())),
                ("n".into(), rmpv::Value::Integer(2.into())),
                ("x".into(), rmpv::Value::String("".into())),
            ]),
            HashMap::from([
                ("c".into(), rmpv::Value::Integer(0.into())),
                ("n".into(), rmpv::Value::Integer(1.into())),
                ("x".into(), rmpv::Value::String("win".into())),
                ("w".into(), rmpv::Value::String(ALICE.into())),
            ]),
        ];
        for payload in cases {
            let result = app.handle_incoming("3333333333333333", CMD_MOVE, &payload, ALICE, BOB);
            assert!(result.error.is_some());
            assert_eq!(
                app.get_session("3333333333333333", BOB).unwrap().metadata,
                before.metadata
            );
        }
    }

    #[test]
    fn forged_winner_and_post_terminal_moves_are_rejected() {
        let app = setup_active("4444444444444444");
        for (index, column) in [0, 1, 0, 1, 0, 1].into_iter().enumerate() {
            let (mover, receiver) = if index % 2 == 0 {
                (ALICE, BOB)
            } else {
                (BOB, ALICE)
            };
            play(&app, "4444444444444444", mover, receiver, column);
        }
        let forged = HashMap::from([
            ("c".into(), rmpv::Value::Integer(0.into())),
            ("n".into(), rmpv::Value::Integer(7.into())),
            ("x".into(), rmpv::Value::String("win".into())),
            ("w".into(), rmpv::Value::String("mallory".into())),
        ]);
        let result = app.handle_incoming("4444444444444444", CMD_MOVE, &forged, ALICE, BOB);
        assert!(result.error.is_some());
        play(&app, "4444444444444444", ALICE, BOB, 0);
        let post_terminal = app.handle_outgoing("4444444444444444", CMD_MOVE, &intent(2), BOB);
        assert!(post_terminal.payload.is_empty());
        assert!(post_terminal.fallback_text.contains("not active"));
    }

    #[test]
    fn full_nonwinning_board_is_a_draw_and_converges() {
        let app = setup_active("5555555555555555");
        for (index, column) in DRAW_SEQUENCE.into_iter().enumerate() {
            let (mover, receiver) = if index % 2 == 0 {
                (ALICE, BOB)
            } else {
                (BOB, ALICE)
            };
            let payload = play(&app, "5555555555555555", mover, receiver, column);
            assert_eq!(
                value_as_str(payload.get("x").unwrap()),
                Some(if index == 41 { "draw" } else { "" })
            );
        }
        let alice = app.get_session("5555555555555555", ALICE).unwrap();
        let bob = app.get_session("5555555555555555", BOB).unwrap();
        assert_eq!(alice.status, STATUS_COMPLETED);
        assert_shared_state(&alice, &bob);
        assert_eq!(meta_str(&alice.metadata, "terminal"), "draw");
        assert!(meta_str(&alice.metadata, "winner").is_empty());
        assert_eq!(meta_i64(&alice.metadata, "move_count"), 42);
        assert!(board_is_draw(meta_str(&alice.metadata, "board").as_bytes()));
    }

    #[test]
    fn resign_and_draw_offer_lifecycle_stays_symmetric() {
        let draw_app = setup_active("6666666666666666");
        let offer =
            draw_app.handle_outgoing("6666666666666666", CMD_DRAW_OFFER, &HashMap::new(), ALICE);
        let offered = draw_app.handle_incoming(
            "6666666666666666",
            CMD_DRAW_OFFER,
            &offer.payload,
            ALICE,
            BOB,
        );
        assert!(offered.error.is_none());
        assert_eq!(
            draw_app
                .get_session("6666666666666666", BOB)
                .unwrap()
                .draw_offered_by(),
            Some(ALICE)
        );
        let accept =
            draw_app.handle_outgoing("6666666666666666", CMD_DRAW_ACCEPT, &HashMap::new(), BOB);
        let accepted = draw_app.handle_incoming(
            "6666666666666666",
            CMD_DRAW_ACCEPT,
            &accept.payload,
            BOB,
            ALICE,
        );
        assert!(accepted.error.is_none());
        for player in [ALICE, BOB] {
            let session = draw_app.get_session("6666666666666666", player).unwrap();
            assert_eq!(session.status, STATUS_COMPLETED);
            assert_eq!(meta_str(&session.metadata, "terminal"), "draw");
            assert!(!session.has_draw_offer());
        }

        let resign_app = setup_active("7777777777777777");
        let resign =
            resign_app.handle_outgoing("7777777777777777", CMD_RESIGN, &HashMap::new(), ALICE);
        let resigned =
            resign_app.handle_incoming("7777777777777777", CMD_RESIGN, &resign.payload, ALICE, BOB);
        assert!(resigned.error.is_none());
        for player in [ALICE, BOB] {
            let session = resign_app.get_session("7777777777777777", player).unwrap();
            assert_eq!(session.status, STATUS_COMPLETED);
            assert_eq!(meta_str(&session.metadata, "terminal"), "resign");
            assert_eq!(meta_str(&session.metadata, "winner"), BOB);
        }
    }

    #[test]
    fn challenge_decline_and_draw_decline_are_symmetric() {
        let challenge_app = FourInARowApp::new();
        challenge_app.handle_outgoing("1212121212121212", CMD_CHALLENGE, &HashMap::new(), ALICE);
        challenge_app
            .bind_session_peer("1212121212121212", ALICE, BOB)
            .unwrap();
        challenge_app.handle_incoming(
            "1212121212121212",
            CMD_CHALLENGE,
            &HashMap::new(),
            ALICE,
            BOB,
        );
        let declined =
            challenge_app.handle_outgoing("1212121212121212", CMD_DECLINE, &HashMap::new(), BOB);
        challenge_app.handle_incoming(
            "1212121212121212",
            CMD_DECLINE,
            &declined.payload,
            BOB,
            ALICE,
        );
        for player in [ALICE, BOB] {
            assert_eq!(
                challenge_app
                    .get_session("1212121212121212", player)
                    .unwrap()
                    .status,
                STATUS_DECLINED
            );
        }

        let draw_app = setup_active("1313131313131313");
        let offer =
            draw_app.handle_outgoing("1313131313131313", CMD_DRAW_OFFER, &HashMap::new(), ALICE);
        draw_app.handle_incoming(
            "1313131313131313",
            CMD_DRAW_OFFER,
            &offer.payload,
            ALICE,
            BOB,
        );
        let own_answer = draw_app.validate_outgoing_action(
            "1313131313131313",
            CMD_DRAW_DECLINE,
            &HashMap::new(),
            ALICE,
        );
        assert!(!own_answer.0);
        let decline =
            draw_app.handle_outgoing("1313131313131313", CMD_DRAW_DECLINE, &HashMap::new(), BOB);
        draw_app.handle_incoming(
            "1313131313131313",
            CMD_DRAW_DECLINE,
            &decline.payload,
            BOB,
            ALICE,
        );
        for player in [ALICE, BOB] {
            let session = draw_app.get_session("1313131313131313", player).unwrap();
            assert_eq!(session.status, STATUS_ACTIVE);
            assert!(!session.has_draw_offer());
        }
    }

    #[test]
    fn participant_authorization_and_snapshot_rollback_are_enforced() {
        let app = setup_active("8888888888888888");
        assert!(
            app.authorize_incoming("8888888888888888", CMD_MOVE, ALICE, BOB)
                .is_ok()
        );
        assert!(matches!(
            app.authorize_incoming("8888888888888888", CMD_MOVE, "mallory", BOB),
            Err(LrgpError::UnauthorizedPeer { .. })
        ));
        let snapshot = app.snapshot_session("8888888888888888", ALICE);
        let before = snapshot.as_ref().unwrap().metadata.clone();
        let moved = app.handle_outgoing("8888888888888888", CMD_MOVE, &intent(3), ALICE);
        assert!(!moved.payload.is_empty());
        app.rollback_session("8888888888888888", ALICE, snapshot);
        assert_eq!(
            app.get_session("8888888888888888", ALICE).unwrap().metadata,
            before
        );
    }

    #[test]
    fn hydration_is_semantic_and_applies_expiry() {
        let source = setup_active("9999999999999999");
        play(&source, "9999999999999999", ALICE, BOB, 3);
        let valid = source.get_session("9999999999999999", ALICE).unwrap();
        let restored = FourInARowApp::new();
        restored.upsert_session(valid.clone()).unwrap();
        assert_eq!(
            restored
                .get_session("9999999999999999", ALICE)
                .unwrap()
                .metadata,
            valid.metadata
        );

        let mut inverted_challenger = valid.clone();
        inverted_challenger.session_id = "8989898989898989".into();
        inverted_challenger.initiator = BOB.into();
        assert!(restored.upsert_session(inverted_challenger).is_err());

        let mut wrong_count = valid.clone();
        wrong_count
            .metadata
            .insert("move_count".into(), JsonValue::Number(2.into()));
        assert!(restored.upsert_session(wrong_count).is_err());

        let mut floating = valid.clone();
        let mut board = EMPTY_BOARD.as_bytes().to_vec();
        board[0] = b'A';
        floating.metadata.insert(
            "board".into(),
            JsonValue::String(String::from_utf8(board).unwrap()),
        );
        floating
            .metadata
            .insert("last_column".into(), JsonValue::Number(0.into()));
        floating
            .metadata
            .insert("last_row".into(), JsonValue::Number(0.into()));
        floating
            .metadata
            .insert("last_cell".into(), JsonValue::Number(0.into()));
        assert!(restored.upsert_session(floating).is_err());

        // A terminal board is valid only as a completed draw; merely changing
        // its status/claim back to active must not resurrect it.
        let drawn_source = setup_active("abababababababab");
        for (index, column) in DRAW_SEQUENCE.into_iter().enumerate() {
            let (mover, receiver) = if index % 2 == 0 {
                (ALICE, BOB)
            } else {
                (BOB, ALICE)
            };
            play(&drawn_source, "abababababababab", mover, receiver, column);
        }
        let full_draw = drawn_source.get_session("abababababababab", ALICE).unwrap();
        restored.upsert_session(full_draw.clone()).unwrap();
        let mut active_full_board = full_draw;
        active_full_board.session_id = "cdcdcdcdcdcdcdcd".into();
        active_full_board.status = STATUS_ACTIVE.into();
        active_full_board
            .metadata
            .insert("terminal".into(), JsonValue::String(String::new()));
        active_full_board
            .metadata
            .insert("turn".into(), JsonValue::String(ALICE.into()));
        assert!(restored.upsert_session(active_full_board).is_err());

        // A negotiated draw can legitimately end before the board is full.
        let mut negotiated_draw = valid.clone();
        negotiated_draw.session_id = "dededededededede".into();
        negotiated_draw.status = STATUS_COMPLETED.into();
        negotiated_draw
            .metadata
            .insert("turn".into(), JsonValue::String(String::new()));
        negotiated_draw
            .metadata
            .insert("terminal".into(), JsonValue::String("draw".into()));
        restored.upsert_session(negotiated_draw).unwrap();

        // Even shape-valid, gravity-valid state cannot contain both winners.
        let mut dual_winner = Session::new("efefefefefefefef");
        dual_winner.identity_id = ALICE.into();
        dual_winner.contact_hash = BOB.into();
        dual_winner.initiator = ALICE.into();
        dual_winner.app_id = APP_ID.into();
        dual_winner.app_version = APP_VERSION;
        dual_winner.status = STATUS_COMPLETED.into();
        dual_winner.metadata = default_metadata("A", ALICE);
        dual_winner.metadata.insert(
            "board".into(),
            JsonValue::String("____________________________BBBB___AAAA___".into()),
        );
        dual_winner
            .metadata
            .insert("move_count".into(), JsonValue::Number(8.into()));
        dual_winner
            .metadata
            .insert("last_column".into(), JsonValue::Number(3.into()));
        dual_winner
            .metadata
            .insert("last_row".into(), JsonValue::Number(4.into()));
        dual_winner
            .metadata
            .insert("last_cell".into(), JsonValue::Number(31.into()));
        dual_winner
            .metadata
            .insert("terminal".into(), JsonValue::String("win".into()));
        dual_winner
            .metadata
            .insert("winner".into(), JsonValue::String(BOB.into()));
        assert!(restored.upsert_session(dual_winner).is_err());

        let mut expired = valid;
        expired.last_action_at = 0.0;
        restored.upsert_session(expired).unwrap();
        assert_eq!(
            restored
                .get_session("9999999999999999", ALICE)
                .unwrap()
                .status,
            STATUS_EXPIRED
        );

        // Expiry must clear an outstanding offer so a serialized expired
        // record remains semantically valid when it is hydrated again.
        let offered_source = setup_active("7878787878787878");
        let offer = offered_source.handle_outgoing(
            "7878787878787878",
            CMD_DRAW_OFFER,
            &HashMap::new(),
            ALICE,
        );
        assert!(offer.payload.is_empty());
        let mut stale_offer = offered_source
            .get_session("7878787878787878", ALICE)
            .unwrap();
        stale_offer.last_action_at = 0.0;
        restored.upsert_session(stale_offer).unwrap();
        let normalized = restored.get_session("7878787878787878", ALICE).unwrap();
        assert_eq!(normalized.status, STATUS_EXPIRED);
        assert!(!normalized.has_draw_offer());
        assert!(normalized.draw_offered_by().is_none());

        let round_trip = FourInARowApp::new();
        round_trip.upsert_session(normalized.clone()).unwrap();

        // Accept legacy expired records that retained only the stale offer,
        // but normalize them before they are made observable again.
        let mut legacy_expired = normalized;
        legacy_expired.session_id = "6767676767676767".into();
        legacy_expired.set_draw_offer(ALICE);
        round_trip.upsert_session(legacy_expired).unwrap();
        let legacy_normalized = round_trip.get_session("6767676767676767", ALICE).unwrap();
        assert!(!legacy_normalized.has_draw_offer());
        assert!(legacy_normalized.draw_offered_by().is_none());
    }

    #[test]
    fn maximum_move_envelope_stays_below_wire_budget() {
        let payload = HashMap::from([
            ("c".into(), rmpv::Value::Integer(6.into())),
            ("n".into(), rmpv::Value::Integer(42.into())),
            ("x".into(), rmpv::Value::String("win".into())),
            ("w".into(), rmpv::Value::String(ALICE.into())),
        ]);
        let envelope = pack_envelope(
            APP_ID,
            APP_VERSION,
            CMD_MOVE,
            "abcdef0123456789",
            Some(payload),
            Some([0xff; NONCE_BYTES]),
        )
        .unwrap();
        let packed = pack_to_bytes(&envelope).unwrap();
        assert!(
            packed.len() <= ENVELOPE_MAX_PACKED,
            "{} bytes",
            packed.len()
        );
        assert!(packed.len() + LXMF_OVERHEAD <= OPPORTUNISTIC_MAX_CONTENT);
    }
}
