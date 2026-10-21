//! Shared engine for the single-token turn games: the stone-placement games
//! (Hex, Gomoku, Reversi), Dots and Boxes, and Mancala.
//!
//! Every move is one token, such as a square (`h8`), a line (`c3h`), or a pit
//! (`d`). The wire carries `{m, n, x}` plus `w` on a win and never a board;
//! both peers replay the move history from the starting position
//! (VALIDATION_BOTH). A game supplies only its rules through
//! [`PlacementRules`], including who moves next, so extra turns need no
//! message; this module owns the LRGP lifecycle.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Mutex;

use serde_json::Value as JsonValue;

use crate::app_base::{AppManifest, GameApp, IncomingResult, OutgoingResult};
use crate::constants::*;
use crate::envelope::{has_exact_keys, value_as_str, value_as_u64};
use crate::errors::LrgpError;
use crate::session::{Session, SessionStateMachine};

pub(crate) const R_RESIGN: &str = "rsn";
pub(crate) const R_AGREEMENT: &str = "agr";

// Wire payload keys.
const KEY_MOVE: &str = "m"; // move token, e.g. "h8", "swap", "c3h", or "d"
const KEY_PLY: &str = "n"; // ply counter (u32), 0-based
const KEY_TERMINAL: &str = "x"; // "" | "win" | "draw"
const KEY_WINNER: &str = "w"; // winner identity hash (terminal="win")
const KEY_BLACK: &str = "b"; // Black-player hash (ACCEPT payload)

pub(crate) const EMPTY: u8 = b'_';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Black,
    White,
}

impl Side {
    pub(crate) fn other(self) -> Self {
        match self {
            Side::Black => Side::White,
            Side::White => Side::Black,
        }
    }

    pub(crate) fn stone(self) -> u8 {
        match self {
            Side::Black => b'b',
            Side::White => b'w',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ongoing,
    Win(Side),
    Draw,
}

impl Outcome {
    fn marker(self) -> &'static str {
        match self {
            Outcome::Ongoing => "",
            Outcome::Win(_) => "win",
            Outcome::Draw => "draw",
        }
    }
}

/// Squares are `rank * size + file` with a1 = 0.
pub(crate) fn square_name(index: usize, size: usize) -> String {
    format!(
        "{}{}",
        (b'a' + (index % size) as u8) as char,
        index / size + 1
    )
}

/// Canonical square: lowercase file, then a decimal rank with no leading zero.
pub(crate) fn parse_square(text: &str, size: usize) -> Option<usize> {
    let (&file, rank) = text.as_bytes().split_first()?;
    let file = file.checked_sub(b'a')? as usize;
    if file >= size
        || rank.is_empty()
        || rank.len() > 2
        || rank[0] == b'0'
        || !rank.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let rank: usize = std::str::from_utf8(rank).ok()?.parse().ok()?;
    (1..=size).contains(&rank).then(|| (rank - 1) * size + file)
}

/// Row-major from the highest rank down, so rank 1 is the last row.
pub(crate) fn render_grid(cells: &[u8], size: usize) -> String {
    (0..size)
        .rev()
        .flat_map(|rank| (0..size).map(move |file| cells[rank * size + file] as char))
        .collect()
}

/// The rules a placement game supplies to [`PlacementApp`].
pub(crate) trait PlacementRules: Send + Sync + 'static {
    type Position: Clone;

    const APP_ID: &'static str;
    const DISPLAY_NAME: &'static str;
    /// The longest canonical move, for the envelope-budget test.
    #[cfg(test)]
    const LONGEST_MOVE: &'static str;
    /// Upper bound on game length, for the self-play test.
    #[cfg(test)]
    const MAX_PLIES: usize;
    /// Whether a move can end the game as a draw.
    const CAN_DRAW: bool;
    /// Local `terminal_reason` for a win or draw decided by the board.
    const WIN_REASON: &'static str;
    const DRAW_REASON: &'static str;

    fn start() -> Self::Position;
    fn side_to_move(position: &Self::Position) -> Side;
    /// `true` when `notation` is the canonical spelling of some move.
    fn is_canonical(notation: &str) -> bool;
    /// Legal moves in canonical order; empty once the game is over.
    fn legal_moves(position: &Self::Position) -> Vec<String>;
    /// Apply a canonical move to an ongoing position, or say why it is illegal.
    fn play(position: &mut Self::Position, notation: &str) -> Result<(), String>;
    fn outcome(position: &Self::Position) -> Outcome;
    fn render(position: &Self::Position) -> String;
    /// Game-specific derived metadata.
    fn extra_metadata(_position: &Self::Position, _metadata: &mut HashMap<String, JsonValue>) {}

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static Mutex<Option<bool>>;
}

/// Apply one move, rejecting moves after the game ended and non-canonical spellings.
pub(crate) fn play_checked<R: PlacementRules>(
    position: &mut R::Position,
    notation: &str,
) -> Result<(), String> {
    if R::outcome(position) != Outcome::Ongoing {
        return Err("The game is already over".into());
    }
    if !R::is_canonical(notation) {
        return Err(format!("Invalid move notation: {notation}"));
    }
    R::play(position, notation)
}

/// Replay move history from the starting position.
pub(crate) fn replay<R: PlacementRules>(moves: &[String]) -> Result<R::Position, String> {
    let mut position = R::start();
    for notation in moves {
        play_checked::<R>(&mut position, notation)?;
    }
    Ok(position)
}

