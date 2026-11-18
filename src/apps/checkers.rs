//! Checkers (American checkers / English draughts). Wire moves are
//! space-separated square paths `{m, n, x, r, w}` such as `a3 b4` or
//! `a5 c3 e1`; boards and legal moves are local-only. Both peers replay the
//! path history from the starting position (VALIDATION_BOTH).

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value as JsonValue;

use crate::app_base::{AppManifest, GameApp, IncomingResult, OutgoingResult};
use crate::constants::*;
use crate::envelope::{has_exact_keys, value_as_str, value_as_u64};
use crate::errors::LrgpError;
use crate::session::{Session, SessionStateMachine};

pub const APP_ID: &str = "checkers";
pub const APP_VERSION: u32 = 1;

/// Starting board, 64 cells row-major from a8 (index 0) to h1 (index 63).
/// `_` is empty, `b`/`w` are men and `B`/`W` are kings.
pub const STARTING_BOARD: &str = "_b_b_b_bb_b_b_b__b_b_b_b________________w_w_w_w__w_w_w_ww_w_w_w_";

// Terminal reason codes (≤3 chars to fit envelope budget).
const R_NO_PIECES: &str = "np";
const R_NO_MOVES: &str = "nm";
const R_THREEFOLD: &str = "3fr";
const R_FORTY_MOVE: &str = "40m";
const R_RESIGN: &str = "rsn";
const R_AGREEMENT: &str = "agr";

/// Forty moves per side without a capture or a man move.
const FORTY_MOVE_PLIES: usize = 80;
/// Twelve captures is the most any single move can make.
const MAX_PATH_SQUARES: usize = 13;

// Wire payload keys.
const KEY_MOVE: &str = "m"; // square path, e.g. "a3 b4" or "a5 c3 e1"
const KEY_PLY: &str = "n"; // ply counter (u32), 0-based
const KEY_TERMINAL: &str = "x"; // "" | "win"
const KEY_REASON: &str = "r"; // terminal / draw-claim reason
const KEY_WINNER: &str = "w"; // winner identity hash (terminal="win")
const KEY_BLACK: &str = "b"; // Black-player hash (ACCEPT payload)

const EMPTY: u8 = b'_';
const BLACK_MAN: u8 = b'b';
const BLACK_KING: u8 = b'B';
const WHITE_MAN: u8 = b'w';
const WHITE_KING: u8 = b'W';

#[cfg(any(test, feature = "test-helpers"))]
static FORCED_COIN: Mutex<Option<bool>> = Mutex::new(None);

/// `true` if the challenger gets Black. Pin via `force_coin` in tests.
fn flip_responder_coin() -> bool {
    #[cfg(any(test, feature = "test-helpers"))]
    {
        if let Ok(guard) = FORCED_COIN.lock() {
            if let Some(v) = *guard {
                return v;
            }
        }
    }
    use rand::RngCore;
    (rand::thread_rng().next_u32() & 1) == 0
}

/// Pin the coin-flip for tests. Process-global; callers must serialize.
#[cfg(any(test, feature = "test-helpers"))]
pub fn force_coin(challenger_is_black: Option<bool>) {
    if let Ok(mut guard) = FORCED_COIN.lock() {
        *guard = challenger_is_black;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Black,
    White,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Side::Black => Side::White,
            Side::White => Side::Black,
        }
    }

    fn owns(self, piece: u8) -> bool {
        match self {
            Side::Black => matches!(piece, BLACK_MAN | BLACK_KING),
            Side::White => matches!(piece, WHITE_MAN | WHITE_KING),
        }
    }

    fn king(self) -> u8 {
        match self {
            Side::Black => BLACK_KING,
            Side::White => WHITE_KING,
        }
    }

    /// Rank index (0 = rank 1) on which this side's men are crowned.
    fn crown_rank(self) -> usize {
        match self {
            Side::Black => 0,
            Side::White => 7,
        }
    }
}

fn is_king(piece: u8) -> bool {
    matches!(piece, BLACK_KING | WHITE_KING)
}

/// `(rank, file)` steps. Black men move toward rank 1, White men toward rank 8.
fn directions(piece: u8) -> &'static [(isize, isize)] {
    match piece {
        BLACK_MAN => &[(-1, -1), (-1, 1)],
        WHITE_MAN => &[(1, -1), (1, 1)],
        _ => &[(-1, -1), (-1, 1), (1, -1), (1, 1)],
    }
}

/// Squares are `rank * 8 + file` with a1 = 0 and h8 = 63.
fn step(square: usize, (dr, df): (isize, isize)) -> Option<usize> {
    let rank = (square / 8) as isize + dr;
    let file = (square % 8) as isize + df;
    ((0..8).contains(&rank) && (0..8).contains(&file)).then_some((rank * 8 + file) as usize)
}

/// a1 is dark; only dark squares are playable.
fn is_dark(square: usize) -> bool {
    (square / 8 + square % 8) & 1 == 0
}

fn square_name(square: usize) -> String {
    format!("{}{}", (b'a' + (square % 8) as u8) as char, square / 8 + 1)
}

fn parse_square(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.len() != 2 {
        return None;
    }
    let file = bytes[0].checked_sub(b'a')? as usize;
    let rank = bytes[1].checked_sub(b'1')? as usize;
    if file >= 8 || rank >= 8 {
        return None;
    }
    let square = rank * 8 + file;
    is_dark(square).then_some(square)
}

