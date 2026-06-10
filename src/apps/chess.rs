//! Chess app. Wire format is UCI-only `{m, n, x, r, w, dr}`; FEN and legal
//! moves are local-only. Both peers replay UCI history (VALIDATION_BOTH).
//! Terminal reason codes are 2-3 chars to keep envelopes ≤150 bytes.

use std::collections::HashMap;
use std::sync::Mutex;

use cozy_chess::{Board, Color, Move, Piece, Square};
use serde_json::Value as JsonValue;

use crate::app_base::{AppManifest, GameApp, IncomingResult, OutgoingResult};
use crate::constants::*;
use crate::envelope::{value_as_str, value_as_u64};
use crate::session::{Session, SessionStateMachine};

const APP_ID: &str = "chess";
const APP_VERSION: u32 = 1;

pub const STARTING_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

// Terminal reason codes (≤3 chars to fit envelope budget).
const R_CHECKMATE: &str = "cm";
const R_STALEMATE: &str = "sm";
const R_INSUFFICIENT: &str = "ins";
const R_THREEFOLD: &str = "3fr";
const R_FIFTY_MOVE: &str = "50m";
const R_RESIGN: &str = "rsn";
const R_AGREEMENT: &str = "agr";

// Wire payload keys.
const KEY_MOVE: &str = "m"; // UCI move, e.g. "e2e4" or "e7e8q"
const KEY_PLY: &str = "n"; // ply counter (u32)
const KEY_TERMINAL: &str = "x"; // "" | "win" | "draw"
const KEY_REASON: &str = "r"; // terminal / draw-claim reason
const KEY_WINNER: &str = "w"; // winner identity hash (terminal="win")
const KEY_WHITE: &str = "w"; // White-player hash (ACCEPT payload)

#[cfg(any(test, feature = "test-helpers"))]
static FORCED_COIN: Mutex<Option<bool>> = Mutex::new(None);

/// `true` if the challenger gets White. Pin via `force_coin` in tests.
fn flip_responder_coin() -> bool {
    #[cfg(any(test, feature = "test-helpers"))]
    {
        if let Ok(guard) = FORCED_COIN.lock()
            && let Some(v) = *guard
        {
            return v;
        }
    }
    use rand::RngCore;
    (rand::thread_rng().next_u32() & 1) == 0
}

/// Pin the coin-flip for tests. Process-global; callers must serialize.
#[cfg(any(test, feature = "test-helpers"))]
pub fn force_coin(challenger_is_white: Option<bool>) {
    if let Ok(mut guard) = FORCED_COIN.lock() {
        *guard = challenger_is_white;
    }
}