/// `true` if the challenger gets Black. Pinned by each game's `force_coin` in tests.
fn flip_responder_coin<R: PlacementRules>() -> bool {
    #[cfg(any(test, feature = "test-helpers"))]
    {
        if let Ok(guard) = R::coin_pin().lock() {
            if let Some(v) = *guard {
                return v;
            }
        }
    }
    use rand::RngCore;
    (rand::thread_rng().next_u32() & 1) == 0
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

fn meta_str(meta: &HashMap<String, JsonValue>, key: &str) -> String {
    meta.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn meta_string_list(meta: &HashMap<String, JsonValue>, key: &str) -> Vec<String> {
    meta.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn string_list_value(v: &[String]) -> JsonValue {
    JsonValue::Array(v.iter().map(|s| JsonValue::String(s.clone())).collect())
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

/// Identity playing `side`, derived from the stored Black player.
fn player(session: &Session, side: Side) -> Option<String> {
    let black = meta_str(&session.metadata, "first_turn");
    if black.is_empty() {
        return None;
    }
    match side {
        Side::Black => Some(black),
        Side::White => other_player(session, &black),
    }
}

/// The LRGP application for one placement game.
pub(crate) struct PlacementApp<R: PlacementRules> {
    sessions: Mutex<HashMap<(String, String), Session>>,
    rules: PhantomData<R>,
}

impl<R: PlacementRules> PlacementApp<R> {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            rules: PhantomData,
        }
    }

    fn rejected(&self, text: impl std::fmt::Display) -> OutgoingResult {
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: format!("[LRGP {}] {text}", R::DISPLAY_NAME),
        }
    }

    fn emit_event(event_type: &str, session_id: &str, from: &str) -> HashMap<String, JsonValue> {
        let mut m = HashMap::new();
        m.insert("type".into(), JsonValue::String(event_type.into()));
        m.insert("session_id".into(), JsonValue::String(session_id.into()));
        m.insert("app_id".into(), JsonValue::String(R::APP_ID.into()));
        m.insert("from".into(), JsonValue::String(from.into()));
        m
    }

    fn ttl_policy() -> HashMap<String, f64> {
        let mut ttl = HashMap::new();
        ttl.insert(STATUS_PENDING.into(), TTL_PENDING);
        ttl.insert(STATUS_ACTIVE.into(), TTL_ACTIVE);
        ttl
    }

    /// Apply TTL policy; an expired game can no longer answer a draw offer.
    fn check_session_expiry(session: &mut Session, ttl: &HashMap<String, f64>) -> bool {
        let expired = SessionStateMachine::check_expiry(session, Some(ttl), None);
        if session.status == STATUS_EXPIRED {
            session.clear_draw_offer();
        }
        expired
    }

    pub(crate) fn get_session(&self, session_id: &str, identity_id: &str) -> Option<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(&(session_id.to_string(), identity_id.to_string()))?;
        Self::check_session_expiry(session, &Self::ttl_policy());
        Some(session.clone())
    }

    fn save_session(&self, session: &Session) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(
            (session.session_id.clone(), session.identity_id.clone()),
            session.clone(),
        );
    }

    fn default_metadata() -> HashMap<String, JsonValue> {
        let mut m = HashMap::new();
        m.insert("moves".into(), JsonValue::Array(vec![]));
        m.insert("my_color".into(), JsonValue::String("".into()));
        m.insert("first_turn".into(), JsonValue::String("".into()));
        m.insert("turn".into(), JsonValue::String("".into()));
        m.insert("move_count".into(), JsonValue::Number(0.into()));
        m.insert("last_move".into(), JsonValue::String("".into()));
        m.insert("winner".into(), JsonValue::String("".into()));
        m.insert("terminal".into(), JsonValue::String("".into()));
        m.insert("terminal_reason".into(), JsonValue::String("".into()));
        m.insert("draw_offered".into(), JsonValue::Bool(false));
        m.insert("draw_offered_by".into(), JsonValue::String("".into()));
        let start = R::start();
        m.insert("board".into(), JsonValue::String(R::render(&start)));
        m.insert("legal_moves".into(), JsonValue::Array(vec![]));
        R::extra_metadata(&start, &mut m);
        m
    }

    /// Refresh derived session metadata (board, legal_moves, game extras).
    fn refresh_derived(session: &mut Session, position: &R::Position) {
        session
            .metadata
            .insert("board".into(), JsonValue::String(R::render(position)));
        session.metadata.insert(
            "legal_moves".into(),
            string_list_value(&R::legal_moves(position)),
        );
        R::extra_metadata(position, &mut session.metadata);
    }

    /// Assign colors and reset the board when a challenge is accepted.
    fn start_game(session: &mut Session, black_hash: &str) {
        let my_color = if black_hash == session.identity_id {
            "b"
        } else {
            "w"
        };
        session
            .metadata
            .insert("moves".into(), JsonValue::Array(vec![]));
        session
            .metadata
            .insert("move_count".into(), JsonValue::Number(0.into()));
        session
            .metadata
            .insert("first_turn".into(), JsonValue::String(black_hash.into()));
        session
            .metadata
            .insert("turn".into(), JsonValue::String(black_hash.into()));
        session
            .metadata
            .insert("my_color".into(), JsonValue::String(my_color.into()));
        Self::refresh_derived(session, &R::start());
    }

    /// Record a move already applied to `position` by `mover`.
    fn record_move(
        session: &mut Session,
        position: &R::Position,
        notation: &str,
    ) -> Result<(), String> {
        let outcome = R::outcome(position);
        let (turn, winner, reason) = match outcome {
            Outcome::Ongoing => (
                player(session, R::side_to_move(position))
                    .ok_or_else(|| "Opponent unknown".to_string())?,
                String::new(),
                "",
            ),
            Outcome::Win(side) => (
                String::new(),
                player(session, side).ok_or_else(|| "Opponent unknown".to_string())?,
                R::WIN_REASON,
            ),
            Outcome::Draw => (String::new(), String::new(), R::DRAW_REASON),
        };
        let mut moves = meta_string_list(&session.metadata, "moves");
        moves.push(notation.to_string());

        session
            .metadata
            .insert("moves".into(), string_list_value(&moves));
        session.metadata.insert(
            "move_count".into(),
            JsonValue::Number((moves.len() as u64).into()),
        );
        session
            .metadata
            .insert("turn".into(), JsonValue::String(turn));
        session
            .metadata
            .insert("last_move".into(), JsonValue::String(notation.to_string()));
        session.metadata.insert(
            "terminal".into(),
            JsonValue::String(outcome.marker().to_string()),
        );
        session.metadata.insert(
            "terminal_reason".into(),
            JsonValue::String(reason.to_string()),
        );
        session
            .metadata
            .insert("winner".into(), JsonValue::String(winner));
        session.clear_draw_offer();
        Self::refresh_derived(session, position);

        SessionStateMachine::apply_command(session, CMD_MOVE, outcome != Outcome::Ongoing)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Record a non-move game end (resignation or agreement).
    fn finish(session: &mut Session, terminal: &str, reason: &str, winner: &str) {
        session
            .metadata
            .insert("terminal".into(), JsonValue::String(terminal.into()));
        session
            .metadata
            .insert("terminal_reason".into(), JsonValue::String(reason.into()));
        session
            .metadata
            .insert("winner".into(), JsonValue::String(winner.into()));
        session
            .metadata
            .insert("turn".into(), JsonValue::String("".into()));
        session
            .metadata
            .insert("legal_moves".into(), JsonValue::Array(vec![]));
        session.clear_draw_offer();
    }

    fn validate_incoming_payload(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
        sender_hash: &str,
    ) -> Result<(), String> {
        match command {
            CMD_CHALLENGE | CMD_DECLINE | CMD_RESIGN | CMD_DRAW_OFFER | CMD_DRAW_ACCEPT
            | CMD_DRAW_DECLINE => {
                if !payload.is_empty() {
                    return Err(format!("{command} payload must be empty"));
                }
            }
            CMD_ACCEPT => {
                if !has_exact_keys(payload, &[KEY_BLACK]) {
                    return Err("accept payload must contain exactly b".into());
                }
                let black = payload
                    .get(KEY_BLACK)
                    .and_then(value_as_str)
                    .ok_or_else(|| "accept b must be a string".to_string())?;
                if black != identity_id && black != sender_hash {
                    return Err("accept b is not a bound participant".into());
                }
            }
            CMD_MOVE => {
                let terminal = payload
                    .get(KEY_TERMINAL)
                    .and_then(value_as_str)
                    .ok_or_else(|| "move x must be a string".to_string())?;
                let expected: &[&str] = match terminal {
                    "" => &[KEY_MOVE, KEY_PLY, KEY_TERMINAL],
                    "win" => &[KEY_MOVE, KEY_PLY, KEY_TERMINAL, KEY_WINNER],
                    "draw" if R::CAN_DRAW => &[KEY_MOVE, KEY_PLY, KEY_TERMINAL],
                    other => return Err(format!("unsupported terminal marker '{other}'")),
                };
                if !has_exact_keys(payload, expected) {
                    return Err(format!(
                        "move payload has invalid keys for terminal marker '{terminal}'"
                    ));
                }
                if payload
                    .get(KEY_MOVE)
                    .and_then(value_as_str)
                    .filter(|notation| R::is_canonical(notation))
                    .is_none()
                    || payload.get(KEY_PLY).and_then(value_as_u64).is_none()
                {
                    return Err("move payload contains a value with the wrong type".into());
                }
                if terminal == "win"
                    && payload
                        .get(KEY_WINNER)
                        .and_then(value_as_str)
                        .filter(|winner| !winner.is_empty())
                        .is_none()
                {
                    return Err("winning move w must be a non-empty string".into());
                }
            }
            CMD_ERROR => {}
            _ => return Err(format!("unsupported command '{command}'")),
        }
        if command != CMD_CHALLENGE && self.get_session(session_id, identity_id).is_none() {
            return Err("Unknown session".into());
        }
        Ok(())
    }

    /// Validate an inbound wire move and return the position with it applied.
    pub(crate) fn validate_move(
        &self,
        session: &Session,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
    ) -> Result<R::Position, String> {
        if session.status != STATUS_ACTIVE {
            return Err(format!("Session is not active (status={})", session.status));
        }

        // Empty turn on an active session is invalid state — fail closed.
        let turn = meta_str(&session.metadata, "turn");
        if turn.is_empty() {
            return Err("Turn is required before moves".into());
        }
        if turn != sender_hash {
            return Err("Not your turn".into());
        }

        let notation = payload
            .get(KEY_MOVE)
            .and_then(value_as_str)
            .ok_or_else(|| "Missing move".to_string())?;
        let ply = payload
            .get(KEY_PLY)
            .and_then(value_as_u64)
            .ok_or_else(|| "Missing ply index".to_string())?;
        let claimed_terminal = payload
            .get(KEY_TERMINAL)
            .and_then(value_as_str)
            .ok_or_else(|| "Missing terminal marker".to_string())?;
        let claimed_winner = payload.get(KEY_WINNER).and_then(value_as_str).unwrap_or("");

        // SPEC: `n` is 0-based, so the next move's ply equals the count of
        // moves already applied.
        let moves = meta_string_list(&session.metadata, "moves");
        let expected_ply = moves.len() as u64;
        if ply != expected_ply {
            return Err(format!("Ply mismatch: expected {expected_ply}, got {ply}"));
        }

        let mut position = replay::<R>(&moves).map_err(|e| format!("Local replay failed: {e}"))?;
        play_checked::<R>(&mut position, notation)?;
        let outcome = R::outcome(&position);

        if outcome.marker() != claimed_terminal {
            return Err(format!(
                "Terminal mismatch: computed='{}' claimed='{claimed_terminal}'",
                outcome.marker()
            ));
        }
        match outcome {
            Outcome::Win(side) => {
                let winner = player(session, side).ok_or_else(|| "Opponent unknown".to_string())?;
                if claimed_winner != winner {
                    return Err(format!(
                        "Winner mismatch: computed='{winner}' claimed='{claimed_winner}'"
                    ));
                }
            }
            _ if !claimed_winner.is_empty() => {
                return Err("Winner must be empty unless the move is a win".into());
            }
            _ => {}
        }
        if outcome == Outcome::Ongoing && player(session, R::side_to_move(&position)).is_none() {
            return Err("Opponent unknown".into());
        }

        Ok(position)
    }

    /// Semantic hydration check: the stored history must replay to the
    /// stored board, and turn, colors, and terminal state must agree with it.
    fn validate_restored_session(session: &Session) -> Result<(), LrgpError> {
        let invalid = |message: &str| LrgpError::Validation {
            code: ERR_PROTOCOL_ERROR.into(),
            message: message.into(),
        };
        let meta = &session.metadata;
        let text = |key: &str| {
            meta.get(key)
                .and_then(JsonValue::as_str)
                .ok_or_else(|| invalid(&format!("restored {key} must be a string")))
        };

        let moves = meta
            .get("moves")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| invalid("restored moves must be an array"))?
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| invalid("restored moves must be strings"))?;
        let position = replay::<R>(&moves)
            .map_err(|e| invalid(&format!("restored move history is invalid: {e}")))?;
        if text("board")? != R::render(&position) {
            return Err(invalid("restored board does not match the move history"));
        }
        if meta.get("move_count").and_then(JsonValue::as_u64) != Some(moves.len() as u64) {
            return Err(invalid(
                "restored move_count does not match the move history",
            ));
        }
        if text("last_move")? != moves.last().map_or("", String::as_str) {
            return Err(invalid(
                "restored last_move does not match the move history",
            ));
        }

        let first_turn = text("first_turn")?;
        let my_color = text("my_color")?;
        let turn = text("turn")?;
        let terminal = text("terminal")?;
        let reason = text("terminal_reason")?;
        let winner = text("winner")?;

        match session.status.as_str() {
            STATUS_PENDING | STATUS_DECLINED => {
                if !moves.is_empty()
                    || !first_turn.is_empty()
                    || !my_color.is_empty()
                    || !turn.is_empty()
                    || !terminal.is_empty()
                    || !winner.is_empty()
                {
                    return Err(invalid(
                        "pending or declined session contains game progress",
                    ));
                }
            }
            STATUS_ACTIVE | STATUS_COMPLETED | STATUS_EXPIRED => {
                if first_turn != session.identity_id && first_turn != session.contact_hash {
                    return Err(invalid("restored first_turn is not a bound participant"));
                }
                let expected_color = if first_turn == session.identity_id {
                    "b"
                } else {
                    "w"
                };
                if my_color != expected_color {
                    return Err(invalid("restored my_color does not match first_turn"));
                }
                if other_player(session, first_turn).is_none() {
                    return Err(invalid("restored session has no bound opponent"));
                }
                let outcome = R::outcome(&position);
                let to_move = player(session, R::side_to_move(&position)).unwrap_or_default();

                match session.status.as_str() {
                    STATUS_ACTIVE => {
                        if turn != to_move
                            || !terminal.is_empty()
                            || !reason.is_empty()
                            || !winner.is_empty()
                            || outcome != Outcome::Ongoing
                        {
                            return Err(invalid(
                                "active session metadata is terminal or out of turn",
                            ));
                        }
                    }
                    STATUS_EXPIRED => {
                        if !terminal.is_empty()
                            || !reason.is_empty()
                            || !winner.is_empty()
                            || outcome != Outcome::Ongoing
                            || (!turn.is_empty() && turn != to_move)
                        {
                            return Err(invalid("expired session contains terminal metadata"));
                        }
                    }
                    _ => {
                        if !turn.is_empty() {
                            return Err(invalid("completed session must not have a turn"));
                        }
                        let consistent = match (terminal, reason, outcome) {
                            ("win", r, Outcome::Win(side)) if r == R::WIN_REASON => {
                                player(session, side).as_deref() == Some(winner)
                            }
                            ("draw", r, Outcome::Draw) if r == R::DRAW_REASON => winner.is_empty(),
                            ("win", R_RESIGN, Outcome::Ongoing) => {
                                winner == session.identity_id || winner == session.contact_hash
                            }
                            ("draw", R_AGREEMENT, Outcome::Ongoing) => winner.is_empty(),
                            _ => false,
                        };
                        if !consistent {
                            return Err(invalid(
                                "completed session terminal metadata does not match the board",
                            ));
                        }
                    }
                }
            }
            _ => return Err(invalid("restored session has an unknown status")),
        }

        if let Some(owner) = session.draw_offered_by() {
            if session.status != STATUS_ACTIVE
                || (owner != session.identity_id && owner != session.contact_hash)
            {
                return Err(invalid(
                    "restored draw offer has an invalid owner or status",
                ));
            }
        }
        Ok(())
    }

    fn handle_challenge_in(
        &self,
        session_id: &str,
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
        session.app_id = R::APP_ID.to_string();
        session.app_version = 1;
        session.contact_hash = sender_hash.to_string();
        session.initiator = sender_hash.to_string();
        session.status = STATUS_PENDING.to_string();
        session.metadata = Self::default_metadata();
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(Self::emit_event("challenge", session_id, sender_hash)),
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

        if let Err(e) = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false) {
            return error_result(ERR_PROTOCOL_ERROR, &e.to_string());
        }

        // Responder is authoritative on color assignment via the `b` field.
        let black_hash = payload
            .get(KEY_BLACK)
            .and_then(value_as_str)
            .unwrap_or("")
            .to_string();
        if black_hash != identity_id && black_hash != sender_hash {
            return error_result(
                ERR_PROTOCOL_ERROR,
                "ACCEPT black-player hash is not one of the bound participants",
            );
        }
        Self::start_game(&mut session, &black_hash);

        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(Self::emit_event("accept", session_id, sender_hash)),
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

        let position = match self.validate_move(&session, payload, sender_hash) {
            Ok(position) => position,
            Err(message) => {
                let mut m = HashMap::new();
                m.insert("code".into(), JsonValue::String(ERR_INVALID_MOVE.into()));
                m.insert("msg".into(), JsonValue::String(message));
                m.insert("ref".into(), JsonValue::String(CMD_MOVE.into()));
                return IncomingResult {
                    session: Some(session_to_json(&session)),
                    emit: None,
                    error: Some(m),
                };
            }
        };

        let notation = payload
            .get(KEY_MOVE)
            .and_then(value_as_str)
            .unwrap_or("")
            .to_string();
        if let Err(error) = Self::record_move(&mut session, &position, &notation) {
            return error_result(ERR_PROTOCOL_ERROR, &error);
        }
        session.unread = 1;
        self.save_session(&session);

        let mut emit = Self::emit_event("move", session_id, sender_hash);
        emit.insert(
            "payload".into(),
            JsonValue::Object(
                payload
                    .iter()
                    .map(|(k, v)| (k.clone(), rmpv_to_json(v)))
                    .collect(),
            ),
        );

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit),
            error: None,
        }
    }

    /// Decline, resign, draw offer, draw accept, and draw decline.
    fn handle_simple_in(
        &self,
        session_id: &str,
        command: &str,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => return error_result(ERR_PROTOCOL_ERROR, "Unknown session"),
        };

        if command == CMD_DRAW_OFFER && session.has_draw_offer() {
            return error_result(ERR_PROTOCOL_ERROR, "A draw offer is already outstanding");
        }
        if matches!(command, CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE) {
            let Some(offered_by) = session.draw_offered_by() else {
                return error_result(ERR_PROTOCOL_ERROR, "No draw offer is outstanding");
            };
            if offered_by == sender_hash {
                return error_result(
                    ERR_PROTOCOL_ERROR,
                    "A participant cannot answer its own draw offer",
                );
            }
        }

        if let Err(error) = SessionStateMachine::apply_command(&mut session, command, false) {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        match command {
            CMD_DECLINE if session.initiator == sender_hash => {
                session
                    .metadata
                    .insert("cancelled_by_initiator".into(), JsonValue::Bool(true));
            }
            // Sender resigned; opponent (= identity_id locally) wins.
            CMD_RESIGN => Self::finish(&mut session, "win", R_RESIGN, identity_id),
            CMD_DRAW_OFFER => session.set_draw_offer(sender_hash),
            CMD_DRAW_ACCEPT => Self::finish(&mut session, "draw", R_AGREEMENT, ""),
            CMD_DRAW_DECLINE => session.clear_draw_offer(),
            _ => {}
        }
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(Self::emit_event(command, session_id, sender_hash)),
            error: None,
        }
    }

    fn handle_challenge_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        let sid = if session_id.is_empty() {
            gen_session_id()
        } else {
            session_id.to_string()
        };

        let mut session = Session::new(&sid);
        session.identity_id = identity_id.to_string();
        session.app_id = R::APP_ID.to_string();
        session.app_version = 1;
        session.initiator = identity_id.to_string();
        session.status = STATUS_PENDING.to_string();
        session.metadata = Self::default_metadata();
        self.save_session(&session);

        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: self.render_fallback_inner(CMD_CHALLENGE, &HashMap::new()),
        }
    }

    fn handle_accept_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        let Some(mut session) = self.get_session(session_id, identity_id) else {
            return self.rejected("Session not found");
        };
        if let Err(error) = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false) {
            return self.rejected(error);
        }

        // Responder coin-flips Black; ships the Black-player hash as `b`.
        let black_hash = if flip_responder_coin::<R>() {
            session.initiator.clone()
        } else {
            identity_id.to_string()
        };
        Self::start_game(&mut session, &black_hash);
        self.save_session(&session);

        let mut payload = HashMap::new();
        payload.insert(
            KEY_BLACK.to_string(),
            rmpv::Value::String(black_hash.into()),
        );

        OutgoingResult {
            payload,
            fallback_text: self.render_fallback_inner(CMD_ACCEPT, &HashMap::new()),
        }
    }

    fn handle_move_out(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> OutgoingResult {
        let Some(mut session) = self.get_session(session_id, identity_id) else {
            return self.rejected("Session not found");
        };

        // Empty payload sentinel → dashboard surfaces game_action_result{ok:false}.
        if session.status != STATUS_ACTIVE {
            return self.rejected(format!("Session is not active ({})", session.status));
        }
        if meta_str(&session.metadata, "turn") != identity_id {
            return self.rejected("Not your turn");
        }
        let notation = payload
            .get(KEY_MOVE)
            .and_then(value_as_str)
            .unwrap_or("")
            .to_string();

        let moves = meta_string_list(&session.metadata, "moves");
        let mut position = match replay::<R>(&moves) {
            Ok(position) => position,
            Err(e) => return self.rejected(format!("Local replay failed: {e}")),
        };
        if let Err(e) = play_checked::<R>(&mut position, &notation) {
            return self.rejected(format!("Illegal move: {e}"));
        }
        // SPEC: `n` is 0-based (0 = Black's first move) — count before push.
        let ply = moves.len() as u64;
        let outcome = R::outcome(&position);

        if let Err(e) = Self::record_move(&mut session, &position, &notation) {
            return self.rejected(e);
        }
        self.save_session(&session);

        let mut enriched = HashMap::new();
        enriched.insert(KEY_MOVE.to_string(), rmpv::Value::String(notation.into()));
        enriched.insert(KEY_PLY.to_string(), rmpv::Value::Integer(ply.into()));
        enriched.insert(
            KEY_TERMINAL.to_string(),
            rmpv::Value::String(outcome.marker().into()),
        );
        if outcome != Outcome::Ongoing && outcome != Outcome::Draw {
            let winner = meta_str(&session.metadata, "winner");
            enriched.insert(KEY_WINNER.to_string(), rmpv::Value::String(winner.into()));
        }

        let fallback = self.render_fallback_inner(CMD_MOVE, &enriched);
        OutgoingResult {
            payload: enriched,
            fallback_text: fallback,
        }
    }

    /// Decline, resign, draw offer, draw accept, and draw decline.
    fn handle_simple_out(
        &self,
        session_id: &str,
        command: &str,
        identity_id: &str,
    ) -> OutgoingResult {
        let mut cancelled = false;
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            if SessionStateMachine::apply_command(&mut session, command, false).is_ok() {
                match command {
                    CMD_DECLINE if session.initiator == identity_id => {
                        session
                            .metadata
                            .insert("cancelled_by_initiator".into(), JsonValue::Bool(true));
                        cancelled = true;
                    }
                    CMD_RESIGN => {
                        let winner = session.contact_hash.clone();
                        Self::finish(&mut session, "win", R_RESIGN, &winner);
                    }
                    CMD_DRAW_OFFER => session.set_draw_offer(identity_id),
                    CMD_DRAW_ACCEPT => Self::finish(&mut session, "draw", R_AGREEMENT, ""),
                    CMD_DRAW_DECLINE => session.clear_draw_offer(),
                    _ => {}
                }
                self.save_session(&session);
            }
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: if cancelled {
                format!("[LRGP {}] Challenge cancelled", R::DISPLAY_NAME)
            } else {
                self.render_fallback_inner(command, &HashMap::new())
            },
        }
    }

    fn render_fallback_inner(
        &self,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
    ) -> String {
        let prefix = format!("[LRGP {}]", R::DISPLAY_NAME);
        match command {
            CMD_CHALLENGE => format!("{prefix} Sent a challenge!"),
            CMD_ACCEPT => format!("{prefix} Challenge accepted"),
            CMD_DECLINE => format!("{prefix} Challenge declined"),
            CMD_MOVE => {
                let notation = payload.get(KEY_MOVE).and_then(value_as_str).unwrap_or("");
                match payload.get(KEY_TERMINAL).and_then(value_as_str) {
                    Some("win") => format!("{prefix} {notation} (game won)"),
                    Some("draw") => format!("{prefix} {notation} (game drawn)"),
                    _ => format!("{prefix} {notation}"),
                }
            }
            CMD_RESIGN => format!("{prefix} Resigned."),
            CMD_DRAW_OFFER => format!("{prefix} Offered a draw"),
            CMD_DRAW_ACCEPT => format!("{prefix} Draw accepted"),
            CMD_DRAW_DECLINE => format!("{prefix} Draw declined"),
            CMD_ERROR => {
                let msg = payload
                    .get("msg")
                    .and_then(value_as_str)
                    .unwrap_or("Unknown");
                format!("{prefix} Error: {msg}")
            }
            other => format!("{prefix} {other}"),
        }
    }
}

