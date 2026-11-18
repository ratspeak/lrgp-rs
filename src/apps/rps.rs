//! Rock Paper Scissors, first to three rounds. Choices are simultaneous over
//! a slow link by commitment: the leader sends a SHA-256 hash of its choice
//! and a random salt, the follower answers in the open, and the leader then
//! reveals the choice and salt together with the next round's commitment.
//! SPEC.md Appendix I defines the protocol.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use super::placement::{
    R_AGREEMENT, R_RESIGN, error_result, gen_session_id, meta_str, other_player, rmpv_to_json,
    session_to_json,
};
use crate::app_base::{AppManifest, GameApp, IncomingResult, OutgoingResult};
use crate::constants::*;
use crate::envelope::{has_exact_keys, value_as_str, value_as_u64};
use crate::errors::LrgpError;
use crate::session::{Session, SessionStateMachine};

pub const APP_ID: &str = "rps";
pub const APP_VERSION: u32 = 1;
/// Round wins that take the match.
pub const WINS_NEEDED: usize = 3;
pub const CHOICES: [&str; 3] = ["p", "r", "s"];

const DISPLAY_NAME: &str = "Rock Paper Scissors";
const R_ROUNDS: &str = "rnd";

// Wire payload keys.
const KEY_CHOICE: &str = "c"; // "r" | "p" | "s"
const KEY_SALT: &str = "k"; // revealed salt
const KEY_COMMIT: &str = "h"; // commitment to the leader's next choice
const KEY_PLY: &str = "n"; // ply counter, 0-based
const KEY_TERMINAL: &str = "x"; // "" | "win"
const KEY_WINNER: &str = "w"; // winner identity hash
const KEY_BLACK: &str = "b"; // leader (ACCEPT payload)

pub const PHASE_COMMIT: &str = "commit";
pub const PHASE_CHOOSE: &str = "choose";
pub const PHASE_REVEAL: &str = "reveal";

#[cfg(any(test, feature = "test-helpers"))]
static FORCED_COIN: Mutex<Option<bool>> = Mutex::new(None);

/// Pin the responder coin-flip for tests (`true` = challenger leads).
/// Process-global; callers must serialize.
#[cfg(any(test, feature = "test-helpers"))]
pub fn force_coin(challenger_leads: Option<bool>) {
    if let Ok(mut guard) = FORCED_COIN.lock() {
        *guard = challenger_leads;
    }
}

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

/// Lowercase hex SHA-256 of `"<choice>:<salt>"`.
pub fn commitment(choice: &str, salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(choice.as_bytes());
    hasher.update(b":");
    hasher.update(salt.as_bytes());
    hex::encode(hasher.finalize())
}

fn is_lower_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_choice(text: &str) -> bool {
    CHOICES.contains(&text)
}

fn random_salt() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// `Some(true)` if `a` beats `b`, `None` for a tie.
fn beats(a: &str, b: &str) -> Option<bool> {
    if a == b {
        return None;
    }
    Some(matches!((a, b), ("r", "s") | ("s", "p") | ("p", "r")))
}

/// One ply as stored: the wire keys other than `n`, `x`, and `w`.
type Ply = HashMap<String, String>;

fn stored_plies(meta: &HashMap<String, JsonValue>) -> Result<Vec<Ply>, String> {
    let Some(items) = meta.get("plies").and_then(JsonValue::as_array) else {
        return Err("plies must be an array".into());
    };
    items
        .iter()
        .map(|item| {
            item.as_object()
                .ok_or("a ply must be an object")?
                .iter()
                .map(|(k, v)| {
                    Ok((
                        k.clone(),
                        v.as_str().ok_or("ply values must be strings")?.to_string(),
                    ))
                })
                .collect()
        })
        .collect()
}

fn plies_value(plies: &[Ply]) -> JsonValue {
    JsonValue::Array(
        plies
            .iter()
            .map(|ply| {
                JsonValue::Object(
                    ply.iter()
                        .map(|(k, v)| (k.clone(), JsonValue::String(v.clone())))
                        .collect(),
                )
            })
            .collect(),
    )
}

fn ply_has(ply: &Ply, keys: &[&str]) -> bool {
    ply.len() == keys.len() && keys.iter().all(|key| ply.contains_key(*key))
}

/// Everything the stored plies imply.
struct Replayed {
    turn: String,
    phase: &'static str,
    /// Rounds played: the leader's choice, then the follower's.
    rounds: Vec<(String, String)>,
    /// Round wins: the leader's, then the follower's.
    score: [usize; 2],
    winner: Option<String>,
    /// The follower's choice in the round awaiting the leader's reveal.
    pending: Option<String>,
}