fn short(s: &str) -> &str {
    let n = s.len().min(8);
    s.get(..n).unwrap_or("")
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

fn meta_str(meta: &HashMap<String, JsonValue>, key: &str) -> String {
    meta.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn meta_i64(meta: &HashMap<String, JsonValue>, key: &str) -> i64 {
    meta.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
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

fn legal_uci_moves(board: &Board) -> Vec<String> {
    let mut moves: Vec<Move> = Vec::new();
    board.generate_moves(|mvs| {
        moves.extend(mvs);
        false
    });
    moves.into_iter().map(|m| format!("{}", m)).collect()
}

fn in_check(board: &Board) -> bool {
    !board.checkers().is_empty()
}

/// `true` if the square is light. A1 is dark; `(file + rank)` even = dark.
fn square_is_light(sq: Square) -> bool {
    let idx = sq as u8;
    let file = idx & 0b111;
    let rank = idx >> 3;
    (file + rank) & 1 == 1
}

/// Conservative K/K, KN/K, KB/K, KB/KB-same-color check. Pawn/rook/queen → sufficient.
fn insufficient_material(board: &Board) -> bool {
    if !board.pieces(Piece::Pawn).is_empty()
        || !board.pieces(Piece::Rook).is_empty()
        || !board.pieces(Piece::Queen).is_empty()
    {
        return false;
    }

    let white = board.colors(Color::White);
    let black = board.colors(Color::Black);
    let wn = (board.pieces(Piece::Knight) & white).len();
    let bn = (board.pieces(Piece::Knight) & black).len();
    let wb = (board.pieces(Piece::Bishop) & white).len();
    let bb = (board.pieces(Piece::Bishop) & black).len();
    let w_minors = wn + wb;
    let b_minors = bn + bb;

    if w_minors == 0 && b_minors == 0 {
        return true;
    }
    if (w_minors == 1 && b_minors == 0) || (b_minors == 1 && w_minors == 0) {
        return true;
    }
    // K+B vs K+B same color.
    if wn == 0 && bn == 0 && wb == 1 && bb == 1 {
        let mut w_sq = None;
        let mut b_sq = None;
        for sq in board.pieces(Piece::Bishop) & white {
            w_sq = Some(sq);
        }
        for sq in board.pieces(Piece::Bishop) & black {
            b_sq = Some(sq);
        }
        if let (Some(ws), Some(bs)) = (w_sq, b_sq)
            && square_is_light(ws) == square_is_light(bs)
        {
            return true;
        }
    }
    false
}

/// Replay UCI history from the starting position.
fn replay_moves(moves: &[String]) -> Result<Board, String> {
    let mut board = Board::default();
    for uci in moves {
        let mv: Move = uci
            .parse()
            .map_err(|_| format!("UCI parse failed: {uci}"))?;
        let mut legal: Vec<Move> = Vec::new();
        board.generate_moves(|mvs| {
            legal.extend(mvs);
            false
        });
        if !legal.contains(&mv) {
            return Err(format!("Illegal move replayed: {uci}"));
        }
        board.play(mv);
    }
    Ok(board)
}

/// Zobrist hash per visited position; length == `moves.len() + 1`.
fn hash_history(moves: &[String]) -> Vec<u64> {
    let mut board = Board::default();
    let mut hashes = Vec::with_capacity(moves.len() + 1);
    hashes.push(board.hash());
    for uci in moves {
        let Ok(mv) = uci.parse::<Move>() else {
            break;
        };
        board.play(mv);
        hashes.push(board.hash());
    }
    hashes
}

/// Threefold-repetition or 50-move-rule claim eligibility. 50-move wins ties.
fn claim_reason(board: &Board, moves: &[String]) -> Option<&'static str> {
    if board.halfmove_clock() >= 100 {
        return Some(R_FIFTY_MOVE);
    }
    let hashes = hash_history(moves);
    let current = board.hash();
    let count = hashes.iter().filter(|&&h| h == current).count();
    if count >= 3 {
        return Some(R_THREEFOLD);
    }
    None
}

/// Returns `(terminal, reason)` where terminal is `""`/`"win"`/`"draw"`.
/// On `"win"` the mover (opposite of `board.side_to_move()`) won.
fn detect_auto_terminal(board: &Board) -> (&'static str, &'static str) {
    let legal = legal_uci_moves(board);
    if legal.is_empty() {
        if in_check(board) {
            return ("win", R_CHECKMATE);
        }
        return ("draw", R_STALEMATE);
    }
    if insufficient_material(board) {
        return ("draw", R_INSUFFICIENT);
    }
    ("", "")
}

pub struct ChessApp {
    sessions: Mutex<HashMap<(String, String), Session>>,
}

impl ChessApp {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn get_session(&self, session_id: &str, identity_id: &str) -> Option<Session> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(&(session_id.to_string(), identity_id.to_string()))
            .cloned()
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
        m.insert("fen".into(), JsonValue::String(STARTING_FEN.into()));
        m.insert("moves".into(), JsonValue::Array(vec![]));
        m.insert("my_color".into(), JsonValue::String("".into()));
        m.insert("first_turn".into(), JsonValue::String("".into()));
        m.insert("turn".into(), JsonValue::String("".into()));
        m.insert("move_count".into(), JsonValue::Number(0.into()));
        m.insert("winner".into(), JsonValue::String("".into()));
        m.insert("terminal".into(), JsonValue::String("".into()));
        m.insert("terminal_reason".into(), JsonValue::String("".into()));
        m.insert("draw_offered".into(), JsonValue::Bool(false));
        m.insert("draw_offer_reason".into(), JsonValue::String("".into()));
        m.insert("last_move".into(), JsonValue::String("".into()));
        m.insert("in_check".into(), JsonValue::Bool(false));
        m.insert("legal_moves".into(), JsonValue::Array(vec![]));
        m
    }

    /// Refresh derived session metadata (fen, legal_moves, in_check, draw_offer_reason).
    fn refresh_derived(session: &mut Session, board: &Board, moves: &[String]) {
        let fen = format!("{}", board);
        session
            .metadata
            .insert("fen".into(), JsonValue::String(fen));
        session.metadata.insert(
            "legal_moves".into(),
            string_list_value(&legal_uci_moves(board)),
        );
        session
            .metadata
            .insert("in_check".into(), JsonValue::Bool(in_check(board)));
        let claim = claim_reason(board, moves).unwrap_or("");
        session.metadata.insert(
            "draw_offer_reason".into(),
            JsonValue::String(claim.to_string()),
        );
    }

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
            emit: Some(emit_event("challenge", session_id, APP_ID, sender_hash)),
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

        // Responder is authoritative on color assignment via the `w` field.
        let white_hash = payload
            .get(KEY_WHITE)
            .and_then(|v| value_as_str(v))
            .unwrap_or("")
            .to_string();
        if white_hash.is_empty() {
            return error_result(ERR_PROTOCOL_ERROR, "ACCEPT missing white-player hash");
        }
        let my_color = if white_hash == identity_id { "w" } else { "b" };

        tracing::info!(
            target: "chess_trace",
            step = "accept_in.learned_color",
            sid = %short(session_id),
            my = %short(identity_id),
            from = %short(sender_hash),
            white = %short(&white_hash),
            my_color = %my_color,
            "accept_in learned color from payload"
        );

        session
            .metadata
            .insert("fen".into(), JsonValue::String(STARTING_FEN.into()));
        session
            .metadata
            .insert("moves".into(), JsonValue::Array(vec![]));
        session
            .metadata
            .insert("first_turn".into(), JsonValue::String(white_hash.clone()));
        session
            .metadata
            .insert("turn".into(), JsonValue::String(white_hash));
        session
            .metadata
            .insert("my_color".into(), JsonValue::String(my_color.into()));

        let board = Board::default();
        Self::refresh_derived(&mut session, &board, &[]);

        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("accept", session_id, APP_ID, sender_hash)),
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
            emit: Some(emit_event("decline", session_id, APP_ID, sender_hash)),
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
            tracing::warn!(
                target: "chess_trace",
                step = "move_in.rejected",
                sid = %short(session_id),
                my = %short(identity_id),
                from = %short(sender_hash),
                err = %err_msg.as_deref().unwrap_or(""),
                "move_in rejected by validate_move"
            );
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

        let uci = payload
            .get(KEY_MOVE)
            .and_then(|v| value_as_str(v))
            .unwrap_or("")
            .to_string();

        let mut moves = meta_string_list(&session.metadata, "moves");
        moves.push(uci.clone());
        let board = match replay_moves(&moves) {
            Ok(b) => b,
            Err(e) => return error_result(ERR_INVALID_MOVE, &e),
        };

        let first_turn = meta_str(&session.metadata, "first_turn");
        let next_turn_hash = if session.contact_hash.is_empty() {
            String::new()
        } else if board.side_to_move() == Color::White {
            first_turn.clone()
        } else if first_turn == identity_id {
            session.contact_hash.clone()
        } else {
            identity_id.to_string()
        };

        let (terminal, reason) = detect_auto_terminal(&board);
        let winner_hash = if terminal == "win" {
            sender_hash.to_string()
        } else {
            String::new()
        };

        session
            .metadata
            .insert("moves".into(), string_list_value(&moves));
        session.metadata.insert(
            "move_count".into(),
            JsonValue::Number((moves.len() as i64).into()),
        );
        session.metadata.insert(
            "turn".into(),
            JsonValue::String(if terminal.is_empty() {
                next_turn_hash
            } else {
                String::new()
            }),
        );
        session
            .metadata
            .insert("last_move".into(), JsonValue::String(uci.clone()));
        session
            .metadata
            .insert("terminal".into(), JsonValue::String(terminal.to_string()));
        session.metadata.insert(
            "terminal_reason".into(),
            JsonValue::String(reason.to_string()),
        );
        session
            .metadata
            .insert("winner".into(), JsonValue::String(winner_hash));
        session
            .metadata
            .insert("draw_offered".into(), JsonValue::Bool(false));

        Self::refresh_derived(&mut session, &board, &moves);

        let _ = SessionStateMachine::apply_command(&mut session, CMD_MOVE, !terminal.is_empty());
        session.unread = 1;
        self.save_session(&session);

        let mut emit = emit_event("move", session_id, APP_ID, sender_hash);
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

        let _ = SessionStateMachine::apply_command(&mut session, CMD_RESIGN, false);
        session
            .metadata
            .insert("terminal".into(), JsonValue::String("win".into()));
        session
            .metadata
            .insert("terminal_reason".into(), JsonValue::String(R_RESIGN.into()));
        // Sender resigned; opponent (= identity_id locally) wins.
        session
            .metadata
            .insert("winner".into(), JsonValue::String(identity_id.to_string()));
        session
            .metadata
            .insert("turn".into(), JsonValue::String("".into()));
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("resign", session_id, APP_ID, sender_hash)),
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

        // draw_offer with a valid `r` reason is a FIDE claim (threefold /
        // 50-move). Verify locally; valid claim auto-accepts, otherwise plain offer.
        let claimed_reason = payload
            .get(KEY_REASON)
            .and_then(|v| value_as_str(v))
            .unwrap_or("");

        let is_valid_claim = if claimed_reason == R_THREEFOLD || claimed_reason == R_FIFTY_MOVE {
            let moves = meta_string_list(&session.metadata, "moves");
            if let Ok(board) = replay_moves(&moves) {
                claim_reason(&board, &moves) == Some(claimed_reason)
            } else {
                false
            }
        } else {
            false
        };

        if is_valid_claim {
            let _ = SessionStateMachine::apply_command(&mut session, CMD_DRAW_ACCEPT, false);
            session
                .metadata
                .insert("terminal".into(), JsonValue::String("draw".into()));
            session.metadata.insert(
                "terminal_reason".into(),
                JsonValue::String(claimed_reason.into()),
            );
            session
                .metadata
                .insert("draw_offered".into(), JsonValue::Bool(false));
            session
                .metadata
                .insert("turn".into(), JsonValue::String("".into()));
            session.unread = 1;
            self.save_session(&session);

            return IncomingResult {
                session: Some(session_to_json(&session)),
                emit: Some(emit_event("draw_claim", session_id, APP_ID, sender_hash)),
                error: None,
            };
        }

        session
            .metadata
            .insert("draw_offered".into(), JsonValue::Bool(true));
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("draw_offer", session_id, APP_ID, sender_hash)),
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

        let _ = SessionStateMachine::apply_command(&mut session, CMD_DRAW_ACCEPT, false);
        session
            .metadata
            .insert("terminal".into(), JsonValue::String("draw".into()));
        session.metadata.insert(
            "terminal_reason".into(),
            JsonValue::String(R_AGREEMENT.into()),
        );
        session
            .metadata
            .insert("draw_offered".into(), JsonValue::Bool(false));
        session
            .metadata
            .insert("turn".into(), JsonValue::String("".into()));
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("draw_accept", session_id, APP_ID, sender_hash)),
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

        session
            .metadata
            .insert("draw_offered".into(), JsonValue::Bool(false));
        session.unread = 1;
        self.save_session(&session);

        IncomingResult {
            session: Some(session_to_json(&session)),
            emit: Some(emit_event("draw_decline", session_id, APP_ID, sender_hash)),
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
            fallback_text: "[LRGP Chess] Sent a challenge!".into(),
        }
    }

    fn handle_accept_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        let mut session = match self.get_session(session_id, identity_id) {
            Some(s) => s,
            None => {
                return OutgoingResult {
                    payload: HashMap::new(),
                    fallback_text: "[LRGP Chess] Challenge accepted".into(),
                };
            }
        };

        let _ = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false);

        // Responder coin-flips White; ships the White-player hash as `w`.
        let challenger_is_white = flip_responder_coin();
        let white_hash = if challenger_is_white {
            session.initiator.clone()
        } else {
            identity_id.to_string()
        };
        let my_color = if challenger_is_white { "b" } else { "w" };

        tracing::info!(
            target: "chess_trace",
            step = "accept_out.chose_white",
            sid = %short(session_id),
            my = %short(identity_id),
            challenger_is_white = %challenger_is_white,
            white_hash = %short(&white_hash),
            my_color = %my_color,
            "accept_out coin-flipped White assignment"
        );

        session
            .metadata
            .insert("fen".into(), JsonValue::String(STARTING_FEN.into()));
        session
            .metadata
            .insert("moves".into(), JsonValue::Array(vec![]));
        session
            .metadata
            .insert("first_turn".into(), JsonValue::String(white_hash.clone()));
        session
            .metadata
            .insert("turn".into(), JsonValue::String(white_hash.clone()));
        session
            .metadata
            .insert("my_color".into(), JsonValue::String(my_color.into()));

        let board = Board::default();
        Self::refresh_derived(&mut session, &board, &[]);
        self.save_session(&session);

        let mut payload = HashMap::new();
        payload.insert(
            KEY_WHITE.to_string(),
            rmpv::Value::String(white_hash.into()),
        );

        OutgoingResult {
            payload,
            fallback_text: "[LRGP Chess] Challenge accepted".into(),
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
                "[LRGP Chess] Challenge cancelled".into()
            } else {
                "[LRGP Chess] Challenge declined".into()
            },
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
                    fallback_text: "[LRGP Chess] Session not found".into(),
                };
            }
        };

        let uci = payload
            .get(KEY_MOVE)
            .and_then(|v| value_as_str(v))
            .unwrap_or("")
            .to_string();
        let current_turn = meta_str(&session.metadata, "turn");

        tracing::info!(
            target: "chess_trace",
            step = "move_out.before",
            sid = %short(session_id),
            my = %short(identity_id),
            turn = %short(&current_turn),
            uci = %uci,
            "handle_move_out entry"
        );

        // Empty payload sentinel → dashboard surfaces game_action_result{ok:false}.
        if current_turn != identity_id {
            tracing::warn!(
                target: "chess_trace",
                step = "move_out.wrong_turn",
                sid = %short(session_id),
                my = %short(identity_id),
                turn = %short(&current_turn),
                "move rejected: not this identity's turn"
            );
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP Chess] Not your turn".into(),
            };
        }

        let mut moves = meta_string_list(&session.metadata, "moves");
        let mut board = match replay_moves(&moves) {
            Ok(b) => b,
            Err(e) => {
                return OutgoingResult {
                    payload: HashMap::new(),
                    fallback_text: format!("[LRGP Chess] {e}"),
                };
            }
        };

        let mv: Move = match uci.parse() {
            Ok(m) => m,
            Err(_) => {
                return OutgoingResult {
                    payload: HashMap::new(),
                    fallback_text: format!("[LRGP Chess] Invalid UCI: {uci}"),
                };
            }
        };
        let mut legal: Vec<Move> = Vec::new();
        board.generate_moves(|mvs| {
            legal.extend(mvs);
            false
        });
        if !legal.contains(&mv) {
            return OutgoingResult {
                payload: HashMap::new(),
                fallback_text: format!("[LRGP Chess] Illegal move: {uci}"),
            };
        }

        board.play(mv);
        // SPEC: `n` is 0-based (0 = White's first move) — count before push.
        let ply = moves.len() as u64;
        moves.push(uci.clone());

        let (terminal, reason) = detect_auto_terminal(&board);
        let winner_hash = if terminal == "win" {
            identity_id.to_string()
        } else {
            String::new()
        };

        let mut enriched = HashMap::new();
        enriched.insert(
            KEY_MOVE.to_string(),
            rmpv::Value::String(uci.clone().into()),
        );
        enriched.insert(
            KEY_PLY.to_string(),
            rmpv::Value::Integer((ply as i64).into()),
        );
        enriched.insert(
            KEY_TERMINAL.to_string(),
            rmpv::Value::String(terminal.into()),
        );
        if !reason.is_empty() {
            enriched.insert(KEY_REASON.to_string(), rmpv::Value::String(reason.into()));
        }
        if terminal == "win" {
            enriched.insert(
                KEY_WINNER.to_string(),
                rmpv::Value::String(winner_hash.clone().into()),
            );
        }

        let next_turn = if terminal.is_empty() {
            session.contact_hash.clone()
        } else {
            String::new()
        };

        session
            .metadata
            .insert("moves".into(), string_list_value(&moves));
        session.metadata.insert(
            "move_count".into(),
            JsonValue::Number((moves.len() as i64).into()),
        );
        session
            .metadata
            .insert("turn".into(), JsonValue::String(next_turn));
        session
            .metadata
            .insert("last_move".into(), JsonValue::String(uci.clone()));
        session
            .metadata
            .insert("terminal".into(), JsonValue::String(terminal.to_string()));
        session.metadata.insert(
            "terminal_reason".into(),
            JsonValue::String(reason.to_string()),
        );
        session
            .metadata
            .insert("winner".into(), JsonValue::String(winner_hash));
        session
            .metadata
            .insert("draw_offered".into(), JsonValue::Bool(false));

        Self::refresh_derived(&mut session, &board, &moves);

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
                .insert("terminal".into(), JsonValue::String("win".into()));
            session
                .metadata
                .insert("terminal_reason".into(), JsonValue::String(R_RESIGN.into()));
            session.metadata.insert(
                "winner".into(),
                JsonValue::String(session.contact_hash.clone()),
            );
            session
                .metadata
                .insert("turn".into(), JsonValue::String("".into()));
            self.save_session(&session);
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP Chess] Resigned.".into(),
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
            .and_then(|v| value_as_str(v))
            .unwrap_or("")
            .to_string();

        let mut enriched = HashMap::new();
        if !reason.is_empty() {
            enriched.insert(
                KEY_REASON.to_string(),
                rmpv::Value::String(reason.clone().into()),
            );
        }

        if let Some(mut session) = self.get_session(session_id, identity_id) {
            // On claim, pre-terminate locally so UI reflects immediately.
            if reason == R_THREEFOLD || reason == R_FIFTY_MOVE {
                let moves = meta_string_list(&session.metadata, "moves");
                if let Ok(board) = replay_moves(&moves)
                    && claim_reason(&board, &moves) == Some(reason.as_str())
                {
                    let _ =
                        SessionStateMachine::apply_command(&mut session, CMD_DRAW_ACCEPT, false);
                    session
                        .metadata
                        .insert("terminal".into(), JsonValue::String("draw".into()));
                    session
                        .metadata
                        .insert("terminal_reason".into(), JsonValue::String(reason.clone()));
                    session
                        .metadata
                        .insert("turn".into(), JsonValue::String("".into()));
                }
            }
            self.save_session(&session);
        }

        let fallback = if reason == R_THREEFOLD {
            "[LRGP Chess] Claimed threefold repetition".into()
        } else if reason == R_FIFTY_MOVE {
            "[LRGP Chess] Claimed fifty-move rule".into()
        } else {
            "[LRGP Chess] Offered a draw".into()
        };

        OutgoingResult {
            payload: enriched,
            fallback_text: fallback,
        }
    }

    fn handle_draw_accept_out(&self, session_id: &str, identity_id: &str) -> OutgoingResult {
        if let Some(mut session) = self.get_session(session_id, identity_id) {
            let _ = SessionStateMachine::apply_command(&mut session, CMD_DRAW_ACCEPT, false);
            session
                .metadata
                .insert("terminal".into(), JsonValue::String("draw".into()));
            session.metadata.insert(
                "terminal_reason".into(),
                JsonValue::String(R_AGREEMENT.into()),
            );
            session
                .metadata
                .insert("draw_offered".into(), JsonValue::Bool(false));
            session
                .metadata
                .insert("turn".into(), JsonValue::String("".into()));
            self.save_session(&session);
        }
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: "[LRGP Chess] Draw accepted".into(),
        }
    }

    fn validate_move(
        &self,
        session: &Session,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
    ) -> (bool, Option<String>) {
        let meta = &session.metadata;

        if session.status != STATUS_ACTIVE {
            return (
                false,
                Some(format!("Session is not active (status={})", session.status)),
            );
        }

        // Empty turn on an active session is invalid state — fail closed
        // (canonical per SPEC; matches lrgp-py).
        let turn = meta_str(meta, "turn");
        if turn.is_empty() {
            return (false, Some("Turn is required before moves".into()));
        }
        if turn != sender_hash {
            return (false, Some("Not your turn".into()));
        }

        let uci = match payload.get(KEY_MOVE).and_then(|v| value_as_str(v)) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return (false, Some("Missing UCI move".into())),
        };

        let ply = payload.get(KEY_PLY).and_then(value_as_u64).unwrap_or(0);
        let claimed_terminal = payload
            .get(KEY_TERMINAL)
            .and_then(|v| value_as_str(v))
            .unwrap_or("")
            .to_string();
        let claimed_reason = payload
            .get(KEY_REASON)
            .and_then(|v| value_as_str(v))
            .unwrap_or("")
            .to_string();
        let claimed_winner = payload
            .get(KEY_WINNER)
            .and_then(|v| value_as_str(v))
            .unwrap_or("")
            .to_string();

        // SPEC: `n` is 0-based, so the next move's ply equals the count of
        // moves already applied.
        let expected_ply = meta_i64(meta, "move_count") as u64;
        if ply != expected_ply {
            return (
                false,
                Some(format!("Ply mismatch: expected {expected_ply}, got {ply}")),
            );
        }

        let mut moves = meta_string_list(meta, "moves");
        let mut board = match replay_moves(&moves) {
            Ok(b) => b,
            Err(e) => return (false, Some(format!("Local replay failed: {e}"))),
        };

        let mv: Move = match uci.parse() {
            Ok(m) => m,
            Err(_) => return (false, Some(format!("Invalid UCI: {uci}"))),
        };

        let mut legal: Vec<Move> = Vec::new();
        board.generate_moves(|mvs| {
            legal.extend(mvs);
            false
        });
        if !legal.contains(&mv) {
            return (
                false,
                Some(format!("Move {uci} is not legal from current position")),
            );
        }

        board.play(mv);
        moves.push(uci.clone());
        let (terminal, reason) = detect_auto_terminal(&board);

        if terminal != claimed_terminal {
            return (
                false,
                Some(format!(
                    "Terminal mismatch: computed='{terminal}' claimed='{claimed_terminal}'"
                )),
            );
        }
        if !terminal.is_empty() && reason != claimed_reason {
            return (
                false,
                Some(format!(
                    "Reason mismatch: computed='{reason}' claimed='{claimed_reason}'"
                )),
            );
        }
        if terminal == "win" && claimed_winner != sender_hash {
            return (
                false,
                Some(format!(
                    "Winner mismatch: computed='{sender_hash}' claimed='{claimed_winner}'"
                )),
            );
        }

        (true, None)
    }

    fn render_fallback_inner(
        &self,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
    ) -> String {
        match command {
            CMD_CHALLENGE => "[LRGP Chess] Sent a challenge!".into(),
            CMD_ACCEPT => "[LRGP Chess] Challenge accepted".into(),
            CMD_DECLINE => "[LRGP Chess] Challenge declined".into(),
            CMD_MOVE => {
                let terminal = payload
                    .get(KEY_TERMINAL)
                    .and_then(|v| value_as_str(v))
                    .unwrap_or("");
                let reason = payload
                    .get(KEY_REASON)
                    .and_then(|v| value_as_str(v))
                    .unwrap_or("");
                let uci = payload
                    .get(KEY_MOVE)
                    .and_then(|v| value_as_str(v))
                    .unwrap_or("");
                if terminal == "win" {
                    let detail = match reason {
                        R_CHECKMATE => "checkmate",
                        R_RESIGN => "resignation",
                        _ => "win",
                    };
                    format!("[LRGP Chess] {uci}# ({detail})")
                } else if terminal == "draw" {
                    let detail = match reason {
                        R_STALEMATE => "stalemate",
                        R_INSUFFICIENT => "insufficient material",
                        R_THREEFOLD => "threefold repetition",
                        R_FIFTY_MOVE => "fifty-move rule",
                        R_AGREEMENT => "agreement",
                        _ => "draw",
                    };
                    format!("[LRGP Chess] Draw ({detail})")
                } else {
                    format!("[LRGP Chess] {uci}")
                }
            }
            CMD_RESIGN => "[LRGP Chess] Resigned.".into(),
            CMD_DRAW_OFFER => {
                let reason = payload
                    .get(KEY_REASON)
                    .and_then(|v| value_as_str(v))
                    .unwrap_or("");
                match reason {
                    R_THREEFOLD => "[LRGP Chess] Claimed threefold repetition".into(),
                    R_FIFTY_MOVE => "[LRGP Chess] Claimed fifty-move rule".into(),
                    _ => "[LRGP Chess] Offered a draw".into(),
                }
            }
            CMD_DRAW_ACCEPT => "[LRGP Chess] Draw accepted".into(),
            CMD_DRAW_DECLINE => "[LRGP Chess] Draw declined".into(),
            CMD_ERROR => {
                let msg = payload
                    .get("msg")
                    .and_then(|v| value_as_str(v))
                    .unwrap_or("Unknown");
                format!("[LRGP Chess] Error: {msg}")
            }
            other => format!("[LRGP Chess] {other}"),
        }
    }
}