/// Parse a canonical path: 2..=13 lowercase dark squares joined by single spaces.
fn parse_path(notation: &str) -> Option<Vec<usize>> {
    let squares = notation
        .split(' ')
        .map(parse_square)
        .collect::<Option<Vec<_>>>()?;
    (2..=MAX_PATH_SQUARES)
        .contains(&squares.len())
        .then_some(squares)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckersMove {
    path: Vec<usize>,
    captured: Vec<usize>,
}

impl CheckersMove {
    fn notation(&self) -> String {
        self.path
            .iter()
            .map(|square| square_name(*square))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn is_capture(&self) -> bool {
        !self.captured.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Position {
    board: [u8; 64],
    side: Side,
}

impl Position {
    fn starting() -> Self {
        let mut board = [EMPTY; 64];
        for (square, cell) in board.iter_mut().enumerate() {
            if !is_dark(square) {
                continue;
            }
            match square / 8 {
                0..=2 => *cell = WHITE_MAN,
                5..=7 => *cell = BLACK_MAN,
                _ => {}
            }
        }
        Self {
            board,
            side: Side::Black,
        }
    }

    /// Row-major from a8 down to h1, matching how the board is drawn.
    fn render(&self) -> String {
        (0..8)
            .rev()
            .flat_map(|rank| (0..8).map(move |file| self.board[rank * 8 + file] as char))
            .collect()
    }

    fn has_pieces(&self, side: Side) -> bool {
        self.board.iter().any(|piece| side.owns(*piece))
    }

    /// Captures are mandatory and every jump sequence must be completed.
    fn legal_moves(&self) -> Vec<CheckersMove> {
        let mut jumps = Vec::new();
        let mut board = self.board;
        for from in 0..64 {
            let piece = self.board[from];
            if !self.side.owns(piece) {
                continue;
            }
            // Lift the piece so a king's circuit may pass through its origin.
            board[from] = EMPTY;
            let mut path = vec![from];
            let mut captured = Vec::new();
            collect_jumps(
                &board,
                piece,
                self.side,
                &mut path,
                &mut captured,
                &mut jumps,
            );
            board[from] = piece;
        }
        if !jumps.is_empty() {
            return jumps;
        }

        let mut steps = Vec::new();
        for from in 0..64 {
            let piece = self.board[from];
            if !self.side.owns(piece) {
                continue;
            }
            for &dir in directions(piece) {
                if let Some(to) = step(from, dir) {
                    if self.board[to] == EMPTY {
                        steps.push(CheckersMove {
                            path: vec![from, to],
                            captured: Vec::new(),
                        });
                    }
                }
            }
        }
        steps
    }

    /// Apply an already-generated legal move and pass the turn.
    fn apply(&mut self, mv: &CheckersMove) {
        let from = mv.path[0];
        let to = mv.path[mv.path.len() - 1];
        let mut piece = self.board[from];
        self.board[from] = EMPTY;
        for &square in &mv.captured {
            self.board[square] = EMPTY;
        }
        if !is_king(piece) && to / 8 == self.side.crown_rank() {
            piece = self.side.king();
        }
        self.board[to] = piece;
        self.side = self.side.other();
    }
}

/// Depth-first jump search. Jumped pieces stay on the board until the move
/// completes: they cannot be jumped twice and block landing. A man that
/// reaches the crown rank is crowned and its move ends there.
fn collect_jumps(
    board: &[u8; 64],
    piece: u8,
    side: Side,
    path: &mut Vec<usize>,
    captured: &mut Vec<usize>,
    out: &mut Vec<CheckersMove>,
) {
    let from = path[path.len() - 1];
    let mut extended = false;
    for &dir in directions(piece) {
        let Some(over) = step(from, dir) else {
            continue;
        };
        let Some(to) = step(over, dir) else {
            continue;
        };
        if !side.other().owns(board[over]) || captured.contains(&over) || board[to] != EMPTY {
            continue;
        }
        extended = true;
        path.push(to);
        captured.push(over);
        if !is_king(piece) && to / 8 == side.crown_rank() {
            out.push(CheckersMove {
                path: path.clone(),
                captured: captured.clone(),
            });
        } else {
            collect_jumps(board, piece, side, path, captured, out);
        }
        path.pop();
        captured.pop();
    }
    if !extended && !captured.is_empty() {
        out.push(CheckersMove {
            path: path.clone(),
            captured: captured.clone(),
        });
    }
}

/// A position plus the history needed for draw-claim verification.
#[derive(Clone, Debug)]
struct Game {
    position: Position,
    history: Vec<Position>,
    /// Plies since the last capture or man move.
    quiet_plies: usize,
}

impl Game {
    fn new() -> Self {
        Self::from_position(Position::starting())
    }

    fn from_position(position: Position) -> Self {
        Self {
            history: vec![position.clone()],
            position,
            quiet_plies: 0,
        }
    }

    /// Replay path history from the starting position.
    fn replay(moves: &[String]) -> Result<Self, String> {
        let mut game = Self::new();
        for notation in moves {
            game.play(notation)?;
        }
        Ok(game)
    }

    fn play(&mut self, notation: &str) -> Result<CheckersMove, String> {
        if parse_path(notation).is_none() {
            return Err(format!("Invalid move notation: {notation}"));
        }
        let mv = self
            .position
            .legal_moves()
            .into_iter()
            .find(|mv| mv.notation() == notation)
            .ok_or_else(|| format!("Move {notation} is not legal from current position"))?;
        let man_moved = !is_king(self.position.board[mv.path[0]]);
        self.position.apply(&mv);
        self.quiet_plies = if man_moved || mv.is_capture() {
            0
        } else {
            self.quiet_plies + 1
        };
        self.history.push(self.position.clone());
        Ok(mv)
    }

    fn claim_is_valid(&self, reason: &str) -> bool {
        match reason {
            R_FORTY_MOVE => self.quiet_plies >= FORTY_MOVE_PLIES,
            R_THREEFOLD => {
                self.history
                    .iter()
                    .filter(|position| **position == self.position)
                    .count()
                    >= 3
            }
            _ => false,
        }
    }

    /// Draw-claim eligibility for display. 40-move wins ties.
    fn claim_reason(&self) -> Option<&'static str> {
        [R_FORTY_MOVE, R_THREEFOLD]
            .into_iter()
            .find(|reason| self.claim_is_valid(reason))
    }

    /// Returns `(terminal, reason)` where terminal is `""`/`"win"`.
    /// On `"win"` the player who just moved won.
    fn terminal(&self) -> (&'static str, &'static str) {
        if !self.position.has_pieces(self.position.side) {
            return ("win", R_NO_PIECES);
        }
        if self.position.legal_moves().is_empty() {
            return ("win", R_NO_MOVES);
        }
        ("", "")
    }
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

fn emit_event(event_type: &str, session_id: &str, from: &str) -> HashMap<String, JsonValue> {
    let mut m = HashMap::new();
    m.insert("type".into(), JsonValue::String(event_type.into()));
    m.insert("session_id".into(), JsonValue::String(session_id.into()));
    m.insert("app_id".into(), JsonValue::String(APP_ID.into()));
    m.insert("from".into(), JsonValue::String(from.into()));
    m
}

fn rejected(fallback_text: impl Into<String>) -> OutgoingResult {
    OutgoingResult {
        payload: HashMap::new(),
        fallback_text: format!("[LRGP Checkers] {}", fallback_text.into()),
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

pub struct CheckersApp {
    sessions: Mutex<HashMap<(String, String), Session>>,
}

impl CheckersApp {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Apply TTL policy; an expired game can no longer answer a draw offer.
    fn check_session_expiry(session: &mut Session, ttl: &HashMap<String, f64>) -> bool {
        let expired = SessionStateMachine::check_expiry(session, Some(ttl), None);
        if session.status == STATUS_EXPIRED {
            session.clear_draw_offer();
        }
        expired
    }

    fn get_session(&self, session_id: &str, identity_id: &str) -> Option<Session> {
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

    fn ttl_policy() -> HashMap<String, f64> {
        let mut ttl = HashMap::new();
        ttl.insert(STATUS_PENDING.into(), TTL_PENDING);
        ttl.insert(STATUS_ACTIVE.into(), TTL_ACTIVE);
        ttl
    }

    fn default_metadata() -> HashMap<String, JsonValue> {
        let mut m = HashMap::new();
        m.insert("board".into(), JsonValue::String(STARTING_BOARD.into()));
        m.insert("moves".into(), JsonValue::Array(vec![]));
        m.insert("my_color".into(), JsonValue::String("".into()));
        m.insert("first_turn".into(), JsonValue::String("".into()));
        m.insert("turn".into(), JsonValue::String("".into()));
        m.insert("move_count".into(), JsonValue::Number(0.into()));
        m.insert("winner".into(), JsonValue::String("".into()));
        m.insert("terminal".into(), JsonValue::String("".into()));
        m.insert("terminal_reason".into(), JsonValue::String("".into()));
        m.insert("draw_offered".into(), JsonValue::Bool(false));
        m.insert("draw_offered_by".into(), JsonValue::String("".into()));
        m.insert("draw_offer_reason".into(), JsonValue::String("".into()));
        m.insert("last_move".into(), JsonValue::String("".into()));
        m.insert("must_capture".into(), JsonValue::Bool(false));
        m.insert("legal_moves".into(), JsonValue::Array(vec![]));
        m
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
            CMD_CHALLENGE | CMD_DECLINE | CMD_RESIGN | CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE => {
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
            CMD_DRAW_OFFER => {
                if !(payload.is_empty() || has_exact_keys(payload, &[KEY_REASON])) {
                    return Err("draw_offer payload must be empty or exactly {r}".into());
                }
                if let Some(reason) = payload.get(KEY_REASON) {
                    let reason = value_as_str(reason)
                        .ok_or_else(|| "draw_offer r must be a string".to_string())?;
                    if !matches!(reason, R_THREEFOLD | R_FORTY_MOVE) {
                        return Err(format!("unsupported draw claim reason '{reason}'"));
                    }
                }
            }
            CMD_MOVE => {
                let terminal = payload
                    .get(KEY_TERMINAL)
                    .and_then(value_as_str)
                    .ok_or_else(|| "move x must be a string".to_string())?;
                let expected: &[&str] = match terminal {
                    "" => &[KEY_MOVE, KEY_PLY, KEY_TERMINAL],
                    "win" => &[KEY_MOVE, KEY_PLY, KEY_TERMINAL, KEY_REASON, KEY_WINNER],
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
                    .and_then(parse_path)
                    .is_none()
                    || payload.get(KEY_PLY).and_then(value_as_u64).is_none()
                {
                    return Err("move payload contains a value with the wrong type".into());
                }
                if terminal == "win" {
                    let reason = payload.get(KEY_REASON).and_then(value_as_str);
                    if !matches!(reason, Some(R_NO_PIECES | R_NO_MOVES)) {
                        return Err("winning move r must be np or nm".into());
                    }
                    if payload
                        .get(KEY_WINNER)
                        .and_then(value_as_str)
                        .filter(|winner| !winner.is_empty())
                        .is_none()
                    {
                        return Err("winning move w must be a non-empty string".into());
                    }
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

    /// Refresh derived session metadata (board, legal_moves, must_capture,
    /// draw_offer_reason).
    fn refresh_derived(session: &mut Session, game: &Game) {
        session
            .metadata
            .insert("board".into(), JsonValue::String(game.position.render()));
        let legal = game.position.legal_moves();
        session.metadata.insert(
            "must_capture".into(),
            JsonValue::Bool(legal.first().is_some_and(CheckersMove::is_capture)),
        );
        let notations: Vec<String> = legal.iter().map(CheckersMove::notation).collect();
        session
            .metadata
            .insert("legal_moves".into(), string_list_value(&notations));
        let claim = if game.terminal().0.is_empty() {
            game.claim_reason().unwrap_or("")
        } else {
            ""
        };
        session.metadata.insert(
            "draw_offer_reason".into(),
            JsonValue::String(claim.to_string()),
        );
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
        Self::refresh_derived(session, &Game::new());
    }

    /// Record a move already applied to `game` by `mover`.
    fn record_move(
        session: &mut Session,
        game: &Game,
        notation: &str,
        mover: &str,
    ) -> Result<(), String> {
        let (terminal, reason) = game.terminal();
        let next_turn = if terminal.is_empty() {
            other_player(session, mover).ok_or_else(|| "Opponent unknown".to_string())?
        } else {
            String::new()
        };
        let winner = if terminal == "win" {
            mover.to_string()
        } else {
            String::new()
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
            .insert("turn".into(), JsonValue::String(next_turn));
        session
            .metadata
            .insert("last_move".into(), JsonValue::String(notation.to_string()));
        session
            .metadata
            .insert("terminal".into(), JsonValue::String(terminal.to_string()));
        session.metadata.insert(
            "terminal_reason".into(),
            JsonValue::String(reason.to_string()),
        );
        session
            .metadata
            .insert("winner".into(), JsonValue::String(winner));
        session.clear_draw_offer();
        Self::refresh_derived(session, game);

        SessionStateMachine::apply_command(session, CMD_MOVE, !terminal.is_empty())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Record a non-move game end (resignation, agreement, verified claim).
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
        session
            .metadata
            .insert("must_capture".into(), JsonValue::Bool(false));
        session
            .metadata
            .insert("draw_offer_reason".into(), JsonValue::String("".into()));
        session.clear_draw_offer();
    }

    /// `true` when `reason` is a claim the replayed position supports.
    fn claim_is_valid(session: &Session, reason: &str) -> bool {
        let moves = meta_string_list(&session.metadata, "moves");
        Game::replay(&moves).is_ok_and(|game| game.claim_is_valid(reason))
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
        session.app_id = APP_ID.to_string();
        session.app_version = APP_VERSION;
        session.contact_hash = sender_hash.to_string();
        session.initiator = sender_hash.to_string();
        session.status = STATUS_PENDING.to_string();
        session.metadata = Self::default_metadata();
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("challenge", session_id, sender_hash)),
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
            emit: Some(emit_event("accept", session_id, sender_hash)),
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

        if session.initiator == sender_hash {
            session
                .metadata
                .insert("cancelled_by_initiator".into(), JsonValue::Bool(true));
        }
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("decline", session_id, sender_hash)),
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

        let game = match self.validate_move(&session, payload, sender_hash) {
            Ok(game) => game,
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
        if let Err(error) = Self::record_move(&mut session, &game, &notation, sender_hash) {
            return error_result(ERR_PROTOCOL_ERROR, &error);
        }
        session.unread = 1;
        self.save_session(&session);

        let mut emit = emit_event("move", session_id, sender_hash);
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
        // Sender resigned; opponent (= identity_id locally) wins.
        Self::finish(&mut session, "win", R_RESIGN, identity_id);
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("resign", session_id, sender_hash)),
            error: None,
        }
    }

    fn handle_draw_offer_in(
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

        // draw_offer with a valid `r` reason is a claim (threefold / 40-move).
        // Verify locally; valid claim ends the game, otherwise plain offer.
        let claimed_reason = payload.get(KEY_REASON).and_then(value_as_str).unwrap_or("");

        if !claimed_reason.is_empty() && Self::claim_is_valid(&session, claimed_reason) {
            if let Err(error) =
                SessionStateMachine::apply_command(&mut session, CMD_DRAW_ACCEPT, false)
            {
                return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
            }
            Self::finish(&mut session, "draw", claimed_reason, "");
            session.unread = 1;
            self.save_session(&session);

            return IncomingResult {
                session: Some(session_to_json(&session)),
                emit: Some(emit_event("draw_claim", session_id, sender_hash)),
                error: None,
            };
        }

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
            emit: Some(emit_event("draw_offer", session_id, sender_hash)),
            error: None,
        }
    }

    fn handle_draw_response_in(
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

        let Some(offered_by) = session.draw_offered_by() else {
            return error_result(ERR_PROTOCOL_ERROR, "No draw offer is outstanding");
        };
        if offered_by == sender_hash {
            return error_result(
                ERR_PROTOCOL_ERROR,
                "A participant cannot answer its own draw offer",
            );
        }

        if let Err(error) = SessionStateMachine::apply_command(&mut session, command, false) {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        if command == CMD_DRAW_ACCEPT {
            Self::finish(&mut session, "draw", R_AGREEMENT, "");
        } else {
            session.clear_draw_offer();
        }
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event(command, session_id, sender_hash)),
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
        session.app_id = APP_ID.to_string();
        session.app_version = APP_VERSION;
        session.initiator = identity_id.to_string();
        session.status = STATUS_PENDING.to_string();
        session.metadata = Self::default_metadata();
        self.save_session(&session);

        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP Checkers] Sent a challenge!".into(),
        }
    }

    fn handle_accept_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        let Some(mut session) = self.get_session(session_id, identity_id) else {
            return rejected("Session not found");
        };
        if let Err(error) = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false) {
            return rejected(error.to_string());
        }

        // Responder coin-flips Black; ships the Black-player hash as `b`.
        let black_hash = if flip_responder_coin() {
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
            fallback_text: "[LRGP Checkers] Challenge accepted".into(),
        }
    }

    fn handle_decline_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        let mut was_cancel = false;
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            if session.initiator == identity_id {
                session
                    .metadata
                    .insert("cancelled_by_initiator".into(), JsonValue::Bool(true));
                was_cancel = true;
            }
            let _ = SessionStateMachine::apply_command(&mut session, CMD_DECLINE, false);
            self.save_session(&session);
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: if was_cancel {
                "[LRGP Checkers] Challenge cancelled".into()
            } else {
                "[LRGP Checkers] Challenge declined".into()
            },
        }
    }

    fn handle_move_out(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> OutgoingResult {
        let Some(mut session) = self.get_session(session_id, identity_id) else {
            return rejected("Session not found");
        };

        // Empty payload sentinel → dashboard surfaces game_action_result{ok:false}.
        if session.status != STATUS_ACTIVE {
            return rejected(format!("Session is not active ({})", session.status));
        }
        if meta_str(&session.metadata, "turn") != identity_id {
            return rejected("Not your turn");
        }
        let notation = payload
            .get(KEY_MOVE)
            .and_then(value_as_str)
            .unwrap_or("")
            .to_string();

        let moves = meta_string_list(&session.metadata, "moves");
        let mut game = match Game::replay(&moves) {
            Ok(game) => game,
            Err(e) => return rejected(format!("Local replay failed: {e}")),
        };
        if let Err(e) = game.play(&notation) {
            return rejected(format!("Illegal move: {e}"));
        }
        // SPEC: `n` is 0-based (0 = Black's first move) — count before push.
        let ply = moves.len() as u64;
        let (terminal, reason) = game.terminal();

        if let Err(e) = Self::record_move(&mut session, &game, &notation, identity_id) {
            return rejected(e);
        }
        self.save_session(&session);

        let mut enriched = HashMap::new();
        enriched.insert(KEY_MOVE.to_string(), rmpv::Value::String(notation.into()));
        enriched.insert(KEY_PLY.to_string(), rmpv::Value::Integer(ply.into()));
        enriched.insert(
            KEY_TERMINAL.to_string(),
            rmpv::Value::String(terminal.into()),
        );
        if terminal == "win" {
            enriched.insert(KEY_REASON.to_string(), rmpv::Value::String(reason.into()));
            enriched.insert(
                KEY_WINNER.to_string(),
                rmpv::Value::String(identity_id.into()),
            );
        }

        let fallback = self.render_fallback_inner(CMD_MOVE, &enriched);
        OutgoingResult {
            payload: enriched,
            fallback_text: fallback,
        }
    }

    fn handle_resign_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            if SessionStateMachine::apply_command(&mut session, CMD_RESIGN, false).is_ok() {
                let winner = session.contact_hash.clone();
                Self::finish(&mut session, "win", R_RESIGN, &winner);
                self.save_session(&session);
            }
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP Checkers] Resigned.".into(),
        }
    }

    fn handle_draw_offer_out(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> OutgoingResult {
        let reason = payload
            .get(KEY_REASON)
            .and_then(value_as_str)
            .unwrap_or("")
            .to_string();

        // A locally ineligible claim degrades to a plain offer but keeps `r`
        // on the wire so the receiver verifies eligibility independently.
        let mut enriched = HashMap::new();
        if !reason.is_empty() {
            enriched.insert(
                KEY_REASON.to_string(),
                rmpv::Value::String(reason.clone().into()),
            );
        }

        if let Some(mut session) = self.get_session(session_id, identity_id) {
            // On a valid claim, pre-terminate locally so UI reflects immediately.
            if !reason.is_empty() && Self::claim_is_valid(&session, &reason) {
                if SessionStateMachine::apply_command(&mut session, CMD_DRAW_ACCEPT, false).is_ok()
                {
                    Self::finish(&mut session, "draw", &reason, "");
                }
            } else if SessionStateMachine::apply_command(&mut session, CMD_DRAW_OFFER, false)
                .is_ok()
            {
                session.set_draw_offer(identity_id);
            }
            self.save_session(&session);
        }

        OutgoingResult {
            fallback_text: self.render_fallback_inner(CMD_DRAW_OFFER, &enriched),
            payload: enriched,
        }
    }

    fn handle_draw_response_out(
        &self,
        session_id: &str,
        command: &str,
        identity_id: &str,
    ) -> OutgoingResult {
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            if SessionStateMachine::apply_command(&mut session, command, false).is_ok() {
                if command == CMD_DRAW_ACCEPT {
                    Self::finish(&mut session, "draw", R_AGREEMENT, "");
                } else {
                    session.clear_draw_offer();
                }
                self.save_session(&session);
            }
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: self.render_fallback_inner(command, &HashMap::new()),
        }
    }

    /// Validate an inbound wire move and return the game with it applied.
    fn validate_move(
        &self,
        session: &Session,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
    ) -> Result<Game, String> {
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
            .filter(|notation| !notation.is_empty())
            .ok_or_else(|| "Missing move path".to_string())?;
        let ply = payload
            .get(KEY_PLY)
            .and_then(value_as_u64)
            .ok_or_else(|| "Missing ply index".to_string())?;
        let claimed_terminal = payload
            .get(KEY_TERMINAL)
            .and_then(value_as_str)
            .ok_or_else(|| "Missing terminal marker".to_string())?;
        let claimed_reason = payload.get(KEY_REASON).and_then(value_as_str).unwrap_or("");
        let claimed_winner = payload.get(KEY_WINNER).and_then(value_as_str).unwrap_or("");

        // SPEC: `n` is 0-based, so the next move's ply equals the count of
        // moves already applied.
        let moves = meta_string_list(&session.metadata, "moves");
        let expected_ply = moves.len() as u64;
        if ply != expected_ply {
            return Err(format!("Ply mismatch: expected {expected_ply}, got {ply}"));
        }

        let mut game = Game::replay(&moves).map_err(|e| format!("Local replay failed: {e}"))?;
        game.play(notation)?;
        let (terminal, reason) = game.terminal();

        if terminal != claimed_terminal {
            return Err(format!(
                "Terminal mismatch: computed='{terminal}' claimed='{claimed_terminal}'"
            ));
        }
        if reason != claimed_reason {
            return Err(format!(
                "Reason mismatch: computed='{reason}' claimed='{claimed_reason}'"
            ));
        }
        if terminal == "win" && claimed_winner != sender_hash {
            return Err(format!(
                "Winner mismatch: computed='{sender_hash}' claimed='{claimed_winner}'"
            ));
        }
        if terminal != "win" && !claimed_winner.is_empty() {
            return Err("Winner must be empty unless the move is a win".into());
        }

        Ok(game)
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
        let game = Game::replay(&moves)
            .map_err(|e| invalid(&format!("restored move history is invalid: {e}")))?;
        if text("board")? != game.position.render() {
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
                let second_turn = other_player(session, first_turn)
                    .ok_or_else(|| invalid("restored session has no bound opponent"))?;
                let player = |side: Side| match side {
                    Side::Black => first_turn,
                    Side::White => second_turn.as_str(),
                };
                let to_move = player(game.position.side);
                let (board_terminal, board_reason) = game.terminal();

                match session.status.as_str() {
                    STATUS_ACTIVE => {
                        if turn != to_move
                            || !terminal.is_empty()
                            || !reason.is_empty()
                            || !winner.is_empty()
                            || !board_terminal.is_empty()
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
                            || !board_terminal.is_empty()
                            || (!turn.is_empty() && turn != to_move)
                        {
                            return Err(invalid("expired session contains terminal metadata"));
                        }
                    }
                    _ => {
                        if !turn.is_empty() {
                            return Err(invalid("completed session must not have a turn"));
                        }
                        let consistent = match (terminal, reason) {
                            ("win", R_NO_PIECES | R_NO_MOVES) => {
                                board_terminal == "win"
                                    && board_reason == reason
                                    && winner == player(game.position.side.other())
                            }
                            ("win", R_RESIGN) => {
                                board_terminal.is_empty()
                                    && (winner == session.identity_id
                                        || winner == session.contact_hash)
                            }
                            ("draw", R_AGREEMENT) => board_terminal.is_empty() && winner.is_empty(),
                            ("draw", R_THREEFOLD | R_FORTY_MOVE) => {
                                board_terminal.is_empty()
                                    && winner.is_empty()
                                    && game.claim_is_valid(reason)
                            }
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

    fn render_fallback_inner(
        &self,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
    ) -> String {
        match command {
            CMD_CHALLENGE => "[LRGP Checkers] Sent a challenge!".into(),
            CMD_ACCEPT => "[LRGP Checkers] Challenge accepted".into(),
            CMD_DECLINE => "[LRGP Checkers] Challenge declined".into(),
            CMD_MOVE => {
                let terminal = payload
                    .get(KEY_TERMINAL)
                    .and_then(value_as_str)
                    .unwrap_or("");
                let reason = payload.get(KEY_REASON).and_then(value_as_str).unwrap_or("");
                let path = payload.get(KEY_MOVE).and_then(value_as_str).unwrap_or("");
                if terminal == "win" {
                    let detail = match reason {
                        R_NO_PIECES => "no pieces left",
                        R_NO_MOVES => "no moves left",
                        _ => "win",
                    };
                    format!("[LRGP Checkers] {path} wins ({detail})")
                } else {
                    format!("[LRGP Checkers] {path}")
                }
            }
            CMD_RESIGN => "[LRGP Checkers] Resigned.".into(),
            CMD_DRAW_OFFER => {
                let reason = payload.get(KEY_REASON).and_then(value_as_str).unwrap_or("");
                match reason {
                    R_THREEFOLD => "[LRGP Checkers] Claimed threefold repetition".into(),
                    R_FORTY_MOVE => "[LRGP Checkers] Claimed forty-move rule".into(),
                    _ => "[LRGP Checkers] Offered a draw".into(),
                }
            }
            CMD_DRAW_ACCEPT => "[LRGP Checkers] Draw accepted".into(),
            CMD_DRAW_DECLINE => "[LRGP Checkers] Draw declined".into(),
            CMD_ERROR => {
                let msg = payload
                    .get("msg")
                    .and_then(value_as_str)
                    .unwrap_or("Unknown");
                format!("[LRGP Checkers] Error: {msg}")
            }
            other => format!("[LRGP Checkers] {other}"),
        }
    }
}

impl Default for CheckersApp {
    fn default() -> Self {
        Self::new()
    }
}

impl GameApp for CheckersApp {
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
        let preferred_delivery = actions
            .iter()
            .map(|command| (command.clone(), self.get_delivery_method(command)))
            .collect();

        AppManifest {
            app_id: APP_ID.into(),
            version: APP_VERSION,
            display_name: "Checkers".into(),
            icon: "checkers".into(),
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
            CMD_DECLINE => self.handle_decline_in(session_id, sender_hash, identity_id),
            CMD_MOVE => self.handle_move_in(session_id, payload, sender_hash, identity_id),
            CMD_RESIGN => self.handle_resign_in(session_id, sender_hash, identity_id),
            CMD_DRAW_OFFER => {
                self.handle_draw_offer_in(session_id, payload, sender_hash, identity_id)
            }
            CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE => {
                self.handle_draw_response_in(session_id, command, sender_hash, identity_id)
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
            CMD_DECLINE => self.handle_decline_out(session_id, identity_id),
            CMD_MOVE => self.handle_move_out(session_id, payload, identity_id),
            CMD_RESIGN => self.handle_resign_out(session_id, identity_id),
            CMD_DRAW_OFFER => self.handle_draw_offer_out(session_id, payload, identity_id),
            CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE => {
                self.handle_draw_response_out(session_id, command, identity_id)
            }
            _ => OutgoingResult {
                payload: payload.clone(),
                fallback_text: format!("[LRGP Checkers] {command}"),
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
            CMD_CHALLENGE | CMD_ACCEPT | CMD_DECLINE | CMD_RESIGN | CMD_DRAW_ACCEPT
            | CMD_DRAW_DECLINE => {
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
            CMD_DRAW_OFFER => {
                if !(payload.is_empty() || has_exact_keys(payload, &[KEY_REASON])) {
                    return (
                        false,
                        Some("draw_offer intent must be empty or exactly {r}".into()),
                    );
                }
                if let Some(reason) = payload.get(KEY_REASON) {
                    let Some(reason) = value_as_str(reason) else {
                        return (false, Some("draw_offer r must be a string".into()));
                    };
                    if !matches!(reason, R_THREEFOLD | R_FORTY_MOVE) {
                        return (
                            false,
                            Some(format!("unsupported draw claim reason '{reason}'")),
                        );
                    }
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

        let claim_is_immediately_valid = command == CMD_DRAW_OFFER
            && payload
                .get(KEY_REASON)
                .and_then(value_as_str)
                .is_some_and(|reason| Self::claim_is_valid(&session, reason));
        if command == CMD_DRAW_OFFER && session.has_draw_offer() && !claim_is_immediately_valid {
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
            let state_command = if claim_is_immediately_valid {
                CMD_DRAW_ACCEPT
            } else {
                command
            };
            let mut candidate = session;
            return match SessionStateMachine::apply_command(&mut candidate, state_command, false) {
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
        let mut game = match Game::replay(&moves) {
            Ok(game) => game,
            Err(error) => return (false, Some(format!("Local replay failed: {error}"))),
        };
        match game.play(notation) {
            Ok(_) => (true, None),
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
        if session.app_id != APP_ID || session.app_version != APP_VERSION {
            return Err(LrgpError::Validation {
                code: ERR_UNSUPPORTED_APP.into(),
                message: "session app/version does not match Checkers".into(),
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
            if let Ok(game) = Game::replay(&moves) {
                Self::refresh_derived(&mut session, &game);
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
mod rules_tests {
    use super::*;

    fn sq(name: &str) -> usize {
        parse_square(name).expect("dark square")
    }

    fn position(side: Side, pieces: &[(&str, u8)]) -> Position {
        let mut board = [EMPTY; 64];
        for &(name, piece) in pieces {
            board[sq(name)] = piece;
        }
        Position { board, side }
    }

    fn notations(position: &Position) -> Vec<String> {
        let mut moves: Vec<String> = position
            .legal_moves()
            .iter()
            .map(CheckersMove::notation)
            .collect();
        moves.sort();
        moves
    }

    #[test]
    fn starting_position_matches_board_constant_and_opening_moves() {
        let start = Position::starting();
        assert_eq!(start.render(), STARTING_BOARD);
        assert_eq!(start.side, Side::Black);
        assert_eq!(start.board.iter().filter(|p| **p == BLACK_MAN).count(), 12);
        assert_eq!(start.board.iter().filter(|p| **p == WHITE_MAN).count(), 12);
        assert_eq!(
            notations(&start),
            [
                "b6 a5", "b6 c5", "d6 c5", "d6 e5", "f6 e5", "f6 g5", "h6 g5"
            ]
        );
    }

    #[test]
    fn square_and_path_notation_is_strict() {
        assert_eq!(parse_square("a1"), Some(0));
        assert_eq!(parse_square("h8"), Some(63));
        assert_eq!(square_name(sq("e7")), "e7");
        assert!(parse_path("a3 b4").is_some());
        assert!(parse_path("e7 c5 a3 c1").is_some());
        for bad in [
            "", "a3", "a3 b4 ", " a3 b4", "a3  b4", "a3-b4", "A3 b4", "a3 b5", "i1 h2", "a0 b1",
            "a9 b8", "a3\tb4",
        ] {
            assert!(parse_path(bad).is_none(), "accepted {bad:?}");
        }
        let too_long = vec!["a1"; MAX_PATH_SQUARES + 1].join(" ");
        assert!(parse_path(&too_long).is_none());
    }

    #[test]
    fn simple_move_example() {
        let pos = position(Side::White, &[("a3", WHITE_MAN), ("h8", BLACK_MAN)]);
        assert_eq!(notations(&pos), ["a3 b4"]);
    }

    #[test]
    fn double_jump_example_must_be_completed_and_crowns() {
        let pos = position(
            Side::Black,
            &[("a5", BLACK_MAN), ("b4", WHITE_MAN), ("d2", WHITE_MAN)],
        );
        assert_eq!(notations(&pos), ["a5 c3 e1"]);

        let mut game = Game::from_position(pos);
        assert!(game.play("a5 c3").is_err(), "partial jump must be illegal");
        game.play("a5 c3 e1").unwrap();
        assert_eq!(game.position.board[sq("e1")], BLACK_KING);
        assert_eq!(game.position.board[sq("b4")], EMPTY);
        assert_eq!(game.position.board[sq("d2")], EMPTY);
        assert_eq!(game.terminal(), ("win", R_NO_PIECES));
    }

    #[test]
    fn triple_jump_example() {
        let pos = position(
            Side::Black,
            &[
                ("e7", BLACK_MAN),
                ("d6", WHITE_MAN),
                ("b4", WHITE_MAN),
                ("b2", WHITE_MAN),
                ("h2", WHITE_MAN),
            ],
        );
        assert_eq!(notations(&pos), ["e7 c5 a3 c1"]);
    }

    #[test]
    fn captures_are_mandatory() {
        let pos = position(
            Side::White,
            &[("c3", WHITE_MAN), ("g3", WHITE_MAN), ("d4", BLACK_MAN)],
        );
        assert_eq!(notations(&pos), ["c3 e5"]);
    }

    #[test]
    fn crowning_ends_the_jump() {
        // As a king on d8 the piece could continue over e7, but crowning ends the move.
        let pos = position(
            Side::White,
            &[("b6", WHITE_MAN), ("c7", BLACK_MAN), ("e7", BLACK_MAN)],
        );
        assert_eq!(notations(&pos), ["b6 d8"]);
        let mut game = Game::from_position(pos);
        game.play("b6 d8").unwrap();
        assert_eq!(game.position.board[sq("d8")], WHITE_KING);
        assert_eq!(game.position.board[sq("e7")], BLACK_MAN);
    }

    #[test]
    fn kings_move_and_capture_backward_men_do_not() {
        let king = position(Side::White, &[("d4", WHITE_KING), ("h8", BLACK_MAN)]);
        assert_eq!(notations(&king), ["d4 c3", "d4 c5", "d4 e3", "d4 e5"]);

        let man = position(Side::White, &[("d4", WHITE_MAN), ("c3", BLACK_MAN)]);
        assert_eq!(notations(&man), ["d4 c5", "d4 e5"]);

        let king_capture = position(Side::White, &[("d4", WHITE_KING), ("c3", BLACK_MAN)]);
        assert_eq!(notations(&king_capture), ["d4 b2"]);
    }

    #[test]
    fn jumped_pieces_cannot_be_jumped_twice() {
        // A king circuit d4→f6→h4→f2→d4 would re-cross no piece, but a
        // second pass over e5 is impossible.
        let pos = position(
            Side::White,
            &[
                ("d4", WHITE_KING),
                ("e5", BLACK_MAN),
                ("g5", BLACK_MAN),
                ("g3", BLACK_MAN),
                ("e3", BLACK_MAN),
            ],
        );
        let moves = notations(&pos);
        assert!(moves.contains(&"d4 f6 h4 f2 d4".to_string()), "{moves:?}");
        for mv in pos.legal_moves() {
            let mut seen = mv.captured.clone();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), mv.captured.len());
        }
    }

    #[test]
    fn blocked_side_loses() {
        let pos = position(
            Side::Black,
            &[("a3", BLACK_MAN), ("b2", WHITE_MAN), ("c1", WHITE_MAN)],
        );
        assert!(pos.legal_moves().is_empty());
        assert_eq!(Game::from_position(pos).terminal(), ("win", R_NO_MOVES));
    }

    #[test]
    fn threefold_and_forty_move_claims() {
        let pos = position(Side::White, &[("d4", WHITE_KING), ("h8", BLACK_KING)]);
        let mut game = Game::from_position(pos);
        assert_eq!(game.claim_reason(), None);
        for _ in 0..2 {
            for mv in ["d4 e5", "h8 g7", "e5 d4", "g7 h8"] {
                game.play(mv).unwrap();
            }
        }
        assert!(game.claim_is_valid(R_THREEFOLD));
        assert!(!game.claim_is_valid(R_FORTY_MOVE));
        assert_eq!(game.claim_reason(), Some(R_THREEFOLD));

        game.quiet_plies = FORTY_MOVE_PLIES - 1;
        game.play("d4 e5").unwrap();
        assert!(game.claim_is_valid(R_FORTY_MOVE));
        assert_eq!(game.claim_reason(), Some(R_FORTY_MOVE));
    }

    #[test]
    fn man_moves_reset_the_forty_move_counter() {
        let pos = position(
            Side::White,
            &[("d4", WHITE_KING), ("a3", WHITE_MAN), ("h8", BLACK_KING)],
        );
        let mut game = Game::from_position(pos);
        game.play("d4 e5").unwrap();
        game.play("h8 g7").unwrap();
        assert_eq!(game.quiet_plies, 2);
        game.play("a3 b4").unwrap();
        assert_eq!(game.quiet_plies, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{pack_envelope, validate_envelope_size};
    use std::sync::MutexGuard;

    const SID: &str = "0123456789abcdef";
    const ALICE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // Serializes access to the FORCED_COIN static across tests.
    static COIN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct CoinReset(#[allow(dead_code)] MutexGuard<'static, ()>);
    impl Drop for CoinReset {
        fn drop(&mut self) {
            force_coin(None);
        }
    }

    fn pin_coin(challenger_is_black: bool) -> CoinReset {
        let guard = COIN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        force_coin(Some(challenger_is_black));
        CoinReset(guard)
    }

    /// Alice challenges, Bob accepts. With the coin pinned to `true` Alice is Black.
    fn setup_active(app: &CheckersApp) {
        app.handle_outgoing(SID, CMD_CHALLENGE, &HashMap::new(), ALICE);
        app.bind_session_peer(SID, ALICE, BOB).unwrap();
        let challenge = app.handle_incoming(SID, CMD_CHALLENGE, &HashMap::new(), ALICE, BOB);
        assert!(challenge.error.is_none());
        let accept = app.handle_outgoing(SID, CMD_ACCEPT, &HashMap::new(), BOB);
        assert_eq!(accept.payload.keys().collect::<Vec<_>>(), vec![KEY_BLACK]);
        let accepted = app.handle_incoming(SID, CMD_ACCEPT, &accept.payload, BOB, ALICE);
        assert!(accepted.error.is_none(), "{:?}", accepted.error);
    }

    fn intent(path: &str) -> HashMap<String, rmpv::Value> {
        HashMap::from([(KEY_MOVE.into(), rmpv::Value::String(path.into()))])
    }

    fn play(app: &CheckersApp, path: &str, mover: &str, receiver: &str) -> OutgoingResult {
        let (valid, message) = app.validate_outgoing_action(SID, CMD_MOVE, &intent(path), mover);
        assert!(valid, "outgoing {path} rejected: {message:?}");
        let out = app.handle_outgoing(SID, CMD_MOVE, &intent(path), mover);
        assert!(!out.payload.is_empty(), "{}", out.fallback_text);
        let incoming = app.handle_incoming(SID, CMD_MOVE, &out.payload, mover, receiver);
        assert!(incoming.error.is_none(), "{:?}", incoming.error);
        out
    }

    fn assert_shared_state(app: &CheckersApp) {
        let mut left = app.get_session(SID, ALICE).unwrap().metadata;
        let mut right = app.get_session(SID, BOB).unwrap().metadata;
        left.remove("my_color");
        right.remove("my_color");
        assert_eq!(left, right);
    }

    #[test]
    fn manifest_is_complete() {
        let manifest = CheckersApp::new().manifest();
        assert_eq!(manifest.app_id, APP_ID);
        assert_eq!(manifest.version, 1);
        assert_eq!(manifest.display_name, "Checkers");
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

    #[test]
    fn accept_assigns_colors_from_coin() {
        for challenger_is_black in [true, false] {
            let _coin = pin_coin(challenger_is_black);
            let app = CheckersApp::new();
            setup_active(&app);
            let alice = app.get_session(SID, ALICE).unwrap();
            let bob = app.get_session(SID, BOB).unwrap();
            let black = if challenger_is_black { ALICE } else { BOB };
            assert_eq!(alice.status, STATUS_ACTIVE);
            assert_eq!(bob.status, STATUS_ACTIVE);
            assert_eq!(meta_str(&alice.metadata, "first_turn"), black);
            assert_eq!(meta_str(&alice.metadata, "turn"), black);
            assert_eq!(
                meta_str(&alice.metadata, "my_color"),
                if challenger_is_black { "b" } else { "w" }
            );
            assert_eq!(
                meta_str(&bob.metadata, "my_color"),
                if challenger_is_black { "w" } else { "b" }
            );
            assert_eq!(meta_string_list(&alice.metadata, "legal_moves").len(), 7);
            assert_shared_state(&app);
        }
    }

    #[test]
    fn accept_with_unbound_black_player_is_rejected() {
        let app = CheckersApp::new();
        app.handle_outgoing(SID, CMD_CHALLENGE, &HashMap::new(), ALICE);
        app.bind_session_peer(SID, ALICE, BOB).unwrap();
        let forged = HashMap::from([(KEY_BLACK.into(), rmpv::Value::String("cc".into()))]);
        let result = app.handle_incoming(SID, CMD_ACCEPT, &forged, BOB, ALICE);
        assert!(result.error.is_some());
        assert_eq!(app.get_session(SID, ALICE).unwrap().status, STATUS_PENDING);
    }

    #[test]
    fn decline_flow() {
        let app = CheckersApp::new();
        app.handle_outgoing(SID, CMD_CHALLENGE, &HashMap::new(), ALICE);
        app.handle_incoming(SID, CMD_CHALLENGE, &HashMap::new(), ALICE, BOB);
        let out = app.handle_outgoing(SID, CMD_DECLINE, &HashMap::new(), BOB);
        assert_eq!(out.fallback_text, "[LRGP Checkers] Challenge declined");
        assert_eq!(app.get_session(SID, BOB).unwrap().status, STATUS_DECLINED);
        app.handle_incoming(SID, CMD_DECLINE, &HashMap::new(), BOB, ALICE);
        assert_eq!(app.get_session(SID, ALICE).unwrap().status, STATUS_DECLINED);
    }

    #[test]
    fn forced_capture_exchange_converges() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);

        let first = play(&app, "d6 e5", ALICE, BOB);
        assert_eq!(value_as_u64(&first.payload[KEY_PLY]), Some(0));
        assert_eq!(first.fallback_text, "[LRGP Checkers] d6 e5");
        play(&app, "c3 d4", BOB, ALICE);

        let alice = app.get_session(SID, ALICE).unwrap();
        assert_eq!(alice.metadata["must_capture"], JsonValue::Bool(true));
        assert_eq!(meta_string_list(&alice.metadata, "legal_moves"), ["e5 c3"]);
        let (valid, message) = app.validate_outgoing_action(SID, CMD_MOVE, &intent("b6 a5"), ALICE);
        assert!(!valid);
        assert!(message.unwrap().contains("not legal"));

        play(&app, "e5 c3", ALICE, BOB);
        let mut replies =
            meta_string_list(&app.get_session(SID, BOB).unwrap().metadata, "legal_moves");
        replies.sort();
        assert_eq!(replies, ["b2 d4", "d2 b4"]);
        let last = play(&app, "d2 b4", BOB, ALICE);
        assert_eq!(value_as_u64(&last.payload[KEY_PLY]), Some(3));

        assert_shared_state(&app);
        let alice = app.get_session(SID, ALICE).unwrap();
        assert_eq!(alice.metadata["move_count"], JsonValue::from(4));
        assert_eq!(meta_str(&alice.metadata, "turn"), ALICE);
        assert_eq!(meta_str(&alice.metadata, "last_move"), "d2 b4");
    }

    /// Deterministic self-play to a decisive result keeps both peers in
    /// lockstep, exercises multi-jumps and crowning, and ends with a
    /// verified winning move.
    #[test]
    fn self_play_reaches_a_verified_win() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);

        let mut last = None;
        for ply in 0..400usize {
            let session = app.get_session(SID, ALICE).unwrap();
            if session.status == STATUS_COMPLETED {
                break;
            }
            let legal = meta_string_list(&session.metadata, "legal_moves");
            let mover = meta_str(&session.metadata, "turn");
            let receiver = if mover == ALICE { BOB } else { ALICE };
            // Prefer the longest capture, then vary deterministically.
            let longest = legal.iter().map(String::len).max().unwrap();
            let candidates: Vec<&String> = legal.iter().filter(|m| m.len() == longest).collect();
            let choice = candidates[(ply * 7 + 5) % candidates.len()].clone();
            let out = play(&app, &choice, &mover, receiver);
            let size = validate_envelope_size(
                &pack_envelope(
                    APP_ID,
                    APP_VERSION,
                    CMD_MOVE,
                    SID,
                    Some(out.payload.clone()),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
            assert!(size <= 150, "move envelope {size} bytes");
            assert_shared_state(&app);
            last = Some((out, mover));
        }

        let (out, mover) = last.unwrap();
        let alice = app.get_session(SID, ALICE).unwrap();
        assert_eq!(alice.status, STATUS_COMPLETED, "self-play did not finish");
        assert_eq!(app.get_session(SID, BOB).unwrap().status, STATUS_COMPLETED);
        assert_eq!(value_as_str(&out.payload[KEY_TERMINAL]), Some("win"));
        assert_eq!(value_as_str(&out.payload[KEY_WINNER]), Some(mover.as_str()));
        assert_eq!(meta_str(&alice.metadata, "winner"), mover);
        assert!(matches!(
            meta_str(&alice.metadata, "terminal_reason").as_str(),
            R_NO_PIECES | R_NO_MOVES
        ));
        assert!(meta_string_list(&alice.metadata, "legal_moves").is_empty());
        assert!(out.fallback_text.contains(" wins ("));

        // A completed game survives hydration on both sides.
        for identity in [ALICE, BOB] {
            let record = app.get_session(SID, identity).unwrap();
            CheckersApp::new().upsert_session(record).unwrap();
        }

        let (valid, _) = app.validate_outgoing_action(SID, CMD_MOVE, &intent("a3 b4"), ALICE);
        assert!(!valid);
    }

    #[test]
    fn out_of_turn_illegal_and_unknown_session_moves_are_rejected() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);

        let out = app.handle_outgoing(SID, CMD_MOVE, &intent("c3 d4"), BOB);
        assert_eq!(out.fallback_text, "[LRGP Checkers] Not your turn");
        assert!(out.payload.is_empty());

        let out = app.handle_outgoing(SID, CMD_MOVE, &intent("d6 d5"), ALICE);
        assert!(out.fallback_text.contains("Illegal"));
        assert!(out.payload.is_empty());

        let out = app.handle_outgoing("ffffffffffffffff", CMD_MOVE, &intent("d6 e5"), ALICE);
        assert_eq!(out.fallback_text, "[LRGP Checkers] Session not found");

        let (valid, _) = app.validate_outgoing_action(SID, CMD_MOVE, &intent("d6 e5"), BOB);
        assert!(!valid);
        let (valid, _) = app.validate_outgoing_action(SID, CMD_MOVE, &HashMap::new(), ALICE);
        assert!(!valid);
        assert_eq!(
            app.get_session(SID, ALICE).unwrap().metadata["move_count"],
            JsonValue::from(0)
        );
    }

    #[test]
    fn forged_wire_moves_are_rejected_without_mutation() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);
        let before = app.get_session(SID, BOB).unwrap();

        let wire = |path: &str, ply: u64, terminal: &str| {
            HashMap::from([
                (KEY_MOVE.into(), rmpv::Value::String(path.into())),
                (KEY_PLY.into(), rmpv::Value::Integer(ply.into())),
                (KEY_TERMINAL.into(), rmpv::Value::String(terminal.into())),
            ])
        };

        let cases = [
            (wire("d6 e5", 1, ""), ALICE),     // ply mismatch
            (wire("d6 d5", 0, ""), ALICE),     // light square / malformed
            (wire("d6 c7", 0, ""), ALICE),     // illegal direction
            (wire("c3 d4", 0, ""), BOB),       // out of turn
            (wire("d6 e5", 0, "draw"), ALICE), // unsupported terminal
        ];
        for (payload, sender) in cases {
            let result = app.handle_incoming(SID, CMD_MOVE, &payload, sender, BOB);
            assert!(result.error.is_some(), "accepted {payload:?}");
        }

        let mut forged_win = wire("d6 e5", 0, "win");
        forged_win.insert(KEY_REASON.into(), rmpv::Value::String(R_NO_MOVES.into()));
        forged_win.insert(KEY_WINNER.into(), rmpv::Value::String(ALICE.into()));
        assert!(
            app.handle_incoming(SID, CMD_MOVE, &forged_win, ALICE, BOB)
                .error
                .is_some()
        );

        let mut extra = wire("d6 e5", 0, "");
        extra.insert("extra".into(), rmpv::Value::Nil);
        assert!(
            app.handle_incoming(SID, CMD_MOVE, &extra, ALICE, BOB)
                .error
                .is_some()
        );

        let junk = HashMap::from([("extra".into(), rmpv::Value::Boolean(true))]);
        assert!(
            app.handle_incoming("1111111111111111", CMD_CHALLENGE, &junk, ALICE, BOB)
                .error
                .is_some()
        );
        assert!(app.get_session("1111111111111111", BOB).is_none());

        let after = app.get_session(SID, BOB).unwrap();
        assert_eq!(after.metadata, before.metadata);
    }

    #[test]
    fn validate_move_rejects_empty_turn() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);
        let mut session = app.get_session(SID, BOB).unwrap();
        session
            .metadata
            .insert("turn".into(), JsonValue::String("".into()));
        let payload = HashMap::from([
            (KEY_MOVE.into(), rmpv::Value::String("d6 e5".into())),
            (KEY_PLY.into(), rmpv::Value::Integer(0.into())),
            (KEY_TERMINAL.into(), rmpv::Value::String("".into())),
        ]);
        let error = app.validate_move(&session, &payload, ALICE).unwrap_err();
        assert!(error.contains("Turn is required"));
    }

    #[test]
    fn resign_completes_both_sides() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);

        let out = app.handle_outgoing(SID, CMD_RESIGN, &HashMap::new(), ALICE);
        assert_eq!(out.fallback_text, "[LRGP Checkers] Resigned.");
        app.handle_incoming(SID, CMD_RESIGN, &HashMap::new(), ALICE, BOB);
        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(session.status, STATUS_COMPLETED);
            assert_eq!(meta_str(&session.metadata, "terminal"), "win");
            assert_eq!(meta_str(&session.metadata, "terminal_reason"), R_RESIGN);
            assert_eq!(meta_str(&session.metadata, "winner"), BOB);
            assert_eq!(meta_str(&session.metadata, "turn"), "");
        }
        assert_shared_state(&app);
    }

    #[test]
    fn draw_offer_ownership_and_agreement() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);

        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE);
        let offer = app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE, BOB);
        assert!(offer.error.is_none());
        assert_eq!(
            app.get_session(SID, BOB).unwrap().draw_offered_by(),
            Some(ALICE)
        );

        // Owner cannot answer or replace its own offer.
        let (valid, _) = app.validate_outgoing_action(SID, CMD_DRAW_ACCEPT, &HashMap::new(), ALICE);
        assert!(!valid);
        let (valid, _) = app.validate_outgoing_action(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE);
        assert!(!valid);
        let duplicate = app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE, BOB);
        assert!(duplicate.error.is_some());
        let self_accept = app.handle_incoming(SID, CMD_DRAW_ACCEPT, &HashMap::new(), ALICE, BOB);
        assert!(self_accept.error.is_some());

        let out = app.handle_outgoing(SID, CMD_DRAW_ACCEPT, &HashMap::new(), BOB);
        assert_eq!(out.fallback_text, "[LRGP Checkers] Draw accepted");
        app.handle_incoming(SID, CMD_DRAW_ACCEPT, &HashMap::new(), BOB, ALICE);
        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(session.status, STATUS_COMPLETED);
            assert_eq!(meta_str(&session.metadata, "terminal"), "draw");
            assert_eq!(meta_str(&session.metadata, "terminal_reason"), R_AGREEMENT);
            assert_eq!(session.draw_offered_by(), None);
        }
        assert_shared_state(&app);
    }

    #[test]
    fn draw_decline_and_move_clear_the_offer() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);

        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB);
        app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB, ALICE);
        app.handle_outgoing(SID, CMD_DRAW_DECLINE, &HashMap::new(), ALICE);
        app.handle_incoming(SID, CMD_DRAW_DECLINE, &HashMap::new(), ALICE, BOB);
        assert!(!app.get_session(SID, ALICE).unwrap().has_draw_offer());
        assert!(!app.get_session(SID, BOB).unwrap().has_draw_offer());

        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB);
        app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB, ALICE);
        play(&app, "d6 e5", ALICE, BOB);
        assert!(!app.get_session(SID, ALICE).unwrap().has_draw_offer());
        assert!(!app.get_session(SID, BOB).unwrap().has_draw_offer());
        assert_shared_state(&app);
    }

    #[test]
    fn ineligible_claim_degrades_to_plain_offer() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);

        let claim = HashMap::from([(KEY_REASON.into(), rmpv::Value::String(R_THREEFOLD.into()))]);
        let (valid, _) = app.validate_outgoing_action(SID, CMD_DRAW_OFFER, &claim, ALICE);
        assert!(valid);
        let out = app.handle_outgoing(SID, CMD_DRAW_OFFER, &claim, ALICE);
        assert_eq!(out.payload.keys().collect::<Vec<_>>(), vec![KEY_REASON]);
        assert_eq!(
            out.fallback_text,
            "[LRGP Checkers] Claimed threefold repetition"
        );
        app.handle_incoming(SID, CMD_DRAW_OFFER, &out.payload, ALICE, BOB);
        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(session.status, STATUS_ACTIVE);
            assert_eq!(session.draw_offered_by(), Some(ALICE));
        }

        let bad = HashMap::from([(KEY_REASON.into(), rmpv::Value::String("50m".into()))]);
        let (valid, _) = app.validate_outgoing_action(SID, CMD_DRAW_OFFER, &bad, BOB);
        assert!(!valid);
        assert!(
            app.handle_incoming(SID, CMD_DRAW_OFFER, &bad, ALICE, BOB)
                .error
                .is_some()
        );
    }

    #[test]
    fn hydration_replays_history_and_rejects_tampering() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);
        play(&app, "d6 e5", ALICE, BOB);
        play(&app, "c3 d4", BOB, ALICE);

        let record = app.get_session(SID, BOB).unwrap();
        let fresh = CheckersApp::new();
        fresh.upsert_session(record.clone()).unwrap();
        assert_eq!(
            fresh.get_session(SID, BOB).unwrap().metadata,
            record.metadata
        );

        let tamper = |key: &str, value: JsonValue| {
            let mut copy = record.clone();
            copy.metadata.insert(key.into(), value);
            CheckersApp::new().upsert_session(copy)
        };
        assert!(tamper("board", JsonValue::String(STARTING_BOARD.into())).is_err());
        assert!(tamper("turn", JsonValue::String(BOB.into())).is_err());
        assert!(tamper("move_count", JsonValue::from(1)).is_err());
        assert!(tamper("my_color", JsonValue::String("b".into())).is_err());
        assert!(tamper("first_turn", JsonValue::String(BOB.into())).is_err());
        assert!(tamper("terminal", JsonValue::String("win".into())).is_err());
        assert!(
            tamper(
                "moves",
                JsonValue::Array(vec!["d6 e5".into(), "c3 c4".into()])
            )
            .is_err()
        );

        let mut completed = record.clone();
        completed.status = STATUS_COMPLETED.into();
        completed
            .metadata
            .insert("turn".into(), JsonValue::String("".into()));
        completed
            .metadata
            .insert("terminal".into(), JsonValue::String("draw".into()));
        completed.metadata.insert(
            "terminal_reason".into(),
            JsonValue::String(R_THREEFOLD.into()),
        );
        assert!(CheckersApp::new().upsert_session(completed).is_err());

        // A legacy ownerless draw flag is normalized, not answerable.
        let mut legacy = record.clone();
        legacy
            .metadata
            .insert("draw_offered".into(), JsonValue::Bool(true));
        let fresh = CheckersApp::new();
        fresh.upsert_session(legacy).unwrap();
        assert!(!fresh.get_session(SID, BOB).unwrap().has_draw_offer());

        let mut wrong_app = record.clone();
        wrong_app.app_id = "chess".into();
        assert!(CheckersApp::new().upsert_session(wrong_app).is_err());
        let mut bad_sid = record;
        bad_sid.session_id = "g1".into();
        assert!(CheckersApp::new().upsert_session(bad_sid).is_err());
    }

    #[test]
    fn hydration_applies_expiry() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);
        let mut record = app.get_session(SID, ALICE).unwrap();
        record.last_action_at = 0.0;
        let fresh = CheckersApp::new();
        fresh.upsert_session(record).unwrap();
        assert_eq!(
            fresh.get_session(SID, ALICE).unwrap().status,
            STATUS_EXPIRED
        );
        assert!(fresh.authorize_incoming(SID, CMD_MOVE, BOB, ALICE).is_err());
    }

    #[test]
    fn expiry_clears_draw_offer_and_round_trips() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);
        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE);

        let mut record = app.get_session(SID, ALICE).unwrap();
        assert!(record.has_draw_offer());
        record.last_action_at = 0.0;
        let fresh = CheckersApp::new();
        fresh.upsert_session(record).unwrap();
        let expired = fresh.get_session(SID, ALICE).unwrap();
        assert_eq!(expired.status, STATUS_EXPIRED);
        assert!(!expired.has_draw_offer());

        // The expired record, as persisted, hydrates again.
        CheckersApp::new().upsert_session(expired.clone()).unwrap();
        let mut stale = expired;
        stale.set_draw_offer(ALICE);
        let fresh = CheckersApp::new();
        fresh.upsert_session(stale).unwrap();
        assert!(!fresh.get_session(SID, ALICE).unwrap().has_draw_offer());
    }

    #[test]
    fn participant_authorization() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
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

    #[test]
    fn snapshot_rollback() {
        let _coin = pin_coin(true);
        let app = CheckersApp::new();
        setup_active(&app);

        let snap = app.snapshot_session(SID, ALICE);
        play(&app, "d6 e5", ALICE, BOB);
        assert_eq!(
            meta_str(&app.get_session(SID, ALICE).unwrap().metadata, "last_move"),
            "d6 e5"
        );
        app.rollback_session(SID, ALICE, snap);
        let restored = app.get_session(SID, ALICE).unwrap();
        assert_eq!(meta_str(&restored.metadata, "last_move"), "");
        assert_eq!(restored.metadata["move_count"], JsonValue::from(0));

        let fresh = CheckersApp::new();
        let none = fresh.snapshot_session("2222222222222222", ALICE);
        fresh.handle_outgoing("2222222222222222", CMD_CHALLENGE, &HashMap::new(), ALICE);
        fresh.rollback_session("2222222222222222", ALICE, none);
        assert!(fresh.get_session("2222222222222222", ALICE).is_none());
    }

    #[test]
    fn worst_case_move_envelope_fits_budget() {
        let path = [
            "a1", "c3", "e5", "g7", "e5", "c3", "a1", "c3", "e5", "g7", "e5", "c3", "a1",
        ]
        .join(" ");
        assert_eq!(path.split(' ').count(), MAX_PATH_SQUARES);
        let payload = HashMap::from([
            (KEY_MOVE.into(), rmpv::Value::String(path.into())),
            (KEY_PLY.into(), rmpv::Value::Integer(65_535.into())),
            (KEY_TERMINAL.into(), rmpv::Value::String("win".into())),
            (KEY_REASON.into(), rmpv::Value::String(R_NO_PIECES.into())),
            (KEY_WINNER.into(), rmpv::Value::String(ALICE.into())),
        ]);
        let envelope =
            pack_envelope(APP_ID, APP_VERSION, CMD_MOVE, SID, Some(payload), None).unwrap();
        let size = validate_envelope_size(&envelope).expect("envelope size OK");
        assert!(size <= 150, "worst-case move envelope {size} bytes");
    }

    #[test]
    fn render_fallback() {
        let app = CheckersApp::new();
        assert_eq!(
            app.render_fallback(CMD_CHALLENGE, &HashMap::new()),
            "[LRGP Checkers] Sent a challenge!"
        );
        let mut p = intent("a5 c3 e1");
        p.insert(KEY_TERMINAL.into(), rmpv::Value::String("".into()));
        assert_eq!(
            app.render_fallback(CMD_MOVE, &p),
            "[LRGP Checkers] a5 c3 e1"
        );
        p.insert(KEY_TERMINAL.into(), rmpv::Value::String("win".into()));
        p.insert(KEY_REASON.into(), rmpv::Value::String(R_NO_PIECES.into()));
        assert_eq!(
            app.render_fallback(CMD_MOVE, &p),
            "[LRGP Checkers] a5 c3 e1 wins (no pieces left)"
        );
        let claim = HashMap::from([(KEY_REASON.into(), rmpv::Value::String(R_FORTY_MOVE.into()))]);
        assert_eq!(
            app.render_fallback(CMD_DRAW_OFFER, &claim),
            "[LRGP Checkers] Claimed forty-move rule"
        );
    }
}