fn replay(session: &Session) -> Result<Replayed, String> {
    let meta = &session.metadata;
    let leader = meta_str(meta, "first_turn");
    let follower = other_player(session, &leader).ok_or("first_turn is not a bound participant")?;
    let plies = stored_plies(meta)?;
    let mut commit: Option<String> = None;
    let mut pending: Option<String> = None;
    let mut rounds = Vec::new();
    let mut score = [0, 0];
    let mut winner = None;

    for (index, ply) in plies.iter().enumerate() {
        if winner.is_some() {
            return Err("a move follows the end of the match".into());
        }
        let get = |key: &str| ply.get(key).map(String::as_str).unwrap_or("");
        if index == 0 {
            if !ply_has(ply, &[KEY_COMMIT]) || !is_lower_hex(get(KEY_COMMIT), 64) {
                return Err("the first ply must be the leader's commitment".into());
            }
            commit = Some(get(KEY_COMMIT).to_string());
        } else if index % 2 == 1 {
            if !ply_has(ply, &[KEY_CHOICE]) || !is_choice(get(KEY_CHOICE)) {
                return Err("the follower must send exactly one choice".into());
            }
            pending = Some(get(KEY_CHOICE).to_string());
        } else {
            let ends = ply_has(ply, &[KEY_CHOICE, KEY_SALT]);
            if !ends && !ply_has(ply, &[KEY_CHOICE, KEY_SALT, KEY_COMMIT]) {
                return Err("a reveal must carry c and k, and h unless the match ends".into());
            }
            let (choice, salt) = (get(KEY_CHOICE), get(KEY_SALT));
            if !is_choice(choice) || !is_lower_hex(salt, 32) {
                return Err("a reveal carries an invalid choice or salt".into());
            }
            if commit.as_deref() != Some(commitment(choice, salt).as_str()) {
                return Err("the revealed choice does not match its commitment".into());
            }
            let theirs = pending
                .take()
                .ok_or("a reveal must follow the follower's choice")?;
            match beats(choice, &theirs) {
                Some(true) => score[0] += 1,
                Some(false) => score[1] += 1,
                None => {}
            }
            rounds.push((choice.to_string(), theirs));
            let over = score.iter().any(|&wins| wins >= WINS_NEEDED);
            if over != ends {
                return Err(if over {
                    "the match is over, so a reveal must not commit again".into()
                } else {
                    "the match goes on, so a reveal must commit to the next round".into()
                });
            }
            if over {
                winner = Some(if score[0] >= WINS_NEEDED {
                    leader.clone()
                } else {
                    follower.clone()
                });
            } else {
                let next = get(KEY_COMMIT);
                if !is_lower_hex(next, 64) {
                    return Err("a commitment must be 64 lowercase hexadecimal characters".into());
                }
                commit = Some(next.to_string());
            }
        }
    }

    let (turn, phase) = match (&winner, plies.len()) {
        (Some(_), _) => (String::new(), ""),
        (None, 0) => (leader.clone(), PHASE_COMMIT),
        (None, n) if n % 2 == 1 => (follower.clone(), PHASE_CHOOSE),
        (None, _) => (leader.clone(), PHASE_REVEAL),
    };
    Ok(Replayed {
        turn,
        phase,
        rounds,
        score,
        winner,
        pending,
    })
}

/// The built-in Rock Paper Scissors LRGP application.
pub struct RpsApp {
    sessions: Mutex<HashMap<(String, String), Session>>,
}

impl Default for RpsApp {
    fn default() -> Self {
        Self::new()
    }
}

impl RpsApp {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn rejected(&self, text: impl std::fmt::Display) -> OutgoingResult {
        OutgoingResult {
            payload: HashMap::new(),
            fallback_text: format!("[LRGP {DISPLAY_NAME}] {text}"),
        }
    }

    fn emit_event(event_type: &str, session_id: &str, from: &str) -> HashMap<String, JsonValue> {
        HashMap::from([
            ("type".to_string(), JsonValue::String(event_type.into())),
            (
                "session_id".to_string(),
                JsonValue::String(session_id.into()),
            ),
            ("app_id".to_string(), JsonValue::String(APP_ID.into())),
            ("from".to_string(), JsonValue::String(from.into())),
        ])
    }

    fn ttl_policy() -> HashMap<String, f64> {
        HashMap::from([
            (STATUS_PENDING.to_string(), TTL_PENDING),
            (STATUS_ACTIVE.to_string(), TTL_ACTIVE),
        ])
    }

    fn check_session_expiry(session: &mut Session) -> bool {
        let expired = SessionStateMachine::check_expiry(session, Some(&Self::ttl_policy()), None);
        if session.status == STATUS_EXPIRED {
            session.clear_draw_offer();
        }
        expired
    }