impl Default for ChessApp {
    fn default() -> Self {
        Self::new()
    }
}

impl GameApp for ChessApp {
    fn app_id(&self) -> &str {
        APP_ID
    }

    fn version(&self) -> u32 {
        APP_VERSION
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

        let mut ttl = HashMap::new();
        ttl.insert(STATUS_PENDING.into(), TTL_PENDING);
        ttl.insert(STATUS_ACTIVE.into(), TTL_ACTIVE);

        AppManifest {
            app_id: APP_ID.into(),
            version: APP_VERSION,
            display_name: "Chess".into(),
            icon: "chess".into(),
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
        match command {
            CMD_CHALLENGE => {
                self.handle_challenge_in(session_id, payload, sender_hash, identity_id)
            }
            CMD_ACCEPT => self.handle_accept_in(session_id, payload, sender_hash, identity_id),
            CMD_DECLINE => self.handle_decline_in(session_id, sender_hash, identity_id),
            CMD_MOVE => self.handle_move_in(session_id, payload, sender_hash, identity_id),
            CMD_RESIGN => self.handle_resign_in(session_id, sender_hash, identity_id),
            CMD_DRAW_OFFER => {
                self.handle_draw_offer_in(session_id, payload, sender_hash, identity_id)
            }
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
            CMD_DRAW_OFFER => self.handle_draw_offer_out(session_id, payload, identity_id),
            CMD_DRAW_ACCEPT => self.handle_draw_accept_out(session_id, identity_id),
            CMD_DRAW_DECLINE => OutgoingResult {
                payload: HashMap::new(),
                fallback_text: "[LRGP Chess] Declined draw offer".into(),
            },
            _ => OutgoingResult {
                payload: payload.clone(),
                fallback_text: format!("[LRGP Chess] {command}"),
            },
        }
    }

    fn validate_action(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
    ) -> (bool, Option<String>) {
        let session = match self.get_session(session_id, "") {
            Some(s) => s,
            None => {
                return if command == CMD_CHALLENGE {
                    (true, None)
                } else {
                    (false, Some("Session not found".into()))
                };
            }
        };

        let ttl = {
            let mut m = HashMap::new();
            m.insert(STATUS_PENDING.to_string(), TTL_PENDING);
            m.insert(STATUS_ACTIVE.to_string(), TTL_ACTIVE);
            m
        };
        let mut session = session;
        if SessionStateMachine::check_expiry(&mut session, Some(&ttl), None) {
            self.save_session(&session);
            return (false, Some("Session expired".into()));
        }

        if command == CMD_MOVE {
            return self.validate_move(&session, payload, sender_hash);
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
mod smoke_tests {
    use cozy_chess::{Board, Color, GameStatus, Move, Piece};
    use std::str::FromStr;

    /// Pin down the cozy-chess API surface.
    #[test]
    fn cozy_chess_api_smoke() {
        let mut board = Board::default();
        assert_eq!(board.side_to_move(), Color::White);

        let fen = format!("{}", board);
        assert!(fen.starts_with("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR"));
        let reloaded = Board::from_str(&fen).expect("FEN parse");
        assert_eq!(format!("{}", reloaded), fen);

        let e2e4: Move = "e2e4".parse().expect("UCI parse");
        let mut legal: Vec<Move> = Vec::new();
        board.generate_moves(|mvs| {
            legal.extend(mvs);
            false
        });
        assert_eq!(legal.len(), 20);
        assert!(legal.contains(&e2e4));

        board.play(e2e4);
        assert_eq!(board.side_to_move(), Color::Black);
        assert_eq!(board.status(), GameStatus::Ongoing);

        let h1 = board.hash();
        let h2 = Board::from_str(&format!("{}", board)).unwrap().hash();
        assert_eq!(h1, h2);

        assert!(board.checkers().is_empty());

        let white_knights = board.pieces(Piece::Knight) & board.colors(Color::White);
        assert_eq!(white_knights.len(), 2);

        // Fool's-mate → checkmate.
        let mut m = Board::default();
        for uci in ["f2f3", "e7e5", "g2g4", "d8h4"] {
            let mv: Move = uci.parse().unwrap();
            m.play(mv);
        }
        assert_eq!(m.status(), GameStatus::Won);
        assert_eq!(m.side_to_move(), Color::White); // White is the loser.
    }

    #[test]
    fn promotion_uci_roundtrip() {
        let board = Board::from_str("k7/4P3/8/8/8/8/8/4K3 w - - 0 1").expect("FEN");
        let mut legal: Vec<Move> = Vec::new();
        board.generate_moves(|mvs| {
            legal.extend(mvs);
            false
        });
        let uci_set: Vec<String> = legal.iter().map(|m| format!("{}", m)).collect();
        assert!(uci_set.iter().any(|s| s == "e7e8q"));
        assert!(uci_set.iter().any(|s| s == "e7e8r"));
        assert!(uci_set.iter().any(|s| s == "e7e8b"));
        assert!(uci_set.iter().any(|s| s == "e7e8n"));

        let promo: Move = "e7e8q".parse().unwrap();
        assert_eq!(format!("{}", promo), "e7e8q");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::sync::MutexGuard;

    // Serializes access to the FORCED_COIN static across tests.
    static COIN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct _CoinReset(#[allow(dead_code)] MutexGuard<'static, ()>);
    impl Drop for _CoinReset {
        fn drop(&mut self) {
            force_coin(None);
        }
    }

    fn pin_coin(challenger_is_white: bool) -> _CoinReset {
        let guard = COIN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        force_coin(Some(challenger_is_white));
        _CoinReset(guard)
    }

    fn setup_active(app: &ChessApp, white: &str, black: &str) {
        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), white);
        {
            let mut s = app.sessions.lock().unwrap();
            s.get_mut(&("g1".into(), white.into()))
                .unwrap()
                .contact_hash = black.to_string();
        }
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), white, black);
        let accept = app.handle_outgoing("g1", CMD_ACCEPT, &HashMap::new(), black);
        app.handle_incoming("g1", CMD_ACCEPT, &accept.payload, black, white);
    }

    fn play_move(app: &ChessApp, uci: &str, mover: &str, opp: &str) -> OutgoingResult {
        let mut p = HashMap::new();
        p.insert(KEY_MOVE.into(), rmpv::Value::String(uci.into()));
        let out = app.handle_outgoing("g1", CMD_MOVE, &p, mover);
        app.handle_incoming("g1", CMD_MOVE, &out.payload, mover, opp);
        out
    }

    #[test]
    fn test_challenge_flow() {
        let app = ChessApp::new();
        let out = app.handle_outgoing("s1", CMD_CHALLENGE, &HashMap::new(), "alice");
        assert_eq!(out.fallback_text, "[LRGP Chess] Sent a challenge!");

        let sess = app.get_session("s1", "alice").unwrap();
        assert_eq!(sess.status, STATUS_PENDING);
        assert_eq!(meta_str(&sess.metadata, "fen"), STARTING_FEN);

        app.handle_incoming("s1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");
        let sess = app.get_session("s1", "bob").unwrap();
        assert_eq!(sess.status, STATUS_PENDING);
    }

    #[test]
    fn test_accept_flow_challenger_white() {
        let _coin = pin_coin(true); // challenger = white
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let sess_a = app.get_session("g1", "alice").unwrap();
        let sess_b = app.get_session("g1", "bob").unwrap();
        assert_eq!(sess_a.status, STATUS_ACTIVE);
        assert_eq!(sess_b.status, STATUS_ACTIVE);
        assert_eq!(meta_str(&sess_a.metadata, "my_color"), "w");
        assert_eq!(meta_str(&sess_b.metadata, "my_color"), "b");
        assert_eq!(meta_str(&sess_a.metadata, "first_turn"), "alice");
        assert_eq!(meta_str(&sess_b.metadata, "first_turn"), "alice");
        assert_eq!(meta_str(&sess_a.metadata, "turn"), "alice");
        assert_eq!(meta_string_list(&sess_a.metadata, "legal_moves").len(), 20);
    }

    #[test]
    fn test_accept_flow_responder_white() {
        let _coin = pin_coin(false); // responder = white
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let sess_a = app.get_session("g1", "alice").unwrap();
        let sess_b = app.get_session("g1", "bob").unwrap();
        assert_eq!(meta_str(&sess_a.metadata, "my_color"), "b");
        assert_eq!(meta_str(&sess_b.metadata, "my_color"), "w");
        assert_eq!(meta_str(&sess_a.metadata, "first_turn"), "bob");
    }

    #[test]
    fn test_decline_flow() {
        let app = ChessApp::new();
        app.handle_outgoing("g1", CMD_CHALLENGE, &HashMap::new(), "alice");
        app.handle_incoming("g1", CMD_CHALLENGE, &HashMap::new(), "alice", "bob");

        let out = app.handle_outgoing("g1", CMD_DECLINE, &HashMap::new(), "bob");
        assert_eq!(out.fallback_text, "[LRGP Chess] Challenge declined");
        let sess = app.get_session("g1", "bob").unwrap();
        assert_eq!(sess.status, STATUS_DECLINED);
    }

    #[test]
    fn test_fools_mate() {
        // Shortest possible checkmate: 1.f3 e5 2.g4 Qh4#
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        play_move(&app, "f2f3", "alice", "bob");
        play_move(&app, "e7e5", "bob", "alice");
        play_move(&app, "g2g4", "alice", "bob");
        let mate = play_move(&app, "d8h4", "bob", "alice");

        assert_eq!(
            value_as_str(mate.payload.get(KEY_TERMINAL).unwrap()).unwrap(),
            "win"
        );
        assert_eq!(
            value_as_str(mate.payload.get(KEY_REASON).unwrap()).unwrap(),
            R_CHECKMATE
        );
        assert_eq!(
            value_as_str(mate.payload.get(KEY_WINNER).unwrap()).unwrap(),
            "bob"
        );

        let sess = app.get_session("g1", "alice").unwrap();
        assert_eq!(sess.status, STATUS_COMPLETED);
        assert_eq!(meta_str(&sess.metadata, "terminal"), "win");
        assert_eq!(meta_str(&sess.metadata, "winner"), "bob");
    }

    #[test]
    fn test_scholars_mate() {
        // 1.e4 e5 2.Bc4 Nc6 3.Qh5 Nf6?? 4.Qxf7#
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        play_move(&app, "e2e4", "alice", "bob");
        play_move(&app, "e7e5", "bob", "alice");
        play_move(&app, "f1c4", "alice", "bob");
        play_move(&app, "b8c6", "bob", "alice");
        play_move(&app, "d1h5", "alice", "bob");
        play_move(&app, "g8f6", "bob", "alice");
        let mate = play_move(&app, "h5f7", "alice", "bob");

        assert_eq!(
            value_as_str(mate.payload.get(KEY_TERMINAL).unwrap()).unwrap(),
            "win"
        );
        assert_eq!(
            value_as_str(mate.payload.get(KEY_REASON).unwrap()).unwrap(),
            R_CHECKMATE
        );
        let sess = app.get_session("g1", "alice").unwrap();
        assert_eq!(meta_str(&sess.metadata, "winner"), "alice");
    }

    #[test]
    fn test_stalemate() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        // Stalemate FEN: Black king h8, White king f6, White queen g6, Black to move.
        let stalemate_fen = "7k/8/5KQ1/8/8/8/8/8 b - - 0 1";
        let board = Board::from_str(stalemate_fen).unwrap();
        let (terminal, reason) = detect_auto_terminal(&board);
        assert_eq!(terminal, "draw");
        assert_eq!(reason, R_STALEMATE);
        let _ = &app;
    }

    #[test]
    fn test_insufficient_material_k_vs_k() {
        let fen = "4k3/8/8/8/8/8/8/4K3 w - - 0 1";
        let board = Board::from_str(fen).unwrap();
        assert!(insufficient_material(&board));
        let (t, r) = detect_auto_terminal(&board);
        assert_eq!(t, "draw");
        assert_eq!(r, R_INSUFFICIENT);
    }

    #[test]
    fn test_insufficient_material_kb_vs_k() {
        let fen = "4k3/8/8/8/8/8/8/3BK3 w - - 0 1";
        let board = Board::from_str(fen).unwrap();
        assert!(insufficient_material(&board));
    }

    #[test]
    fn test_insufficient_material_kn_vs_k() {
        let fen = "4k3/8/8/8/8/8/8/3NK3 w - - 0 1";
        let board = Board::from_str(fen).unwrap();
        assert!(insufficient_material(&board));
    }

    #[test]
    fn test_insufficient_material_kb_vs_kb_same_color() {
        // Bishops on c1 (dark) and f8 (dark) → same color → insufficient.
        let fen = "5b1k/8/8/8/8/8/8/2B1K3 w - - 0 1";
        let board = Board::from_str(fen).unwrap();
        assert!(insufficient_material(&board));
    }

    #[test]
    fn test_sufficient_material_kb_vs_kb_diff_color() {
        // Bishops on c1 (dark) and c8 (light) → different colors → sufficient.
        let fen = "2b4k/8/8/8/8/8/8/2B1K3 w - - 0 1";
        let board = Board::from_str(fen).unwrap();
        assert!(!insufficient_material(&board));
    }

    #[test]
    fn test_sufficient_material_has_pawn() {
        let fen = "4k3/p7/8/8/8/8/8/4K3 w - - 0 1";
        let board = Board::from_str(fen).unwrap();
        assert!(!insufficient_material(&board));
    }

    #[test]
    fn test_fifty_move_claim_eligibility() {
        let fen = "4k3/8/8/8/8/8/8/R3K3 w - - 100 60";
        let board = Board::from_str(fen).unwrap();
        assert_eq!(board.halfmove_clock(), 100);
        assert_eq!(claim_reason(&board, &[]), Some(R_FIFTY_MOVE));
    }

    #[test]
    fn test_threefold_claim_eligibility() {
        // Knight shuffle: 8 plies → start position appears 3 times.
        let moves: Vec<String> = [
            "b1c3", "b8c6", "c3b1", "c6b8", "b1c3", "b8c6", "c3b1", "c6b8",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let board = replay_moves(&moves).unwrap();
        assert_eq!(claim_reason(&board, &moves), Some(R_THREEFOLD));
    }

    #[test]
    fn test_resign() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let out = app.handle_outgoing("g1", CMD_RESIGN, &HashMap::new(), "alice");
        assert_eq!(out.fallback_text, "[LRGP Chess] Resigned.");
        let sess = app.get_session("g1", "alice").unwrap();
        assert_eq!(sess.status, STATUS_COMPLETED);
        assert_eq!(meta_str(&sess.metadata, "terminal"), "win");
        assert_eq!(meta_str(&sess.metadata, "terminal_reason"), R_RESIGN);
        assert_eq!(meta_str(&sess.metadata, "winner"), "bob");
    }

    #[test]
    fn test_draw_agreement() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        app.handle_incoming("g1", CMD_DRAW_OFFER, &HashMap::new(), "bob", "alice");
        let sess = app.get_session("g1", "alice").unwrap();
        assert!(
            sess.metadata
                .get("draw_offered")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        );

        let out = app.handle_outgoing("g1", CMD_DRAW_ACCEPT, &HashMap::new(), "alice");
        assert_eq!(out.fallback_text, "[LRGP Chess] Draw accepted");
        let sess = app.get_session("g1", "alice").unwrap();
        assert_eq!(sess.status, STATUS_COMPLETED);
        assert_eq!(meta_str(&sess.metadata, "terminal"), "draw");
        assert_eq!(meta_str(&sess.metadata, "terminal_reason"), R_AGREEMENT);
    }