impl<R: PlacementRules> GameApp for PlacementApp<R> {
    fn app_id(&self) -> &str {
        R::APP_ID
    }

    fn version(&self) -> u32 {
        1
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
        let preferred_delivery = actions
            .iter()
            .map(|command| (command.clone(), self.get_delivery_method(command)))
            .collect();

        AppManifest {
            app_id: R::APP_ID.into(),
            version: 1,
            display_name: R::DISPLAY_NAME.into(),
            icon: R::APP_ID.into(),
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
        if command != CMD_ERROR {
            if let Err(message) = self.validate_incoming_payload(
                session_id,
                command,
                payload,
                identity_id,
                sender_hash,
            ) {
                return error_result(ERR_PROTOCOL_ERROR, &message);
            }
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
        let Some(mut session) = self.get_session(session_id, identity_id) else {
            return if command == CMD_CHALLENGE {
                (true, None)
            } else {
                (false, Some("Session not found".into()))
            };
        };

        if Self::check_session_expiry(&mut session, &Self::ttl_policy()) {
            self.save_session(&session);
            return (false, Some("Session expired".into()));
        }

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
                if !has_exact_keys(payload, &[KEY_MOVE])
                    || payload.get(KEY_MOVE).and_then(value_as_str).is_none()
                {
                    return (
                        false,
                        Some("move intent must contain exactly string m".into()),
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
        if session.contact_hash.is_empty() {
            return (false, Some("Opponent unknown".into()));
        }
        let notation = payload.get(KEY_MOVE).and_then(value_as_str).unwrap_or("");
        let moves = meta_string_list(&session.metadata, "moves");
        let mut position = match replay::<R>(&moves) {
            Ok(position) => position,
            Err(error) => return (false, Some(format!("Local replay failed: {error}"))),
        };
        match play_checked::<R>(&mut position, notation) {
            Ok(()) => (true, None),
            Err(error) => (false, Some(error)),
        }
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
        if session.app_id != R::APP_ID || session.app_version != 1 {
            return Err(LrgpError::Validation {
                code: ERR_UNSUPPORTED_APP.into(),
                message: format!("session app/version does not match {}", R::DISPLAY_NAME),
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
        // A legacy offer flag without an owner, or an offer on a game that
        // already expired, is not answerable.
        if session.draw_offered_by().is_none() || session.status == STATUS_EXPIRED {
            session.clear_draw_offer();
        }
        Self::validate_restored_session(&session)?;
        if session.status == STATUS_ACTIVE {
            let moves = meta_string_list(&session.metadata, "moves");
            if let Ok(position) = replay::<R>(&moves) {
                Self::refresh_derived(&mut session, &position);
            }
        }
        Self::check_session_expiry(&mut session, &Self::ttl_policy());
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

/// Declare a public placement-game app that delegates to [`PlacementApp`].
macro_rules! placement_app {
    ($(#[$doc:meta])* $name:ident, $rules:ty) => {
        $(#[$doc])*
        pub struct $name(crate::apps::placement::PlacementApp<$rules>);

        impl $name {
            pub fn new() -> Self {
                Self(crate::apps::placement::PlacementApp::new())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl crate::app_base::GameApp for $name {
            fn app_id(&self) -> &str {
                self.0.app_id()
            }

            fn version(&self) -> u32 {
                self.0.version()
            }

            fn manifest(&self) -> crate::app_base::AppManifest {
                self.0.manifest()
            }

            fn handle_incoming(
                &self,
                session_id: &str,
                command: &str,
                payload: &std::collections::HashMap<String, rmpv::Value>,
                sender_hash: &str,
                identity_id: &str,
            ) -> crate::app_base::IncomingResult {
                self.0
                    .handle_incoming(session_id, command, payload, sender_hash, identity_id)
            }

            fn handle_outgoing(
                &self,
                session_id: &str,
                command: &str,
                payload: &std::collections::HashMap<String, rmpv::Value>,
                identity_id: &str,
            ) -> crate::app_base::OutgoingResult {
                self.0
                    .handle_outgoing(session_id, command, payload, identity_id)
            }

            fn validate_action(
                &self,
                session_id: &str,
                command: &str,
                payload: &std::collections::HashMap<String, rmpv::Value>,
                identity_id: &str,
            ) -> (bool, Option<String>) {
                self.0
                    .validate_action(session_id, command, payload, identity_id)
            }

            fn validate_outgoing_action(
                &self,
                session_id: &str,
                command: &str,
                payload: &std::collections::HashMap<String, rmpv::Value>,
                identity_id: &str,
            ) -> (bool, Option<String>) {
                self.0
                    .validate_outgoing_action(session_id, command, payload, identity_id)
            }

            fn get_session_state(
                &self,
                session_id: &str,
                identity_id: &str,
            ) -> std::collections::HashMap<String, serde_json::Value> {
                self.0.get_session_state(session_id, identity_id)
            }

            fn render_fallback(
                &self,
                command: &str,
                payload: &std::collections::HashMap<String, rmpv::Value>,
            ) -> String {
                self.0.render_fallback(command, payload)
            }

            fn get_delivery_method(&self, command: &str) -> String {
                self.0.get_delivery_method(command)
            }

            fn get_session_record(
                &self,
                session_id: &str,
                identity_id: &str,
            ) -> Option<crate::session::Session> {
                self.0.get_session_record(session_id, identity_id)
            }

            fn upsert_session(
                &self,
                session: crate::session::Session,
            ) -> Result<(), crate::errors::LrgpError> {
                self.0.upsert_session(session)
            }

            fn remove_session(&self, session_id: &str, identity_id: &str) -> bool {
                self.0.remove_session(session_id, identity_id)
            }

            fn list_session_records(
                &self,
                identity_id: Option<&str>,
            ) -> Vec<crate::session::Session> {
                self.0.list_session_records(identity_id)
            }

            fn bind_session_peer(
                &self,
                session_id: &str,
                identity_id: &str,
                peer_hash: &str,
            ) -> Result<(), crate::errors::LrgpError> {
                self.0.bind_session_peer(session_id, identity_id, peer_hash)
            }

            fn authorize_incoming(
                &self,
                session_id: &str,
                command: &str,
                sender_hash: &str,
                identity_id: &str,
            ) -> Result<(), crate::errors::LrgpError> {
                self.0
                    .authorize_incoming(session_id, command, sender_hash, identity_id)
            }

            fn snapshot_session(
                &self,
                session_id: &str,
                identity_id: &str,
            ) -> Option<crate::session::Session> {
                self.0.snapshot_session(session_id, identity_id)
            }

            fn rollback_session(
                &self,
                session_id: &str,
                identity_id: &str,
                snapshot: Option<crate::session::Session>,
            ) {
                self.0.rollback_session(session_id, identity_id, snapshot)
            }
        }
    };
}
pub(crate) use placement_app;

/// Declare a test-only coin pin for one placement game.
macro_rules! placement_coin {
    () => {
        #[cfg(any(test, feature = "test-helpers"))]
        static FORCED_COIN: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

        /// Pin the responder coin-flip for tests (`true` = challenger is Black).
        /// Process-global; callers must serialize.
        #[cfg(any(test, feature = "test-helpers"))]
        pub fn force_coin(challenger_is_black: Option<bool>) {
            if let Ok(mut guard) = FORCED_COIN.lock() {
                *guard = challenger_is_black;
            }
        }
    };
}
pub(crate) use placement_coin;

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

/// Instantiate every shared lifecycle test for one placement game.
#[cfg(test)]
macro_rules! placement_lifecycle_tests {
    ($rules:ty) => {
        mod lifecycle {
            use crate::apps::placement::harness;

            #[test]
            fn manifest_is_complete() {
                harness::manifest_is_complete::<$rules>();
            }
            #[test]
            fn accept_assigns_colors_from_coin() {
                harness::accept_assigns_colors_from_coin::<$rules>();
            }
            #[test]
            fn accept_with_unbound_black_player_is_rejected() {
                harness::accept_with_unbound_black_player_is_rejected::<$rules>();
            }
            #[test]
            fn decline_and_cancel() {
                harness::decline_and_cancel::<$rules>();
            }
            #[test]
            fn self_play_converges_and_hydrates() {
                harness::self_play_converges_and_hydrates::<$rules>();
            }
            #[test]
            fn out_of_turn_illegal_and_malformed_intents_are_rejected() {
                harness::out_of_turn_illegal_and_malformed_intents_are_rejected::<$rules>();
            }
            #[test]
            fn forged_wire_moves_are_rejected_without_mutation() {
                harness::forged_wire_moves_are_rejected_without_mutation::<$rules>();
            }
            #[test]
            fn empty_turn_fails_closed() {
                harness::empty_turn_fails_closed::<$rules>();
            }
            #[test]
            fn resign_completes_both_sides() {
                harness::resign_completes_both_sides::<$rules>();
            }
            #[test]
            fn draw_offer_ownership_and_agreement() {
                harness::draw_offer_ownership_and_agreement::<$rules>();
            }
            #[test]
            fn draw_decline_and_move_clear_the_offer() {
                harness::draw_decline_and_move_clear_the_offer::<$rules>();
            }
            #[test]
            fn hydration_replays_history_and_rejects_tampering() {
                harness::hydration_replays_history_and_rejects_tampering::<$rules>();
            }
            #[test]
            fn expiry_clears_draw_offer_and_round_trips() {
                harness::expiry_clears_draw_offer_and_round_trips::<$rules>();
            }
            #[test]
            fn participant_authorization() {
                harness::participant_authorization::<$rules>();
            }
            #[test]
            fn snapshot_rollback() {
                harness::snapshot_rollback::<$rules>();
            }
            #[test]
            fn worst_case_move_envelope_fits_budget() {
                harness::worst_case_move_envelope_fits_budget::<$rules>();
            }
            #[test]
            fn render_fallback() {
                harness::render_fallback::<$rules>();
            }
        }
    };
}
#[cfg(test)]
pub(crate) use placement_lifecycle_tests;

/// Lifecycle tests shared by every placement game; each game instantiates
/// them with `placement_lifecycle_tests!`.
#[cfg(test)]
pub(crate) mod harness {
    use super::*;
    use crate::envelope::{pack_envelope, validate_envelope_size};
    use std::sync::MutexGuard;

    pub(crate) const SID: &str = "0123456789abcdef";
    pub(crate) const ALICE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    pub(crate) const BOB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // Serializes every placement game's coin pin across tests.
    static COIN_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct CoinPin<R: PlacementRules> {
        _guard: MutexGuard<'static, ()>,
        _rules: PhantomData<R>,
    }

    impl<R: PlacementRules> Drop for CoinPin<R> {
        fn drop(&mut self) {
            if let Ok(mut pin) = R::coin_pin().lock() {
                *pin = None;
            }
        }
    }

    pub(crate) fn pin<R: PlacementRules>(challenger_is_black: bool) -> CoinPin<R> {
        let guard = COIN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        *R::coin_pin().lock().unwrap() = Some(challenger_is_black);
        CoinPin {
            _guard: guard,
            _rules: PhantomData,
        }
    }

    /// Alice challenges, Bob accepts; with the coin pinned to `true` Alice is Black.
    pub(crate) fn setup_active<R: PlacementRules>(app: &PlacementApp<R>) {
        app.handle_outgoing(SID, CMD_CHALLENGE, &HashMap::new(), ALICE);
        app.bind_session_peer(SID, ALICE, BOB).unwrap();
        let challenge = app.handle_incoming(SID, CMD_CHALLENGE, &HashMap::new(), ALICE, BOB);
        assert!(challenge.error.is_none());
        let accept = app.handle_outgoing(SID, CMD_ACCEPT, &HashMap::new(), BOB);
        assert_eq!(accept.payload.keys().collect::<Vec<_>>(), vec![KEY_BLACK]);
        let accepted = app.handle_incoming(SID, CMD_ACCEPT, &accept.payload, BOB, ALICE);
        assert!(accepted.error.is_none(), "{:?}", accepted.error);
    }

    pub(crate) fn intent(notation: &str) -> HashMap<String, rmpv::Value> {
        HashMap::from([(KEY_MOVE.into(), rmpv::Value::String(notation.into()))])
    }

    pub(crate) fn other(identity: &str) -> &'static str {
        if identity == ALICE { BOB } else { ALICE }
    }

    pub(crate) fn meta<R: PlacementRules>(
        app: &PlacementApp<R>,
        identity: &str,
    ) -> HashMap<String, JsonValue> {
        app.get_session(SID, identity).unwrap().metadata
    }

    pub(crate) fn play<R: PlacementRules>(
        app: &PlacementApp<R>,
        notation: &str,
        mover: &str,
    ) -> OutgoingResult {
        let (valid, message) =
            app.validate_outgoing_action(SID, CMD_MOVE, &intent(notation), mover);
        assert!(valid, "outgoing {notation} rejected: {message:?}");
        let out = app.handle_outgoing(SID, CMD_MOVE, &intent(notation), mover);
        assert!(!out.payload.is_empty(), "{}", out.fallback_text);
        let incoming = app.handle_incoming(SID, CMD_MOVE, &out.payload, mover, other(mover));
        assert!(incoming.error.is_none(), "{:?}", incoming.error);
        out
    }

    /// Play the first legal move for whoever is to move.
    pub(crate) fn play_any<R: PlacementRules>(app: &PlacementApp<R>) -> OutgoingResult {
        let metadata = meta(app, ALICE);
        let mover = meta_str(&metadata, "turn");
        let first = meta_string_list(&metadata, "legal_moves")[0].clone();
        play(app, &first, &mover)
    }

    pub(crate) fn assert_shared_state<R: PlacementRules>(app: &PlacementApp<R>) {
        let mut left = meta(app, ALICE);
        let mut right = meta(app, BOB);
        left.remove("my_color");
        right.remove("my_color");
        assert_eq!(left, right);
    }

    /// Deterministic self-play (`(ply * 7 + seed) % len`) until the game ends.
    pub(crate) fn self_play<R: PlacementRules>(
        app: &PlacementApp<R>,
        seed: usize,
    ) -> Vec<OutgoingResult> {
        let mut outs = Vec::new();
        for ply in 0.. {
            let metadata = meta(app, ALICE);
            if app.get_session(SID, ALICE).unwrap().status == STATUS_COMPLETED {
                break;
            }
            let legal = meta_string_list(&metadata, "legal_moves");
            let mover = meta_str(&metadata, "turn");
            let choice = legal[(ply * 7 + seed) % legal.len()].clone();
            outs.push(play(app, &choice, &mover));
            assert_shared_state(app);
            assert!(ply <= R::MAX_PLIES, "self-play did not finish");
        }
        outs
    }

    pub(crate) fn manifest_is_complete<R: PlacementRules>() {
        let manifest = PlacementApp::<R>::new().manifest();
        assert_eq!(manifest.app_id, R::APP_ID);
        assert_eq!(manifest.version, 1);
        assert_eq!(manifest.display_name, R::DISPLAY_NAME);
        assert_eq!(manifest.session_type, SESSION_TURN_BASED);
        assert_eq!(manifest.validation, VALIDATION_BOTH);
        assert_eq!(manifest.max_players, 2);
        assert_eq!(manifest.actions.len(), 9);
        for action in &manifest.actions {
            assert!(manifest.preferred_delivery.contains_key(action));
        }
        assert_eq!(manifest.preferred_delivery[CMD_RESIGN], "direct");
        assert_eq!(manifest.preferred_delivery[CMD_MOVE], "opportunistic");
        assert_eq!(manifest.ttl[STATUS_PENDING], TTL_PENDING);
        assert_eq!(manifest.ttl[STATUS_ACTIVE], TTL_ACTIVE);
    }

    pub(crate) fn accept_assigns_colors_from_coin<R: PlacementRules>() {
        for challenger_is_black in [true, false] {
            let _coin = pin::<R>(challenger_is_black);
            let app = PlacementApp::<R>::new();
            setup_active(&app);
            let black = if challenger_is_black { ALICE } else { BOB };
            for identity in [ALICE, BOB] {
                let session = app.get_session(SID, identity).unwrap();
                assert_eq!(session.status, STATUS_ACTIVE);
                assert_eq!(meta_str(&session.metadata, "first_turn"), black);
                assert_eq!(meta_str(&session.metadata, "turn"), black);
                assert_eq!(
                    meta_str(&session.metadata, "my_color"),
                    if identity == black { "b" } else { "w" }
                );
                assert_eq!(
                    meta_string_list(&session.metadata, "legal_moves"),
                    R::legal_moves(&R::start())
                );
            }
            assert_shared_state(&app);
        }
    }

    pub(crate) fn accept_with_unbound_black_player_is_rejected<R: PlacementRules>() {
        let app = PlacementApp::<R>::new();
        app.handle_outgoing(SID, CMD_CHALLENGE, &HashMap::new(), ALICE);
        app.bind_session_peer(SID, ALICE, BOB).unwrap();
        let forged = HashMap::from([(KEY_BLACK.into(), rmpv::Value::String("cc".into()))]);
        let result = app.handle_incoming(SID, CMD_ACCEPT, &forged, BOB, ALICE);
        assert!(result.error.is_some());
        assert_eq!(app.get_session(SID, ALICE).unwrap().status, STATUS_PENDING);
    }

    pub(crate) fn decline_and_cancel<R: PlacementRules>() {
        let app = PlacementApp::<R>::new();
        app.handle_outgoing(SID, CMD_CHALLENGE, &HashMap::new(), ALICE);
        app.handle_incoming(SID, CMD_CHALLENGE, &HashMap::new(), ALICE, BOB);
        let out = app.handle_outgoing(SID, CMD_DECLINE, &HashMap::new(), BOB);
        assert_eq!(
            out.fallback_text,
            format!("[LRGP {}] Challenge declined", R::DISPLAY_NAME)
        );
        app.handle_incoming(SID, CMD_DECLINE, &HashMap::new(), BOB, ALICE);
        for identity in [ALICE, BOB] {
            assert_eq!(
                app.get_session(SID, identity).unwrap().status,
                STATUS_DECLINED
            );
        }

        let cancel_sid = "1111111111111111";
        app.handle_outgoing(cancel_sid, CMD_CHALLENGE, &HashMap::new(), ALICE);
        let out = app.handle_outgoing(cancel_sid, CMD_DECLINE, &HashMap::new(), ALICE);
        assert_eq!(
            out.fallback_text,
            format!("[LRGP {}] Challenge cancelled", R::DISPLAY_NAME)
        );
    }

    pub(crate) fn self_play_converges_and_hydrates<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);
        let outs = self_play(&app, 5);
        for (ply, out) in outs.iter().enumerate() {
            assert_eq!(value_as_u64(&out.payload[KEY_PLY]), Some(ply as u64));
        }
        let last = outs.last().unwrap();
        let alice = app.get_session(SID, ALICE).unwrap();
        assert_eq!(alice.status, STATUS_COMPLETED);
        assert_eq!(app.get_session(SID, BOB).unwrap().status, STATUS_COMPLETED);
        let terminal = meta_str(&alice.metadata, "terminal");
        assert_eq!(
            value_as_str(&last.payload[KEY_TERMINAL]),
            Some(terminal.as_str())
        );
        if terminal == "win" {
            let winner = meta_str(&alice.metadata, "winner");
            assert!(winner == ALICE || winner == BOB);
            assert_eq!(
                value_as_str(&last.payload[KEY_WINNER]),
                Some(winner.as_str())
            );
            assert_eq!(meta_str(&alice.metadata, "terminal_reason"), R::WIN_REASON);
        } else {
            assert_eq!(terminal, "draw");
            assert!(R::CAN_DRAW);
        }
        assert!(meta_string_list(&alice.metadata, "legal_moves").is_empty());

        // Completed games survive hydration on both sides.
        for identity in [ALICE, BOB] {
            let record = app.get_session(SID, identity).unwrap();
            PlacementApp::<R>::new().upsert_session(record).unwrap();
        }
        let (valid, _) =
            app.validate_outgoing_action(SID, CMD_MOVE, &intent(R::LONGEST_MOVE), ALICE);
        assert!(!valid);
    }

    pub(crate) fn out_of_turn_illegal_and_malformed_intents_are_rejected<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);
        let legal = meta_string_list(&meta(&app, ALICE), "legal_moves");

        let out = app.handle_outgoing(SID, CMD_MOVE, &intent(&legal[0]), BOB);
        assert_eq!(
            out.fallback_text,
            format!("[LRGP {}] Not your turn", R::DISPLAY_NAME)
        );
        assert!(out.payload.is_empty());
        let (valid, _) = app.validate_outgoing_action(SID, CMD_MOVE, &intent(&legal[0]), BOB);
        assert!(!valid);

        for bad in ["", "A1", "a0", "a01", "zz", "swap ", "a1 "] {
            let (valid, _) = app.validate_outgoing_action(SID, CMD_MOVE, &intent(bad), ALICE);
            assert!(!valid, "accepted {bad:?}");
            assert!(
                app.handle_outgoing(SID, CMD_MOVE, &intent(bad), ALICE)
                    .payload
                    .is_empty()
            );
        }
        let (valid, _) = app.validate_outgoing_action(SID, CMD_MOVE, &HashMap::new(), ALICE);
        assert!(!valid);

        play(&app, &legal[0], ALICE);
        let occupied = legal[0].clone();
        let mover = meta_str(&meta(&app, ALICE), "turn");
        let (valid, message) =
            app.validate_outgoing_action(SID, CMD_MOVE, &intent(&occupied), &mover);
        assert!(!valid, "replayed occupied square accepted");
        assert!(message.is_some());

        let out = app.handle_outgoing("ffffffffffffffff", CMD_MOVE, &intent(&legal[0]), ALICE);
        assert_eq!(
            out.fallback_text,
            format!("[LRGP {}] Session not found", R::DISPLAY_NAME)
        );
    }

    pub(crate) fn forged_wire_moves_are_rejected_without_mutation<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);
        play_any(&app);
        let mover = meta_str(&meta(&app, ALICE), "turn");
        let receiver = other(&mover);
        let legal = meta_string_list(&meta(&app, ALICE), "legal_moves");
        let occupied = meta_str(&meta(&app, ALICE), "last_move");
        let before = meta(&app, receiver);

        let wire = |notation: &str, ply: u64, terminal: &str| {
            HashMap::from([
                (KEY_MOVE.into(), rmpv::Value::String(notation.into())),
                (KEY_PLY.into(), rmpv::Value::Integer(ply.into())),
                (KEY_TERMINAL.into(), rmpv::Value::String(terminal.into())),
            ])
        };
        let mut forged_win = wire(&legal[0], 1, "win");
        forged_win.insert(KEY_WINNER.into(), rmpv::Value::String(mover.clone().into()));
        let mut extra = wire(&legal[0], 1, "");
        extra.insert("extra".into(), rmpv::Value::Nil);
        let mut bool_ply = wire(&legal[0], 1, "");
        bool_ply.insert(KEY_PLY.into(), rmpv::Value::Boolean(true));

        let cases = [
            (wire(&legal[0], 0, ""), mover.as_str()),     // ply mismatch
            (wire(&legal[0], 2, ""), mover.as_str()),     // ply mismatch
            (wire(&occupied, 1, ""), mover.as_str()),     // occupied / illegal
            (wire("A1", 1, ""), mover.as_str()),          // non-canonical
            (wire(&legal[0], 1, ""), receiver),           // out of turn
            (wire(&legal[0], 1, "draw"), mover.as_str()), // false terminal
            (forged_win, mover.as_str()),
            (extra, mover.as_str()),
            (bool_ply, mover.as_str()),
        ];
        for (payload, sender) in cases {
            let result = app.handle_incoming(SID, CMD_MOVE, &payload, sender, other(sender));
            assert!(result.error.is_some(), "accepted {payload:?}");
        }
        assert_eq!(meta(&app, receiver), before);

        let junk = HashMap::from([("extra".into(), rmpv::Value::Boolean(true))]);
        let result = app.handle_incoming("2222222222222222", CMD_CHALLENGE, &junk, ALICE, BOB);
        assert!(result.error.is_some());
        assert!(app.get_session("2222222222222222", BOB).is_none());
    }

    pub(crate) fn empty_turn_fails_closed<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);
        let mut session = app.get_session(SID, BOB).unwrap();
        let first = meta_string_list(&session.metadata, "legal_moves")[0].clone();
        session
            .metadata
            .insert("turn".into(), JsonValue::String("".into()));
        let payload = HashMap::from([
            (KEY_MOVE.into(), rmpv::Value::String(first.into())),
            (KEY_PLY.into(), rmpv::Value::Integer(0.into())),
            (KEY_TERMINAL.into(), rmpv::Value::String("".into())),
        ]);
        let error = app.validate_move(&session, &payload, ALICE).err().unwrap();
        assert!(error.contains("Turn is required"));
    }

    pub(crate) fn resign_completes_both_sides<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);
        let out = app.handle_outgoing(SID, CMD_RESIGN, &HashMap::new(), ALICE);
        assert_eq!(
            out.fallback_text,
            format!("[LRGP {}] Resigned.", R::DISPLAY_NAME)
        );
        app.handle_incoming(SID, CMD_RESIGN, &HashMap::new(), ALICE, BOB);
        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(session.status, STATUS_COMPLETED);
            assert_eq!(meta_str(&session.metadata, "terminal"), "win");
            assert_eq!(meta_str(&session.metadata, "terminal_reason"), R_RESIGN);
            assert_eq!(meta_str(&session.metadata, "winner"), BOB);
            assert_eq!(meta_str(&session.metadata, "turn"), "");
            PlacementApp::<R>::new().upsert_session(session).unwrap();
        }
        assert_shared_state(&app);
    }

    pub(crate) fn draw_offer_ownership_and_agreement<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);

        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE);
        let offer = app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE, BOB);
        assert!(offer.error.is_none());
        assert_eq!(
            app.get_session(SID, BOB).unwrap().draw_offered_by(),
            Some(ALICE)
        );

        for command in [CMD_DRAW_ACCEPT, CMD_DRAW_OFFER] {
            let (valid, _) = app.validate_outgoing_action(SID, command, &HashMap::new(), ALICE);
            assert!(!valid);
        }
        let duplicate = app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE, BOB);
        assert!(duplicate.error.is_some());
        let self_accept = app.handle_incoming(SID, CMD_DRAW_ACCEPT, &HashMap::new(), ALICE, BOB);
        assert!(self_accept.error.is_some());

        let (valid, _) = app.validate_outgoing_action(SID, CMD_DRAW_ACCEPT, &HashMap::new(), BOB);
        assert!(valid);
        let out = app.handle_outgoing(SID, CMD_DRAW_ACCEPT, &HashMap::new(), BOB);
        assert_eq!(
            out.fallback_text,
            format!("[LRGP {}] Draw accepted", R::DISPLAY_NAME)
        );
        app.handle_incoming(SID, CMD_DRAW_ACCEPT, &HashMap::new(), BOB, ALICE);
        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(session.status, STATUS_COMPLETED);
            assert_eq!(meta_str(&session.metadata, "terminal"), "draw");
            assert_eq!(meta_str(&session.metadata, "terminal_reason"), R_AGREEMENT);
            assert_eq!(session.draw_offered_by(), None);
            PlacementApp::<R>::new().upsert_session(session).unwrap();
        }
        assert_shared_state(&app);
    }

    pub(crate) fn draw_decline_and_move_clear_the_offer<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);

        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB);
        app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB, ALICE);
        app.handle_outgoing(SID, CMD_DRAW_DECLINE, &HashMap::new(), ALICE);
        app.handle_incoming(SID, CMD_DRAW_DECLINE, &HashMap::new(), ALICE, BOB);
        assert!(!app.get_session(SID, ALICE).unwrap().has_draw_offer());
        assert!(!app.get_session(SID, BOB).unwrap().has_draw_offer());

        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB);
        app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB, ALICE);
        play_any(&app);
        assert!(!app.get_session(SID, ALICE).unwrap().has_draw_offer());
        assert!(!app.get_session(SID, BOB).unwrap().has_draw_offer());
        assert_shared_state(&app);
    }

    pub(crate) fn hydration_replays_history_and_rejects_tampering<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);
        play_any(&app);
        play_any(&app);

        let record = app.get_session(SID, BOB).unwrap();
        let fresh = PlacementApp::<R>::new();
        fresh.upsert_session(record.clone()).unwrap();
        assert_eq!(
            fresh.get_session(SID, BOB).unwrap().metadata,
            record.metadata
        );

        let start_board = R::render(&R::start());
        let moves = meta_string_list(&record.metadata, "moves");
        let tamper = |key: &str, value: JsonValue| {
            let mut copy = record.clone();
            copy.metadata.insert(key.into(), value);
            PlacementApp::<R>::new().upsert_session(copy)
        };
        let turn = meta_str(&record.metadata, "turn");
        assert!(tamper("board", JsonValue::String(start_board)).is_err());
        assert!(tamper("turn", JsonValue::String(other(&turn).into())).is_err());
        assert!(tamper("move_count", JsonValue::from(1)).is_err());
        assert!(tamper("my_color", JsonValue::String("b".into())).is_err());
        assert!(tamper("first_turn", JsonValue::String(BOB.into())).is_err());
        assert!(tamper("terminal", JsonValue::String("win".into())).is_err());
        assert!(tamper("last_move", JsonValue::String(moves[0].clone())).is_err());
        assert!(
            tamper(
                "moves",
                JsonValue::Array(vec![moves[0].clone().into(), moves[0].clone().into()])
            )
            .is_err()
        );
        assert!(tamper("moves", JsonValue::String(moves[0].clone())).is_err());

        let mut fake_draw = record.clone();
        fake_draw.status = STATUS_COMPLETED.into();
        for (key, value) in [
            ("turn", ""),
            ("terminal", "draw"),
            ("terminal_reason", R::DRAW_REASON),
        ] {
            fake_draw
                .metadata
                .insert(key.into(), JsonValue::String(value.into()));
        }
        assert!(PlacementApp::<R>::new().upsert_session(fake_draw).is_err());

        // A legacy ownerless draw flag is normalized, not answerable.
        let mut legacy = record.clone();
        legacy
            .metadata
            .insert("draw_offered".into(), JsonValue::Bool(true));
        let fresh = PlacementApp::<R>::new();
        fresh.upsert_session(legacy).unwrap();
        assert!(!fresh.get_session(SID, BOB).unwrap().has_draw_offer());

        let mut wrong_app = record.clone();
        wrong_app.app_id = "chess".into();
        assert!(PlacementApp::<R>::new().upsert_session(wrong_app).is_err());
        let mut bad_sid = record;
        bad_sid.session_id = "g1".into();
        assert!(PlacementApp::<R>::new().upsert_session(bad_sid).is_err());
    }

    pub(crate) fn expiry_clears_draw_offer_and_round_trips<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);
        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE);

        let mut record = app.get_session(SID, ALICE).unwrap();
        assert!(record.has_draw_offer());
        record.last_action_at = 0.0;
        let fresh = PlacementApp::<R>::new();
        fresh.upsert_session(record).unwrap();
        let expired = fresh.get_session(SID, ALICE).unwrap();
        assert_eq!(expired.status, STATUS_EXPIRED);
        assert!(!expired.has_draw_offer());
        assert!(fresh.authorize_incoming(SID, CMD_MOVE, BOB, ALICE).is_err());

        PlacementApp::<R>::new()
            .upsert_session(expired.clone())
            .unwrap();
        let mut stale = expired;
        stale.set_draw_offer(ALICE);
        let fresh = PlacementApp::<R>::new();
        fresh.upsert_session(stale).unwrap();
        assert!(!fresh.get_session(SID, ALICE).unwrap().has_draw_offer());
    }

    pub(crate) fn participant_authorization<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);
        assert!(app.authorize_incoming(SID, CMD_MOVE, BOB, ALICE).is_ok());
        assert!(app.authorize_incoming(SID, CMD_MOVE, "cc", ALICE).is_err());
        assert!(
            app.authorize_incoming("ffffffffffffffff", CMD_MOVE, BOB, ALICE)
                .is_err()
        );
        assert!(
            app.authorize_incoming("ffffffffffffffff", CMD_CHALLENGE, BOB, ALICE)
                .is_ok()
        );
        assert!(app.bind_session_peer(SID, ALICE, "cc").is_err());
    }

    pub(crate) fn snapshot_rollback<R: PlacementRules>() {
        let _coin = pin::<R>(true);
        let app = PlacementApp::<R>::new();
        setup_active(&app);

        let snap = app.snapshot_session(SID, ALICE);
        let out = play_any(&app);
        let notation = value_as_str(&out.payload[KEY_MOVE]).unwrap().to_string();
        assert_eq!(meta_str(&meta(&app, ALICE), "last_move"), notation);
        app.rollback_session(SID, ALICE, snap);
        let restored = app.get_session(SID, ALICE).unwrap();
        assert_eq!(meta_str(&restored.metadata, "last_move"), "");
        assert_eq!(restored.metadata["move_count"], JsonValue::from(0));

        let fresh = PlacementApp::<R>::new();
        let none = fresh.snapshot_session("3333333333333333", ALICE);
        fresh.handle_outgoing("3333333333333333", CMD_CHALLENGE, &HashMap::new(), ALICE);
        fresh.rollback_session("3333333333333333", ALICE, none);
        assert!(fresh.get_session("3333333333333333", ALICE).is_none());
    }

    pub(crate) fn worst_case_move_envelope_fits_budget<R: PlacementRules>() {
        let longest = R::LONGEST_MOVE;
        assert!(R::is_canonical(longest));
        let payload = HashMap::from([
            (KEY_MOVE.into(), rmpv::Value::String(longest.into())),
            (KEY_PLY.into(), rmpv::Value::Integer(65_535.into())),
            (KEY_TERMINAL.into(), rmpv::Value::String("win".into())),
            (KEY_WINNER.into(), rmpv::Value::String(ALICE.into())),
        ]);
        let envelope = pack_envelope(R::APP_ID, 1, CMD_MOVE, SID, Some(payload), None).unwrap();
        let size = validate_envelope_size(&envelope).expect("envelope size OK");
        assert!(size <= 150, "worst-case move envelope {size} bytes");
    }

    pub(crate) fn render_fallback<R: PlacementRules>() {
        let app = PlacementApp::<R>::new();
        let prefix = format!("[LRGP {}]", R::DISPLAY_NAME);
        assert_eq!(
            app.render_fallback(CMD_CHALLENGE, &HashMap::new()),
            format!("{prefix} Sent a challenge!")
        );
        let mut p = intent("c3");
        p.insert(KEY_TERMINAL.into(), rmpv::Value::String("".into()));
        assert_eq!(app.render_fallback(CMD_MOVE, &p), format!("{prefix} c3"));
        p.insert(KEY_TERMINAL.into(), rmpv::Value::String("win".into()));
        assert_eq!(
            app.render_fallback(CMD_MOVE, &p),
            format!("{prefix} c3 (game won)")
        );
        p.insert(KEY_TERMINAL.into(), rmpv::Value::String("draw".into()));
        assert_eq!(
            app.render_fallback(CMD_MOVE, &p),
            format!("{prefix} c3 (game drawn)")
        );
    }
}