    pub(crate) fn get_session(&self, session_id: &str, identity_id: &str) -> Option<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(&(session_id.to_string(), identity_id.to_string()))?;
        Self::check_session_expiry(session);
        Some(session.clone())
    }

    fn save_session(&self, session: &Session) {
        self.sessions.lock().unwrap().insert(
            (session.session_id.clone(), session.identity_id.clone()),
            session.clone(),
        );
    }

    fn default_metadata() -> HashMap<String, JsonValue> {
        let mut m = HashMap::new();
        for key in [
            "my_color",
            "first_turn",
            "turn",
            "phase",
            "winner",
            "terminal",
            "terminal_reason",
            "draw_offered_by",
            "my_choice",
            "my_salt",
        ] {
            m.insert(key.into(), JsonValue::String(String::new()));
        }
        for key in ["plies", "rounds", "legal_moves"] {
            m.insert(key.into(), JsonValue::Array(Vec::new()));
        }
        for key in ["move_count", "black_score", "white_score"] {
            m.insert(key.into(), JsonValue::from(0));
        }
        m.insert("draw_offered".into(), JsonValue::Bool(false));
        m
    }

    fn refresh(session: &mut Session, replayed: &Replayed) {
        let leading = meta_str(&session.metadata, "first_turn") == session.identity_id;
        let meta = &mut session.metadata;
        let plies = meta
            .get("plies")
            .and_then(JsonValue::as_array)
            .map_or(0, Vec::len);
        meta.insert("turn".into(), JsonValue::String(replayed.turn.clone()));
        meta.insert("phase".into(), JsonValue::String(replayed.phase.into()));
        meta.insert("move_count".into(), JsonValue::from(plies));
        meta.insert(
            "rounds".into(),
            JsonValue::Array(
                replayed
                    .rounds
                    .iter()
                    .map(|(lead, follow)| JsonValue::from(vec![lead.clone(), follow.clone()]))
                    .collect(),
            ),
        );
        meta.insert("black_score".into(), JsonValue::from(replayed.score[0]));
        meta.insert("white_score".into(), JsonValue::from(replayed.score[1]));
        // A reveal that ends the match takes no new choice.
        let ends = match (&replayed.pending, replayed.phase) {
            (Some(theirs), PHASE_REVEAL) => {
                let mine = meta_str(meta, "my_choice");
                let mut score = replayed.score;
                match beats(&mine, theirs) {
                    Some(true) => score[0] += 1,
                    Some(false) => score[1] += 1,
                    None => {}
                }
                leading && score.iter().any(|&wins| wins >= WINS_NEEDED)
            }
            _ => false,
        };
        let legal: Vec<JsonValue> = if replayed.phase.is_empty() || ends {
            Vec::new()
        } else {
            CHOICES
                .iter()
                .map(|c| JsonValue::String((*c).into()))
                .collect()
        };
        meta.insert("legal_moves".into(), JsonValue::Array(legal));
        if let Some(winner) = &replayed.winner {
            meta.insert("winner".into(), JsonValue::String(winner.clone()));
            meta.insert("terminal".into(), JsonValue::String("win".into()));
            meta.insert("terminal_reason".into(), JsonValue::String(R_ROUNDS.into()));
        }
    }

    fn start_game(session: &mut Session, leader: &str) -> Result<(), String> {
        let my_color = if leader == session.identity_id {
            "b"
        } else {
            "w"
        };
        session
            .metadata
            .insert("first_turn".into(), JsonValue::String(leader.into()));
        session
            .metadata
            .insert("my_color".into(), JsonValue::String(my_color.into()));
        let replayed = replay(session)?;
        Self::refresh(session, &replayed);
        Ok(())
    }

    fn with_ply(session: &Session, ply: Ply) -> Result<(Session, Replayed), String> {
        let mut candidate = session.clone();
        let mut plies = stored_plies(&candidate.metadata)?;
        plies.push(ply);
        candidate
            .metadata
            .insert("plies".into(), plies_value(&plies));
        let replayed = replay(&candidate)?;
        Ok((candidate, replayed))
    }

    fn finish(session: &mut Session, terminal: &str, reason: &str, winner: &str) {
        let meta = &mut session.metadata;
        meta.insert("terminal".into(), JsonValue::String(terminal.into()));
        meta.insert("terminal_reason".into(), JsonValue::String(reason.into()));
        meta.insert("winner".into(), JsonValue::String(winner.into()));
        meta.insert("turn".into(), JsonValue::String(String::new()));
        meta.insert("phase".into(), JsonValue::String(String::new()));
        meta.insert("legal_moves".into(), JsonValue::Array(Vec::new()));
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
        let string = |key: &str| payload.get(key).and_then(value_as_str);
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
                let leader = string(KEY_BLACK).ok_or("accept b must be a string")?;
                if leader != identity_id && leader != sender_hash {
                    return Err("accept b is not a bound participant".into());
                }
            }
            CMD_MOVE => {
                if payload.get(KEY_PLY).and_then(value_as_u64).is_none() {
                    return Err("move n must be an unsigned integer".into());
                }
                let typed = payload
                    .iter()
                    .filter(|(key, _)| key.as_str() != KEY_PLY)
                    .all(|(_, value)| value_as_str(value).is_some());
                if !typed {
                    return Err("move payload contains a value with the wrong type".into());
                }
                let shape_ok = match string(KEY_TERMINAL) {
                    Some("win") => has_exact_keys(
                        payload,
                        &[KEY_CHOICE, KEY_SALT, KEY_PLY, KEY_TERMINAL, KEY_WINNER],
                    ),
                    Some("") => [
                        &[KEY_COMMIT, KEY_PLY, KEY_TERMINAL][..],
                        &[KEY_CHOICE, KEY_PLY, KEY_TERMINAL][..],
                        &[KEY_CHOICE, KEY_SALT, KEY_COMMIT, KEY_PLY, KEY_TERMINAL][..],
                    ]
                    .iter()
                    .any(|keys| has_exact_keys(payload, keys)),
                    _ => false,
                };
                if !shape_ok {
                    return Err("move payload has invalid keys".into());
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

    fn validate_move(
        &self,
        session: &Session,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
    ) -> Result<(Session, Replayed), String> {
        if session.status != STATUS_ACTIVE {
            return Err(format!("Session is not active (status={})", session.status));
        }
        let turn = meta_str(&session.metadata, "turn");
        if turn.is_empty() {
            return Err("Turn is required before moves".into());
        }
        if turn != sender_hash {
            return Err("Not your turn".into());
        }
        let expected = stored_plies(&session.metadata)?.len() as u64;
        let ply = payload.get(KEY_PLY).and_then(value_as_u64);
        if ply != Some(expected) {
            return Err(format!("Ply mismatch: expected {expected}, got {ply:?}"));
        }
        let stored: Ply = payload
            .iter()
            .filter(|(key, _)| ![KEY_PLY, KEY_TERMINAL, KEY_WINNER].contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value_as_str(value).unwrap_or("").to_string()))
            .collect();
        let (candidate, replayed) = Self::with_ply(session, stored)?;
        let terminal = payload
            .get(KEY_TERMINAL)
            .and_then(value_as_str)
            .unwrap_or("");
        let claimed = payload.get(KEY_WINNER).and_then(value_as_str).unwrap_or("");
        match (&replayed.winner, terminal) {
            (Some(winner), "win") if claimed == winner => {}
            (None, "") => {}
            _ => return Err("Terminal claim does not match the replayed match".into()),
        }
        Ok((candidate, replayed))
    }

    fn validate_restored_session(session: &Session) -> Result<(), LrgpError> {
        let invalid = |message: &str| LrgpError::Validation {
            code: ERR_PROTOCOL_ERROR.into(),
            message: message.into(),
        };
        let meta = &session.metadata;
        for key in [
            "my_color",
            "first_turn",
            "turn",
            "phase",
            "winner",
            "terminal",
            "terminal_reason",
            "my_choice",
            "my_salt",
        ] {
            if !meta.get(key).is_some_and(JsonValue::is_string) {
                return Err(invalid(&format!("restored {key} must be a string")));
            }
        }
        let plies = stored_plies(meta).map_err(|e| invalid(&format!("restored {e}")))?;
        let text = |key: &str| meta_str(meta, key);
        match session.status.as_str() {
            STATUS_PENDING | STATUS_DECLINED => {
                let started = !plies.is_empty()
                    || [
                        "first_turn",
                        "my_color",
                        "turn",
                        "terminal",
                        "winner",
                        "my_choice",
                    ]
                    .iter()
                    .any(|key| !text(key).is_empty());
                if started {
                    return Err(invalid(
                        "pending or declined session contains game progress",
                    ));
                }
            }
            STATUS_ACTIVE | STATUS_COMPLETED | STATUS_EXPIRED => {
                let leader = text("first_turn");
                if leader != session.identity_id && leader != session.contact_hash {
                    return Err(invalid("restored first_turn is not a bound participant"));
                }
                let color = if leader == session.identity_id {
                    "b"
                } else {
                    "w"
                };
                if text("my_color") != color {
                    return Err(invalid("restored my_color does not match first_turn"));
                }
                let replayed = replay(session)
                    .map_err(|e| invalid(&format!("restored match is invalid: {e}")))?;
                if meta.get("move_count").and_then(JsonValue::as_u64) != Some(plies.len() as u64) {
                    return Err(invalid("restored move_count does not match the plies"));
                }
                // The leader's own secret must match the commitment awaiting a reveal.
                let awaiting = replayed.winner.is_none() && !plies.is_empty();
                if leader == session.identity_id && awaiting {
                    let commit = plies
                        .iter()
                        .rev()
                        .find_map(|ply| ply.get(KEY_COMMIT))
                        .cloned()
                        .unwrap_or_default();
                    if commitment(&text("my_choice"), &text("my_salt")) != commit {
                        return Err(invalid("restored choice does not match the commitment"));
                    }
                }
                let (terminal, reason, winner, turn) = (
                    text("terminal"),
                    text("terminal_reason"),
                    text("winner"),
                    text("turn"),
                );
                let consistent = match (session.status.as_str(), &replayed.winner) {
                    (STATUS_ACTIVE, None) => {
                        turn == replayed.turn
                            && terminal.is_empty()
                            && reason.is_empty()
                            && winner.is_empty()
                    }
                    (STATUS_EXPIRED, None) => {
                        terminal.is_empty()
                            && winner.is_empty()
                            && (turn.is_empty() || turn == replayed.turn)
                    }
                    (STATUS_COMPLETED, Some(match_winner)) => {
                        turn.is_empty()
                            && terminal == "win"
                            && reason == R_ROUNDS
                            && winner == *match_winner
                    }
                    (STATUS_COMPLETED, None) => {
                        turn.is_empty()
                            && match (terminal.as_str(), reason.as_str()) {
                                ("win", R_RESIGN) => {
                                    winner == session.identity_id || winner == session.contact_hash
                                }
                                ("draw", R_AGREEMENT) => winner.is_empty(),
                                _ => false,
                            }
                    }
                    _ => false,
                };
                if !consistent {
                    return Err(invalid(
                        "restored terminal metadata does not match the match",
                    ));
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

    fn new_session(session_id: &str, identity_id: &str, initiator: &str) -> Session {
        let mut session = Session::new(session_id);
        session.identity_id = identity_id.to_string();
        session.app_id = APP_ID.to_string();
        session.app_version = APP_VERSION;
        session.initiator = initiator.to_string();
        session.status = STATUS_PENDING.to_string();
        session.metadata = Self::default_metadata();
        session
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
        let mut session = Self::new_session(session_id, identity_id, sender_hash);
        session.contact_hash = sender_hash.to_string();
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
        let Some(mut session) = self.get_session(session_id, identity_id) else {
            return error_result(ERR_PROTOCOL_ERROR, "Unknown session");
        };
        if session.contact_hash.is_empty() {
            session.contact_hash = sender_hash.to_string();
        }
        if let Err(e) = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false) {
            return error_result(ERR_PROTOCOL_ERROR, &e.to_string());
        }
        let leader = payload.get(KEY_BLACK).and_then(value_as_str).unwrap_or("");
        if leader != identity_id && leader != sender_hash {
            return error_result(
                ERR_PROTOCOL_ERROR,
                "ACCEPT b is not one of the bound participants",
            );
        }
        if let Err(error) = Self::start_game(&mut session, leader) {
            return error_result(ERR_PROTOCOL_ERROR, &error);
        }
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
        let Some(session) = self.get_session(session_id, identity_id) else {
            return error_result(ERR_PROTOCOL_ERROR, "Unknown session");
        };
        let (mut candidate, replayed) = match self.validate_move(&session, payload, sender_hash) {
            Ok(applied) => applied,
            Err(message) => {
                let error = HashMap::from([
                    (
                        "code".to_string(),
                        JsonValue::String(ERR_INVALID_MOVE.into()),
                    ),
                    ("msg".to_string(), JsonValue::String(message)),
                    ("ref".to_string(), JsonValue::String(CMD_MOVE.into())),
                ]);
                return IncomingResult {
                    session: Some(session_to_json(&session)),
                    emit: None,
                    error: Some(error),
                };
            }
        };
        Self::refresh(&mut candidate, &replayed);
        candidate.clear_draw_offer();
        if let Err(error) =
            SessionStateMachine::apply_command(&mut candidate, CMD_MOVE, replayed.winner.is_some())
        {
            return error_result(ERR_PROTOCOL_ERROR, &error.to_string());
        }
        candidate.unread = 1;
        self.save_session(&candidate);
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
            session: Some(session_to_json(&candidate)),
            emit: Some(emit),
            error: None,
        }
    }

    fn handle_simple_in(
        &self,
        session_id: &str,
        command: &str,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult {
        let Some(mut session) = self.get_session(session_id, identity_id) else {
            return error_result(ERR_PROTOCOL_ERROR, "Unknown session");
        };
        if command == CMD_DRAW_OFFER && session.has_draw_offer() {
            return error_result(ERR_PROTOCOL_ERROR, "A draw offer is already outstanding");
        }
        if matches!(command, CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE) {
            match session.draw_offered_by() {
                None => return error_result(ERR_PROTOCOL_ERROR, "No draw offer is outstanding"),
                Some(owner) if owner == sender_hash => {
                    return error_result(
                        ERR_PROTOCOL_ERROR,
                        "A participant cannot answer its own draw offer",
                    );
                }
                Some(_) => {}
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
        self.save_session(&Self::new_session(&sid, identity_id, identity_id));
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
        let leader = if flip_responder_coin() {
            session.initiator.clone()
        } else {
            identity_id.to_string()
        };
        if let Err(error) = Self::start_game(&mut session, &leader) {
            return self.rejected(error);
        }
        self.save_session(&session);
        OutgoingResult {
            payload: HashMap::from([(KEY_BLACK.to_string(), rmpv::Value::String(leader.into()))]),
            fallback_text: self.render_fallback_inner(CMD_ACCEPT, &HashMap::new()),
        }
    }

    /// Check a local intent and return the session with it applied and the
    /// wire payload. The intent is `{c}`, or `{}` for a reveal that ends the
    /// match; `k` may be given with a choice that will be committed.
    fn prepare_move(
        &self,
        session: &Session,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> Result<(Session, Replayed, HashMap<String, rmpv::Value>), String> {
        if session.status != STATUS_ACTIVE {
            return Err(format!("Session is not active ({})", session.status));
        }
        if meta_str(&session.metadata, "turn") != identity_id {
            return Err("Not your turn".into());
        }
        let replayed = replay(session).map_err(|e| format!("Local replay failed: {e}"))?;
        let choice = payload.get(KEY_CHOICE).and_then(value_as_str);
        let salt = match payload.get(KEY_SALT) {
            None => random_salt(),
            Some(value) => value_as_str(value)
                .filter(|salt| is_lower_hex(salt, 32))
                .ok_or("k must be 32 lowercase hexadecimal characters")?
                .to_string(),
        };
        let wants_choice = !session
            .metadata
            .get("legal_moves")
            .and_then(JsonValue::as_array)
            .is_none_or(Vec::is_empty);
        let shape_ok = if wants_choice {
            has_exact_keys(payload, &[KEY_CHOICE])
                || has_exact_keys(payload, &[KEY_CHOICE, KEY_SALT])
        } else {
            payload.is_empty()
        };
        if !shape_ok || choice.is_some_and(|c| !is_choice(c)) {
            return Err(if wants_choice {
                "Choose r, p, or s".into()
            } else {
                "The match ends with this reveal: send an empty intent".into()
            });
        }
        let text = |value: &str| rmpv::Value::String(value.into());
        let n = rmpv::Value::Integer((stored_plies(&session.metadata)?.len() as u64).into());
        let choice = choice.unwrap_or("");
        let mut ply = Ply::new();
        let mut secret = None;
        match replayed.phase {
            PHASE_COMMIT => {
                ply.insert(KEY_COMMIT.into(), commitment(choice, &salt));
                secret = Some((choice, salt));
            }
            PHASE_CHOOSE => {
                ply.insert(KEY_CHOICE.into(), choice.into());
            }
            PHASE_REVEAL => {
                ply.insert(KEY_CHOICE.into(), meta_str(&session.metadata, "my_choice"));
                ply.insert(KEY_SALT.into(), meta_str(&session.metadata, "my_salt"));
                if wants_choice {
                    ply.insert(KEY_COMMIT.into(), commitment(choice, &salt));
                    secret = Some((choice, salt));
                }
            }
            _ => return Err("The match is over".into()),
        }
        let (mut candidate, after) = Self::with_ply(session, ply.clone())?;
        if let Some((choice, salt)) = secret {
            candidate
                .metadata
                .insert("my_choice".into(), JsonValue::String(choice.into()));
            candidate
                .metadata
                .insert("my_salt".into(), JsonValue::String(salt));
        }
        let mut wire: HashMap<String, rmpv::Value> =
            ply.iter().map(|(k, v)| (k.clone(), text(v))).collect();
        wire.insert(KEY_PLY.into(), n);
        match &after.winner {
            Some(winner) => {
                wire.insert(KEY_TERMINAL.into(), text("win"));
                wire.insert(KEY_WINNER.into(), text(winner));
            }
            None => {
                wire.insert(KEY_TERMINAL.into(), text(""));
            }
        }
        Ok((candidate, after, wire))
    }

    fn handle_move_out(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> OutgoingResult {
        let Some(session) = self.get_session(session_id, identity_id) else {
            return self.rejected("Session not found");
        };
        let (mut candidate, replayed, wire) =
            match self.prepare_move(&session, payload, identity_id) {
                Ok(prepared) => prepared,
                Err(error) => return self.rejected(error),
            };
        Self::refresh(&mut candidate, &replayed);
        candidate.clear_draw_offer();
        if let Err(error) =
            SessionStateMachine::apply_command(&mut candidate, CMD_MOVE, replayed.winner.is_some())
        {
            return self.rejected(error);
        }
        self.save_session(&candidate);
        let fallback = self.render_fallback_inner(CMD_MOVE, &wire);
        OutgoingResult {
            payload: wire,
            fallback_text: fallback,
        }
    }

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
                format!("[LRGP {DISPLAY_NAME}] Challenge cancelled")
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
        let prefix = format!("[LRGP {DISPLAY_NAME}]");
        let string = |key: &str| payload.get(key).and_then(value_as_str);
        let name = |choice: &str| match choice {
            "r" => "rock",
            "p" => "paper",
            _ => "scissors",
        };
        match command {
            CMD_CHALLENGE => format!("{prefix} Sent a challenge!"),
            CMD_ACCEPT => format!("{prefix} Challenge accepted"),
            CMD_DECLINE => format!("{prefix} Challenge declined"),
            CMD_MOVE => match (string(KEY_CHOICE), string(KEY_TERMINAL)) {
                (Some(choice), Some("win")) => {
                    format!("{prefix} Showed {} (match over)", name(choice))
                }
                (Some(choice), _) if payload.contains_key(KEY_SALT) => {
                    format!("{prefix} Showed {} and chose again", name(choice))
                }
                (Some(choice), _) => format!("{prefix} Chose {}", name(choice)),
                _ => format!("{prefix} Chose in secret"),
            },
            CMD_RESIGN => format!("{prefix} Resigned."),
            CMD_DRAW_OFFER => format!("{prefix} Offered a draw"),
            CMD_DRAW_ACCEPT => format!("{prefix} Draw accepted"),
            CMD_DRAW_DECLINE => format!("{prefix} Draw declined"),
            CMD_ERROR => format!("{prefix} Error: {}", string("msg").unwrap_or("Unknown")),
            other => format!("{prefix} {other}"),
        }
    }
}

impl GameApp for RpsApp {
    fn app_id(&self) -> &str {
        APP_ID
    }

    fn version(&self) -> u32 {
        APP_VERSION
    }

    fn manifest(&self) -> AppManifest {
        let actions: Vec<String> = [
            CMD_CHALLENGE,
            CMD_ACCEPT,
            CMD_DECLINE,
            CMD_MOVE,
            CMD_RESIGN,
            CMD_DRAW_OFFER,
            CMD_DRAW_ACCEPT,
            CMD_DRAW_DECLINE,
            CMD_ERROR,
        ]
        .map(String::from)
        .to_vec();
        let preferred_delivery = actions
            .iter()
            .map(|command| (command.clone(), self.get_delivery_method(command)))
            .collect();
        AppManifest {
            app_id: APP_ID.into(),
            version: APP_VERSION,
            display_name: DISPLAY_NAME.into(),
            icon: APP_ID.into(),
            session_type: SESSION_ROUND_BASED.into(),
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
        if Self::check_session_expiry(&mut session) {
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
            CMD_MOVE => {}
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
            match session.draw_offered_by() {
                None => return (false, Some("No draw offer is outstanding".into())),
                Some(owner) if owner == identity_id => {
                    return (
                        false,
                        Some("A participant cannot answer its own draw offer".into()),
                    );
                }
                Some(_) => {}
            }
        }
        if command != CMD_MOVE {
            let mut candidate = session;
            return match SessionStateMachine::apply_command(&mut candidate, command, false) {
                Ok(_) => (true, None),
                Err(error) => (false, Some(error.to_string())),
            };
        }
        if session.contact_hash.is_empty() {
            return (false, Some("Opponent unknown".into()));
        }
        match self.prepare_move(&session, payload, identity_id) {
            Ok(_) => (true, None),
            Err(error) => (false, Some(error)),
        }
    }

    fn get_session_state(&self, session_id: &str, identity_id: &str) -> HashMap<String, JsonValue> {
        self.get_session(session_id, identity_id)
            .map(|s| session_to_json(&s))
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

    fn upsert_session(&self, session: Session) -> Result<(), LrgpError> {
        if session.app_id != APP_ID || session.app_version != APP_VERSION {
            return Err(LrgpError::Validation {
                code: ERR_UNSUPPORTED_APP.into(),
                message: "session app/version does not match Rock Paper Scissors".into(),
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
        if session.draw_offered_by().is_none() || session.status == STATUS_EXPIRED {
            session.clear_draw_offer();
        }
        Self::validate_restored_session(&session)?;
        if session.status == STATUS_ACTIVE {
            if let Ok(replayed) = replay(&session) {
                Self::refresh(&mut session, &replayed);
            }
        }
        Self::check_session_expiry(&mut session);
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
        sessions
            .values_mut()
            .filter(|session| identity_id.is_none_or(|id| session.identity_id == id))
            .map(|session| {
                Self::check_session_expiry(session);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::harness::{ALICE, BOB, SID};
    use crate::envelope::{pack_envelope, validate_envelope_size};

    const SALT: &str = "00112233445566778899aabbccddeeff";

    static COIN_LOCK: Mutex<()> = Mutex::new(());

    struct Pin(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl Drop for Pin {
        fn drop(&mut self) {
            force_coin(None);
        }
    }

    fn setup(challenger_leads: bool) -> (RpsApp, Pin) {
        let guard = COIN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        force_coin(Some(challenger_leads));
        let app = RpsApp::new();
        let challenge = app.handle_outgoing(SID, CMD_CHALLENGE, &HashMap::new(), ALICE);
        assert!(challenge.payload.is_empty());
        app.bind_session_peer(SID, ALICE, BOB).unwrap();
        assert!(
            app.handle_incoming(SID, CMD_CHALLENGE, &HashMap::new(), ALICE, BOB)
                .error
                .is_none()
        );
        let accept = app.handle_outgoing(SID, CMD_ACCEPT, &HashMap::new(), BOB);
        let accepted = app.handle_incoming(SID, CMD_ACCEPT, &accept.payload, BOB, ALICE);
        assert!(accepted.error.is_none(), "{:?}", accepted.error);
        (app, Pin(guard))
    }

    fn text(value: &str) -> rmpv::Value {
        rmpv::Value::String(value.into())
    }

    fn choose(choice: &str) -> HashMap<String, rmpv::Value> {
        HashMap::from([
            (KEY_CHOICE.into(), text(choice)),
            (KEY_SALT.into(), text(SALT)),
        ])
    }

    fn other(identity: &str) -> &'static str {
        if identity == ALICE { BOB } else { ALICE }
    }

    fn meta(app: &RpsApp, identity: &str) -> HashMap<String, JsonValue> {
        app.get_session(SID, identity).unwrap().metadata
    }

    fn send(
        app: &RpsApp,
        intent: &HashMap<String, rmpv::Value>,
        mover: &str,
    ) -> HashMap<String, rmpv::Value> {
        let (valid, message) = app.validate_outgoing_action(SID, CMD_MOVE, intent, mover);
        assert!(valid, "{intent:?} rejected: {message:?}");
        let out = app.handle_outgoing(SID, CMD_MOVE, intent, mover);
        assert!(!out.payload.is_empty(), "{}", out.fallback_text);
        let incoming = app.handle_incoming(SID, CMD_MOVE, &out.payload, mover, other(mover));
        assert!(incoming.error.is_none(), "{:?}", incoming.error);
        out.payload
    }

    /// Play rounds of (leader, follower) choices; the leader is Alice.
    fn play(app: &RpsApp, rounds: &[(&str, &str)]) -> Vec<HashMap<String, rmpv::Value>> {
        let mut wires = vec![send(app, &choose(rounds[0].0), ALICE)];
        for (index, (_, follow)) in rounds.iter().enumerate() {
            wires.push(send(app, &choose(follow), BOB));
            let next = rounds
                .get(index + 1)
                .map(|(lead, _)| choose(lead))
                .unwrap_or_default();
            wires.push(send(app, &next, ALICE));
        }
        wires
    }

    #[test]
    fn choices_and_commitments() {
        assert_eq!(beats("r", "s"), Some(true));
        assert_eq!(beats("s", "r"), Some(false));
        assert_eq!(beats("p", "p"), None);
        assert_eq!(
            commitment("r", SALT),
            "43cde9c5adb91a12b3311cc17a33f446dc379049bae82ad0ff5b391c13cd7a4a"
        );
        assert!(is_lower_hex(&commitment("r", SALT), 64));
        assert_ne!(commitment("r", SALT), commitment("p", SALT));
    }

    #[test]
    fn a_match_to_three() {
        let (app, _coin) = setup(true);
        let m = meta(&app, BOB);
        assert_eq!(meta_str(&m, "phase"), PHASE_COMMIT);
        assert_eq!(meta_str(&m, "turn"), ALICE);

        // Alice wins, ties, loses, wins, wins: 3 to 1.
        let wires = play(
            &app,
            &[("r", "s"), ("p", "p"), ("s", "r"), ("p", "r"), ("s", "p")],
        );
        assert!(has_exact_keys(
            &wires[0],
            &[KEY_COMMIT, KEY_PLY, KEY_TERMINAL]
        ));
        assert_eq!(wires[0][KEY_COMMIT], text(&commitment("r", SALT)));
        assert!(has_exact_keys(
            &wires[1],
            &[KEY_CHOICE, KEY_PLY, KEY_TERMINAL]
        ));
        assert!(has_exact_keys(
            &wires[2],
            &[KEY_CHOICE, KEY_SALT, KEY_COMMIT, KEY_PLY, KEY_TERMINAL]
        ));
        let last = wires.last().unwrap();
        assert!(has_exact_keys(
            last,
            &[KEY_CHOICE, KEY_SALT, KEY_PLY, KEY_TERMINAL, KEY_WINNER]
        ));
        assert_eq!(last[KEY_WINNER], text(ALICE));
        assert_eq!(last[KEY_PLY], rmpv::Value::from(10));

        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(session.status, STATUS_COMPLETED);
            let m = &session.metadata;
            assert_eq!(meta_str(m, "winner"), ALICE);
            assert_eq!(meta_str(m, "terminal_reason"), R_ROUNDS);
            assert_eq!(m["black_score"], JsonValue::from(3));
            assert_eq!(m["white_score"], JsonValue::from(1));
            assert_eq!(m["rounds"].as_array().unwrap().len(), 5);
            RpsApp::new().upsert_session(session).unwrap();
        }
    }

    #[test]
    fn the_follower_can_win_and_the_last_reveal_is_empty() {
        let (app, _coin) = setup(false);
        assert_eq!(meta_str(&meta(&app, ALICE), "turn"), BOB);
        send(&app, &choose("r"), BOB);
        for round in 0..3 {
            send(&app, &choose("p"), ALICE);
            let ends = round == 2;
            let bob = meta(&app, BOB);
            assert_eq!(meta_str(&bob, "phase"), PHASE_REVEAL);
            assert_eq!(bob["legal_moves"].as_array().unwrap().is_empty(), ends);
            if ends {
                assert!(
                    !app.validate_outgoing_action(SID, CMD_MOVE, &choose("r"), BOB)
                        .0
                );
                let last = send(&app, &HashMap::new(), BOB);
                assert_eq!(last[KEY_WINNER], text(ALICE));
            } else {
                assert!(
                    !app.validate_outgoing_action(SID, CMD_MOVE, &HashMap::new(), BOB)
                        .0
                );
                send(&app, &choose("r"), BOB);
            }
        }
        assert_eq!(
            app.get_session(SID, ALICE).unwrap().status,
            STATUS_COMPLETED
        );
        assert_eq!(meta(&app, ALICE)["white_score"], JsonValue::from(3));
    }

    #[test]
    fn forged_moves_are_rejected_without_mutation() {
        let (app, _coin) = setup(true);
        send(&app, &choose("r"), ALICE);
        send(&app, &choose("s"), BOB);
        let before = meta(&app, BOB);
        let wire = |entries: &[(&str, rmpv::Value)]| -> HashMap<String, rmpv::Value> {
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        };
        let next = text(&commitment("p", SALT));
        let cases = [
            // Revealing a different choice from the one committed.
            wire(&[
                (KEY_CHOICE, text("p")),
                (KEY_SALT, text(SALT)),
                (KEY_COMMIT, next.clone()),
                (KEY_PLY, rmpv::Value::from(2)),
                (KEY_TERMINAL, text("")),
            ]),
            // The wrong salt.
            wire(&[
                (KEY_CHOICE, text("r")),
                (KEY_SALT, text(&"0".repeat(32))),
                (KEY_COMMIT, next.clone()),
                (KEY_PLY, rmpv::Value::from(2)),
                (KEY_TERMINAL, text("")),
            ]),
            // No commitment for the next round, or a false win.
            wire(&[
                (KEY_CHOICE, text("r")),
                (KEY_SALT, text(SALT)),
                (KEY_PLY, rmpv::Value::from(2)),
                (KEY_TERMINAL, text("")),
            ]),
            wire(&[
                (KEY_CHOICE, text("r")),
                (KEY_SALT, text(SALT)),
                (KEY_PLY, rmpv::Value::from(2)),
                (KEY_TERMINAL, text("win")),
                (KEY_WINNER, text(ALICE)),
            ]),
            // Wrong ply, bad hash, extra key, wrong type.
            wire(&[
                (KEY_CHOICE, text("r")),
                (KEY_SALT, text(SALT)),
                (KEY_COMMIT, next.clone()),
                (KEY_PLY, rmpv::Value::from(3)),
                (KEY_TERMINAL, text("")),
            ]),
            wire(&[
                (KEY_CHOICE, text("r")),
                (KEY_SALT, text(SALT)),
                (KEY_COMMIT, text("zz")),
                (KEY_PLY, rmpv::Value::from(2)),
                (KEY_TERMINAL, text("")),
            ]),
            wire(&[
                (KEY_CHOICE, text("r")),
                (KEY_SALT, text(SALT)),
                (KEY_COMMIT, next.clone()),
                (KEY_PLY, rmpv::Value::from(2)),
                (KEY_TERMINAL, text("")),
                ("z", text("")),
            ]),
            wire(&[
                (KEY_CHOICE, rmpv::Value::from(1)),
                (KEY_SALT, text(SALT)),
                (KEY_COMMIT, next),
                (KEY_PLY, rmpv::Value::from(2)),
                (KEY_TERMINAL, text("")),
            ]),
        ];
        for payload in &cases {
            assert!(
                app.handle_incoming(SID, CMD_MOVE, payload, ALICE, BOB)
                    .error
                    .is_some(),
                "accepted {payload:?}"
            );
        }
        // Bob choosing out of turn.
        let bob = wire(&[
            (KEY_CHOICE, text("r")),
            (KEY_PLY, rmpv::Value::from(2)),
            (KEY_TERMINAL, text("")),
        ]);
        assert!(
            app.handle_incoming(SID, CMD_MOVE, &bob, BOB, ALICE)
                .error
                .is_some()
        );
        assert_eq!(meta(&app, BOB), before);

        for bad in [
            HashMap::new(),
            choose("x"),
            HashMap::from([(KEY_CHOICE.into(), text("r")), ("z".into(), text(""))]),
        ] {
            assert!(
                !app.validate_outgoing_action(SID, CMD_MOVE, &bad, ALICE).0,
                "{bad:?}"
            );
        }
        assert!(
            app.handle_incoming("2222222222222222", CMD_CHALLENGE, &choose("r"), ALICE, BOB)
                .error
                .is_some()
        );
    }

    #[test]
    fn hydration_keeps_the_secret_and_rejects_tampering() {
        let (app, _coin) = setup(true);
        send(&app, &choose("r"), ALICE);
        send(&app, &choose("s"), BOB);
        for identity in [ALICE, BOB] {
            let record = app.get_session(SID, identity).unwrap();
            let fresh = RpsApp::new();
            fresh.upsert_session(record.clone()).unwrap();
            assert_eq!(
                fresh.get_session(SID, identity).unwrap().metadata,
                record.metadata
            );
        }
        let record = app.get_session(SID, ALICE).unwrap();
        assert_eq!(meta_str(&record.metadata, "my_choice"), "r");
        let tamper = |key: &str, value: JsonValue| {
            let mut copy = record.clone();
            copy.metadata.insert(key.into(), value);
            RpsApp::new().upsert_session(copy)
        };
        assert!(tamper("my_choice", JsonValue::String("p".into())).is_err());
        assert!(tamper("turn", JsonValue::String(BOB.into())).is_err());
        assert!(tamper("my_color", JsonValue::String("w".into())).is_err());
        assert!(tamper("move_count", JsonValue::from(1)).is_err());
        assert!(tamper("plies", JsonValue::from(vec!["r"])).is_err());
        assert!(tamper("terminal", JsonValue::String("win".into())).is_err());

        let mut expired = record;
        expired.last_action_at = 0.0;
        let fresh = RpsApp::new();
        fresh.upsert_session(expired).unwrap();
        assert_eq!(
            fresh.get_session(SID, ALICE).unwrap().status,
            STATUS_EXPIRED
        );
    }

    #[test]
    fn resign_draw_decline_and_authorization() {
        let (app, _coin) = setup(true);
        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB);
        app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), BOB, ALICE);
        assert!(
            !app.validate_outgoing_action(SID, CMD_DRAW_ACCEPT, &HashMap::new(), BOB)
                .0
        );
        app.handle_outgoing(SID, CMD_DRAW_DECLINE, &HashMap::new(), ALICE);
        app.handle_incoming(SID, CMD_DRAW_DECLINE, &HashMap::new(), ALICE, BOB);
        assert!(!app.get_session(SID, BOB).unwrap().has_draw_offer());
        assert!(app.authorize_incoming(SID, CMD_MOVE, "cc", ALICE).is_err());
        let snaps = [ALICE, BOB].map(|identity| app.snapshot_session(SID, identity));
        send(&app, &choose("r"), ALICE);
        for (identity, snap) in [ALICE, BOB].into_iter().zip(snaps) {
            app.rollback_session(SID, identity, snap);
        }
        assert_eq!(meta(&app, ALICE)["move_count"], JsonValue::from(0));
        send(&app, &choose("r"), ALICE);

        app.handle_outgoing(SID, CMD_RESIGN, &HashMap::new(), BOB);
        app.handle_incoming(SID, CMD_RESIGN, &HashMap::new(), BOB, ALICE);
        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(meta_str(&session.metadata, "winner"), ALICE);
            RpsApp::new().upsert_session(session).unwrap();
        }

        let fresh = RpsApp::new();
        fresh.handle_outgoing("3333333333333333", CMD_CHALLENGE, &HashMap::new(), ALICE);
        let out = fresh.handle_outgoing("3333333333333333", CMD_DECLINE, &HashMap::new(), ALICE);
        assert_eq!(
            out.fallback_text,
            "[LRGP Rock Paper Scissors] Challenge cancelled"
        );
        assert_eq!(fresh.manifest().session_type, SESSION_ROUND_BASED);
        assert_eq!(
            fresh.render_fallback(CMD_MOVE, &HashMap::new()),
            "[LRGP Rock Paper Scissors] Chose in secret"
        );
        assert_eq!(
            fresh.render_fallback(CMD_MOVE, &choose("r")),
            "[LRGP Rock Paper Scissors] Showed rock and chose again"
        );
    }

    #[test]
    fn worst_case_envelope_fits_the_budget() {
        let payload: HashMap<String, rmpv::Value> = [
            (KEY_CHOICE, text("s")),
            (KEY_SALT, text(SALT)),
            (KEY_COMMIT, text(&commitment("s", SALT))),
            (KEY_PLY, rmpv::Value::from(65_535)),
            (KEY_TERMINAL, text("")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let envelope = pack_envelope(APP_ID, 1, CMD_MOVE, SID, Some(payload), None).unwrap();
        let size = validate_envelope_size(&envelope).expect("envelope size OK");
        assert!(size <= 190, "worst-case envelope {size} bytes");
    }
}