    #[test]
    fn test_draw_claim_threefold_auto_accepts() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        play_move(&app, "b1c3", "alice", "bob");
        play_move(&app, "b8c6", "bob", "alice");
        play_move(&app, "c3b1", "alice", "bob");
        play_move(&app, "c6b8", "bob", "alice");
        play_move(&app, "b1c3", "alice", "bob");
        play_move(&app, "b8c6", "bob", "alice");
        play_move(&app, "c3b1", "alice", "bob");
        play_move(&app, "c6b8", "bob", "alice");

        let sess = app.get_session("g1", "alice").unwrap();
        assert_eq!(meta_str(&sess.metadata, "draw_offer_reason"), R_THREEFOLD);

        let mut p = HashMap::new();
        p.insert(KEY_REASON.into(), rmpv::Value::String(R_THREEFOLD.into()));
        app.handle_incoming("g1", CMD_DRAW_OFFER, &p, "alice", "bob");

        let sess_b = app.get_session("g1", "bob").unwrap();
        assert_eq!(sess_b.status, STATUS_COMPLETED);
        assert_eq!(meta_str(&sess_b.metadata, "terminal"), "draw");
        assert_eq!(meta_str(&sess_b.metadata, "terminal_reason"), R_THREEFOLD);
    }

    #[test]
    fn test_invalid_claim_falls_back_to_offer() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let mut p = HashMap::new();
        p.insert(KEY_REASON.into(), rmpv::Value::String(R_THREEFOLD.into()));
        app.handle_incoming("g1", CMD_DRAW_OFFER, &p, "bob", "alice");

        let sess = app.get_session("g1", "alice").unwrap();
        assert!(
            sess.metadata
                .get("draw_offered")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        );
        assert_ne!(meta_str(&sess.metadata, "terminal"), "draw");
    }

    #[test]
    fn test_illegal_move_rejected() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let mut p = HashMap::new();
        p.insert(KEY_MOVE.into(), rmpv::Value::String("e2e5".into()));
        let out = app.handle_outgoing("g1", CMD_MOVE, &p, "alice");
        assert!(out.fallback_text.contains("Illegal"));
        assert!(out.payload.is_empty());
    }

    #[test]
    fn test_out_of_turn_rejected() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let mut p = HashMap::new();
        p.insert(KEY_MOVE.into(), rmpv::Value::String("e7e5".into()));
        let out = app.handle_outgoing("g1", CMD_MOVE, &p, "bob");
        assert_eq!(out.fallback_text, "[LRGP Chess] Not your turn");
        assert!(out.payload.is_empty());
    }

    #[test]
    fn test_unknown_session_move_rejected() {
        let app = ChessApp::new();
        let mut p = HashMap::new();
        p.insert(KEY_MOVE.into(), rmpv::Value::String("e2e4".into()));

        let out = app.handle_outgoing("missing", CMD_MOVE, &p, "alice");

        assert_eq!(out.fallback_text, "[LRGP Chess] Session not found");
        assert!(out.payload.is_empty());
    }

    #[test]
    fn test_envelope_size_move() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let mut p = HashMap::new();
        p.insert(KEY_MOVE.into(), rmpv::Value::String("e2e4".into()));
        let out = app.handle_outgoing("g1", CMD_MOVE, &p, "alice");

        use crate::envelope::{pack_envelope, validate_envelope_size};
        let env = pack_envelope(APP_ID, APP_VERSION, CMD_MOVE, "g1", Some(out.payload), None);
        let size = validate_envelope_size(&env).expect("envelope size OK");
        assert!(size <= 150, "move envelope {} bytes (budget ≤150)", size);
    }

    #[test]
    fn test_envelope_size_promotion() {
        use crate::envelope::{pack_envelope, validate_envelope_size};
        let mut p = HashMap::new();
        p.insert(KEY_MOVE.to_string(), rmpv::Value::String("e7e8q".into()));
        p.insert(KEY_PLY.to_string(), rmpv::Value::Integer(64.into()));
        p.insert(KEY_TERMINAL.to_string(), rmpv::Value::String("win".into()));
        p.insert(
            KEY_REASON.to_string(),
            rmpv::Value::String(R_CHECKMATE.into()),
        );
        p.insert(
            KEY_WINNER.to_string(),
            rmpv::Value::String("0123456789abcdef0123456789abcdef".into()),
        );

        let env = pack_envelope(
            APP_ID,
            APP_VERSION,
            CMD_MOVE,
            "abcdef0123456789",
            Some(p),
            None,
        );
        let size = validate_envelope_size(&env).expect("envelope size OK");
        assert!(
            size <= 150,
            "terminal-promotion envelope {} bytes (budget ≤150)",
            size
        );
    }

    /// T1-5: SPEC.md says `n` is 0-based (0 = White's first move). rs
    /// previously emitted 1-based and required it inbound, breaking move-1
    /// interop with lrgp-py and contradicting the shared chess_move.bin
    /// vector (which this encode-side check pins).
    #[test]
    fn test_first_move_emits_zero_based_ply() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let out = play_move(&app, "e2e4", "alice", "bob");
        assert_eq!(value_as_u64(out.payload.get(KEY_PLY).unwrap()).unwrap(), 0);

        let sess_a = app.get_session("g1", "alice").unwrap();
        let sess_b = app.get_session("g1", "bob").unwrap();
        assert_eq!(meta_i64(&sess_a.metadata, "move_count"), 1);
        assert_eq!(meta_i64(&sess_b.metadata, "move_count"), 1);

        let out = play_move(&app, "e7e5", "bob", "alice");
        assert_eq!(value_as_u64(out.payload.get(KEY_PLY).unwrap()).unwrap(), 1);
    }

    /// T1-13: an active session with an empty `turn` is invalid state; a
    /// move against it must fail closed (canonical per SPEC, matches lrgp-py).
    #[test]
    fn test_validate_move_rejects_empty_turn() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let mut session = app.get_session("g1", "bob").unwrap();
        session
            .metadata
            .insert("turn".into(), JsonValue::String("".into()));

        let mut p = HashMap::new();
        p.insert(KEY_MOVE.to_string(), rmpv::Value::String("e2e4".into()));
        p.insert(KEY_PLY.to_string(), rmpv::Value::Integer(0.into()));
        let (valid, err) = app.validate_move(&session, &p, "alice");
        assert!(!valid);
        assert!(err.unwrap().contains("Turn is required"));
    }

    /// T1-5: a 1-based first move (the old rs emission) must be rejected,
    /// a 0-based one accepted.
    #[test]
    fn test_first_move_ply_base_validation() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let session = app.get_session("g1", "bob").unwrap();
        let mut p = HashMap::new();
        p.insert(KEY_MOVE.to_string(), rmpv::Value::String("e2e4".into()));
        p.insert(KEY_PLY.to_string(), rmpv::Value::Integer(1.into()));
        let (valid, err) = app.validate_move(&session, &p, "alice");
        assert!(!valid);
        assert!(err.unwrap().contains("Ply mismatch"));

        p.insert(KEY_PLY.to_string(), rmpv::Value::Integer(0.into()));
        let (valid, err) = app.validate_move(&session, &p, "alice");
        assert!(valid, "0-based first move must validate: {err:?}");
    }

    #[test]
    fn test_snapshot_rollback() {
        let _coin = pin_coin(true);
        let app = ChessApp::new();
        setup_active(&app, "alice", "bob");

        let snap = app.snapshot_session("g1", "alice");
        assert!(snap.is_some());

        play_move(&app, "e2e4", "alice", "bob");
        let after = app.get_session("g1", "alice").unwrap();
        assert_eq!(meta_str(&after.metadata, "last_move"), "e2e4");

        app.rollback_session("g1", "alice", snap);
        let restored = app.get_session("g1", "alice").unwrap();
        assert_eq!(meta_str(&restored.metadata, "last_move"), "");
        assert_eq!(meta_i64(&restored.metadata, "move_count"), 0);
    }

    #[test]
    fn test_rollback_new_session_removed() {
        let app = ChessApp::new();
        let snap = app.snapshot_session("new_sid", "alice"); // None
        app.handle_outgoing("new_sid", CMD_CHALLENGE, &HashMap::new(), "alice");
        assert!(app.get_session("new_sid", "alice").is_some());
        app.rollback_session("new_sid", "alice", snap);
        assert!(app.get_session("new_sid", "alice").is_none());
    }

    #[test]
    fn test_render_fallback() {
        let app = ChessApp::new();
        assert_eq!(
            app.render_fallback(CMD_CHALLENGE, &HashMap::new()),
            "[LRGP Chess] Sent a challenge!"
        );

        let mut p = HashMap::new();
        p.insert(KEY_MOVE.into(), rmpv::Value::String("e2e4".into()));
        p.insert(KEY_TERMINAL.into(), rmpv::Value::String("".into()));
        assert_eq!(app.render_fallback(CMD_MOVE, &p), "[LRGP Chess] e2e4");

        let mut p = HashMap::new();
        p.insert(KEY_MOVE.into(), rmpv::Value::String("d8h4".into()));
        p.insert(KEY_TERMINAL.into(), rmpv::Value::String("win".into()));
        p.insert(KEY_REASON.into(), rmpv::Value::String(R_CHECKMATE.into()));
        assert_eq!(
            app.render_fallback(CMD_MOVE, &p),
            "[LRGP Chess] d8h4# (checkmate)"
        );
    }

    #[test]
    fn test_validate_action_no_session() {
        let app = ChessApp::new();
        let (valid, _) = app.validate_action("nope", CMD_CHALLENGE, &HashMap::new(), "x");
        assert!(valid);
        let (valid, _) = app.validate_action("nope", CMD_MOVE, &HashMap::new(), "x");
        assert!(!valid);
    }
}
