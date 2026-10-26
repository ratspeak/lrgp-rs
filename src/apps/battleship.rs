//! Battleship on a 10x10 board, the first LRGP game with hidden information.
//!
//! Each player commits to a fleet when the game starts by sending only a
//! SHA-256 hash of the fleet and a random salt (`h`). A shot (`s`) travels
//! with the answer to the opponent's previous shot (`a`): miss, hit, or sunk.
//! The player whose last ship sinks concedes by revealing the fleet and salt,
//! and the winner then reveals theirs, so each side checks the other's
//! answers against the commitment before the game counts. SPEC.md Appendix H
//! defines the protocol.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use super::placement::{
    R_AGREEMENT, R_RESIGN, error_result, gen_session_id, meta_str, meta_string_list, other_player,
    parse_square, rmpv_to_json, session_to_json, square_name, string_list_value,
};
use crate::app_base::{AppManifest, GameApp, IncomingResult, OutgoingResult};
use crate::constants::*;
use crate::envelope::{has_exact_keys, value_as_str, value_as_u64};
use crate::errors::LrgpError;
use crate::session::{Session, SessionStateMachine};

pub const APP_ID: &str = "battleship";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 10;
/// Ship lengths in fleet order: carrier, battleship, cruiser, submarine, destroyer.
pub const FLEET: [usize; 5] = [5, 4, 3, 3, 2];

const CELLS: usize = SIZE * SIZE;
const FLEET_SQUARES: usize = 17;
const DISPLAY_NAME: &str = "Battleship";
const R_SUNK: &str = "snk";

// Wire payload keys.
const KEY_SHOT: &str = "s"; // square fired at
const KEY_ANSWER: &str = "a"; // "" | "m" | "h" | "s", for the previous shot
const KEY_FLEET: &str = "f"; // revealed fleet
const KEY_SALT: &str = "k"; // revealed salt
const KEY_COMMIT: &str = "h"; // fleet commitment
const KEY_PLY: &str = "n"; // ply counter, 0-based
const KEY_TERMINAL: &str = "x"; // "" | "win"
const KEY_WINNER: &str = "w"; // winner identity hash
const KEY_BLACK: &str = "b"; // first shooter (ACCEPT payload)

const MISS: &str = "m";
const HIT: &str = "h";
const SUNK: &str = "s";

pub const PHASE_SHOOT: &str = "shoot";
pub const PHASE_CONCEDE: &str = "concede";
pub const PHASE_REVEAL: &str = "reveal";

#[cfg(any(test, feature = "test-helpers"))]
static FORCED_COIN: Mutex<Option<bool>> = Mutex::new(None);

/// Pin the responder coin-flip for tests (`true` = challenger shoots first).
/// Process-global; callers must serialize.
#[cfg(any(test, feature = "test-helpers"))]
pub fn force_coin(challenger_first: Option<bool>) {
    if let Ok(mut guard) = FORCED_COIN.lock() {
        *guard = challenger_first;
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

/// The squares of each ship, in fleet order, or `None` unless `text` is a
/// canonical fleet: five `<square><h|v>` tokens joined by single spaces,
/// each ship running from its square toward higher files (`h`) or ranks
/// (`v`), all on the board and none overlapping.
pub fn parse_fleet(text: &str) -> Option<Vec<Vec<usize>>> {
    let tokens: Vec<&str> = text.split(' ').collect();
    if tokens.len() != FLEET.len() {
        return None;
    }
    let mut taken = [false; CELLS];
    let mut ships = Vec::new();
    for (token, &length) in tokens.iter().zip(FLEET.iter()) {
        let (square, direction) = token.split_at(token.len().checked_sub(1)?);
        let start = parse_square(square, SIZE)?;
        let (file, rank) = (start % SIZE, start / SIZE);
        let step = match direction {
            "h" if file + length <= SIZE => 1,
            "v" if rank + length <= SIZE => SIZE,
            _ => return None,
        };
        let squares: Vec<usize> = (0..length).map(|i| start + i * step).collect();
        for &square in &squares {
            if std::mem::replace(&mut taken[square], true) {
                return None;
            }
        }
        ships.push(squares);
    }
    Some(ships)
}

/// Lowercase hex SHA-256 of `"<fleet>:<salt>"`.
pub fn commitment(fleet: &str, salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(fleet.as_bytes());
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

fn is_salt(text: &str) -> bool {
    is_lower_hex(text, 32)
}

fn is_commit(text: &str) -> bool {
    is_lower_hex(text, 64)
}

/// A random canonical fleet.
pub fn random_fleet() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    loop {
        let tokens: Vec<String> = FLEET
            .iter()
            .map(|&length| {
                let horizontal = rng.gen_bool(0.5);
                let (files, ranks) = if horizontal {
                    (SIZE - length + 1, SIZE)
                } else {
                    (SIZE, SIZE - length + 1)
                };
                let square = rng.gen_range(0..ranks) * SIZE + rng.gen_range(0..files);
                format!(
                    "{}{}",
                    square_name(square, SIZE),
                    if horizontal { "h" } else { "v" }
                )
            })
            .collect();
        let fleet = tokens.join(" ");
        if parse_fleet(&fleet).is_some() {
            return fleet;
        }
    }
}

fn random_salt() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// A fleet being fired at: its ships and the squares hit so far.
struct Target {
    ships: Vec<Vec<usize>>,
    hit: [bool; CELLS],
}

impl Target {
    fn new(ships: Vec<Vec<usize>>) -> Self {
        Self {
            ships,
            hit: [false; CELLS],
        }
    }

    /// Record a shot and return the true answer to it.
    fn fire(&mut self, square: usize) -> &'static str {
        self.hit[square] = true;
        match self.ships.iter().find(|ship| ship.contains(&square)) {
            None => MISS,
            Some(ship) if ship.iter().all(|&s| self.hit[s]) => SUNK,
            Some(_) => HIT,
        }
    }

    fn all_sunk(&self) -> bool {
        self.ships.iter().flatten().all(|&s| self.hit[s])
    }

    fn sunk(&self) -> usize {
        self.ships
            .iter()
            .filter(|ship| ship.iter().all(|&s| self.hit[s]))
            .count()
    }

    /// 100 cells from the top rank down: `s` ship, `x` hit ship, `o` miss, `_` water.
    fn render(&self, shots: &[usize]) -> String {
        let mut cells = [b'_'; CELLS];
        for &square in self.ships.iter().flatten() {
            cells[square] = b's';
        }
        for &square in shots {
            cells[square] = if cells[square] == b's' { b'x' } else { b'o' };
        }
        super::placement::render_grid(&cells, SIZE)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Shot(usize),
    Concede,
    Final,
}

/// Everything the stored plies imply, from one side's point of view.
struct Replayed {
    /// Identity to move next, empty once the game is over.
    turn: String,
    phase: &'static str,
    /// The answer the local player owes for the opponent's last shot.
    answer_due: &'static str,
    winner: Option<String>,
    legal_moves: Vec<String>,
    last_move: String,
    my_board: String,
    their_board: String,
    my_sunk: usize,
    their_sunk: usize,
}

/// Fleet details from session metadata.
struct Fleets {
    me: String,
    first: String,
    second: String,
    mine: Vec<Vec<usize>>,
    theirs: Option<Vec<Vec<usize>>>,
}

fn fleets(session: &Session) -> Result<Fleets, String> {
    let meta = &session.metadata;
    let me = session.identity_id.clone();
    let first = meta_str(meta, "first_turn");
    let second = other_player(session, &first).ok_or("first_turn is not a bound participant")?;
    let my_fleet = meta_str(meta, "my_fleet");
    let mine = parse_fleet(&my_fleet).ok_or("my fleet is not a canonical fleet")?;
    if commitment(&my_fleet, &meta_str(meta, "my_salt")) != meta_str(meta, "my_commit") {
        return Err("my fleet does not match my commitment".into());
    }
    let their_fleet = meta_str(meta, "their_fleet");
    let theirs = if their_fleet.is_empty() {
        None
    } else {
        let ships = parse_fleet(&their_fleet).ok_or("the revealed fleet is not canonical")?;
        if commitment(&their_fleet, &meta_str(meta, "their_salt")) != meta_str(meta, "their_commit")
        {
            return Err("the revealed fleet does not match its commitment".into());
        }
        Some(ships)
    };
    Ok(Fleets {
        me,
        first,
        second,
        mine,
        theirs,
    })
}

/// Replay the stored shots and answers, checking every answer that can be
/// checked: all of the local player's, and the opponent's once revealed.
fn replay(session: &Session) -> Result<Replayed, String> {
    let fleets = fleets(session)?;
    let shots = meta_string_list(&session.metadata, "shots");
    let answers = meta_string_list(&session.metadata, "answers");
    if shots.len() != answers.len() {
        return Err("shots and answers differ in length".into());
    }
    let mover = |ply: usize| {
        if ply & 1 == 0 {
            &fleets.first
        } else {
            &fleets.second
        }
    };

    let mut mine = Target::new(fleets.mine.clone());
    let mut theirs = fleets.theirs.clone().map(Target::new);
    let mut fired = [Vec::new(), Vec::new()]; // by me, by them
    let mut reported = Vec::new(); // their answers to my shots, in order
    let mut previous: Option<Kind> = None;

    for (ply, (shot, answer)) in shots.iter().zip(answers.iter()).enumerate() {
        let by_me = *mover(ply) == fleets.me;
        let kind = if !shot.is_empty() {
            Kind::Shot(parse_square(shot, SIZE).ok_or("a stored shot is not a square")?)
        } else if !answer.is_empty() {
            Kind::Concede
        } else {
            Kind::Final
        };
        if previous == Some(Kind::Final) {
            return Err("a move follows the end of the game".into());
        }

        // Every ply but the first and the final reveal answers the shot before it.
        match (kind, previous) {
            (Kind::Final, Some(Kind::Concede)) => {
                if !by_me && theirs.is_none() {
                    return Err("the winner's fleet is not revealed".into());
                }
            }
            (Kind::Final, _) => return Err("only a concession can be answered by a reveal".into()),
            (Kind::Shot(_), None) if answer.is_empty() => {}
            (_, Some(Kind::Shot(square))) => {
                if !matches!(answer.as_str(), MISS | HIT | SUNK) {
                    return Err("an answer must be m, h, or s".into());
                }
                let target = if by_me {
                    Some(&mut mine)
                } else {
                    theirs.as_mut()
                };
                if let Some(target) = target {
                    if target.fire(square) != answer {
                        return Err(format!(
                            "the answer to {} is false",
                            square_name(square, SIZE)
                        ));
                    }
                }
                if !by_me {
                    reported.push(answer.as_str());
                }
            }
            _ => return Err("a move is out of sequence".into()),
        }

        let hits = reported.iter().filter(|a| **a != MISS).count();
        let sunk = reported.iter().filter(|a| **a == SUNK).count();
        let sunk_now = |me: &Target| me.all_sunk();
        match kind {
            Kind::Shot(square) => {
                if by_me && sunk_now(&mine) {
                    return Err("a sunk fleet must concede, not shoot".into());
                }
                if !by_me && (sunk >= FLEET.len() || theirs.as_ref().is_some_and(sunk_now)) {
                    return Err("a sunk fleet must concede, not shoot".into());
                }
                let list = &mut fired[usize::from(!by_me)];
                if list.contains(&square) {
                    return Err(format!(
                        "{} was already fired at",
                        square_name(square, SIZE)
                    ));
                }
                list.push(square);
            }
            Kind::Concede => {
                if answer != SUNK {
                    return Err("a concession must answer s".into());
                }
                let gone = if by_me {
                    sunk_now(&mine)
                } else {
                    theirs.as_ref().is_some_and(sunk_now)
                };
                if !gone {
                    return Err("a concession must sink the last ship".into());
                }
            }
            Kind::Final => {}
        }
        if hits > FLEET_SQUARES || sunk > FLEET.len() {
            return Err("more hits or sinkings were reported than the fleet holds".into());
        }
        previous = Some(kind);
    }

    let plies = shots.len();
    let next = mover(plies).clone();
    // The answer owed to the opponent's last shot, if it is our turn.
    let answer_due = match previous {
        Some(Kind::Shot(square)) if next == fleets.me => mine.fire(square),
        _ => "",
    };
    let (turn, phase, winner) = match previous {
        Some(Kind::Final) => (String::new(), "", Some(mover(plies - 1).clone())),
        Some(Kind::Concede) => (next, PHASE_REVEAL, None),
        _ if next == fleets.me && mine.all_sunk() => (next, PHASE_CONCEDE, None),
        _ => (next, PHASE_SHOOT, None),
    };
    let legal_moves = if phase == PHASE_SHOOT {
        let done = &fired[usize::from(turn != fleets.me)];
        (0..CELLS)
            .filter(|square| !done.contains(square))
            .map(|square| square_name(square, SIZE))
            .collect()
    } else {
        Vec::new()
    };

    // Their board as far as the local player knows it.
    let their_board = match &theirs {
        Some(target) => target.render(&fired[0]),
        None => {
            let mut cells = [b'_'; CELLS];
            for (index, &square) in fired[0].iter().enumerate() {
                if let Some(answer) = reported.get(index) {
                    cells[square] = if *answer == MISS { b'o' } else { b'x' };
                } else {
                    cells[square] = b'?';
                }
            }
            super::placement::render_grid(&cells, SIZE)
        }
    };
    let my_board = Target::new(fleets.mine.clone()).render(&fired[1]);
    let their_sunk = match &theirs {
        Some(target) => target.sunk(),
        None => reported.iter().filter(|a| **a == SUNK).count(),
    };
    let last_move = shots
        .iter()
        .rev()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default();

    Ok(Replayed {
        turn,
        phase,
        answer_due,
        winner,
        legal_moves,
        last_move,
        my_board,
        their_board,
        my_sunk: mine.sunk(),
        their_sunk,
    })
}

/// Fleet and salt from a local intent: `{}`, `{f}`, or `{f, k}`.
fn chosen_fleet(payload: &HashMap<String, rmpv::Value>) -> Result<(String, String), String> {
    let fleet = match payload.get(KEY_FLEET) {
        None => random_fleet(),
        Some(value) => value_as_str(value)
            .filter(|fleet| parse_fleet(fleet).is_some())
            .ok_or("f must be a canonical fleet")?
            .to_string(),
    };
    let salt = match payload.get(KEY_SALT) {
        None => random_salt(),
        Some(value) => value_as_str(value)
            .filter(|salt| is_salt(salt))
            .ok_or("k must be 32 lowercase hexadecimal characters")?
            .to_string(),
    };
    let keys = payload.len();
    let valid = match keys {
        0 => true,
        1 => payload.contains_key(KEY_FLEET),
        2 => has_exact_keys(payload, &[KEY_FLEET, KEY_SALT]),
        _ => false,
    };
    if !valid {
        return Err("a fleet intent must be {}, {f}, or {f, k}".into());
    }
    Ok((fleet, salt))
}

/// The built-in Battleship LRGP application.
pub struct BattleshipApp {
    sessions: Mutex<HashMap<(String, String), Session>>,
}

impl Default for BattleshipApp {
    fn default() -> Self {
        Self::new()
    }
}

impl BattleshipApp {
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
        let mut m = HashMap::new();
        m.insert("type".into(), JsonValue::String(event_type.into()));
        m.insert("session_id".into(), JsonValue::String(session_id.into()));
        m.insert("app_id".into(), JsonValue::String(APP_ID.into()));
        m.insert("from".into(), JsonValue::String(from.into()));
        m
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
        let blank = JsonValue::String("_".repeat(CELLS));
        let mut m = HashMap::new();
        for key in [
            "my_color",
            "first_turn",
            "turn",
            "last_move",
            "winner",
            "terminal",
            "terminal_reason",
            "draw_offered_by",
            "phase",
            "my_fleet",
            "my_salt",
            "my_commit",
            "their_commit",
            "their_fleet",
            "their_salt",
        ] {
            m.insert(key.into(), JsonValue::String(String::new()));
        }
        for key in ["shots", "answers", "legal_moves"] {
            m.insert(key.into(), JsonValue::Array(Vec::new()));
        }
        for key in ["move_count", "my_sunk", "their_sunk"] {
            m.insert(key.into(), JsonValue::from(0));
        }
        m.insert("draw_offered".into(), JsonValue::Bool(false));
        m.insert("my_board".into(), blank.clone());
        m.insert("their_board".into(), blank);
        m
    }

    fn set_my_fleet(session: &mut Session, fleet: &str, salt: &str) {
        let meta = &mut session.metadata;
        meta.insert("my_fleet".into(), JsonValue::String(fleet.into()));
        meta.insert("my_salt".into(), JsonValue::String(salt.into()));
        meta.insert(
            "my_commit".into(),
            JsonValue::String(commitment(fleet, salt)),
        );
        if let Some(ships) = parse_fleet(fleet) {
            meta.insert(
                "my_board".into(),
                JsonValue::String(Target::new(ships).render(&[])),
            );
        }
    }

    /// Store derived metadata from a replay.
    fn refresh(session: &mut Session, replayed: &Replayed) {
        let meta = &mut session.metadata;
        let plies = meta_string_list(meta, "shots").len();
        meta.insert("turn".into(), JsonValue::String(replayed.turn.clone()));
        meta.insert("phase".into(), JsonValue::String(replayed.phase.into()));
        meta.insert(
            "legal_moves".into(),
            string_list_value(&replayed.legal_moves),
        );
        meta.insert("move_count".into(), JsonValue::from(plies));
        meta.insert(
            "last_move".into(),
            JsonValue::String(replayed.last_move.clone()),
        );
        meta.insert(
            "my_board".into(),
            JsonValue::String(replayed.my_board.clone()),
        );
        meta.insert(
            "their_board".into(),
            JsonValue::String(replayed.their_board.clone()),
        );
        meta.insert("my_sunk".into(), JsonValue::from(replayed.my_sunk));
        meta.insert("their_sunk".into(), JsonValue::from(replayed.their_sunk));
        if let Some(winner) = &replayed.winner {
            meta.insert("winner".into(), JsonValue::String(winner.clone()));
            meta.insert("terminal".into(), JsonValue::String("win".into()));
            meta.insert("terminal_reason".into(), JsonValue::String(R_SUNK.into()));
        }
    }

    fn start_game(session: &mut Session, first: &str) -> Result<(), String> {
        let my_color = if first == session.identity_id {
            "b"
        } else {
            "w"
        };
        session
            .metadata
            .insert("first_turn".into(), JsonValue::String(first.into()));
        session
            .metadata
            .insert("my_color".into(), JsonValue::String(my_color.into()));
        let replayed = replay(session)?;
        Self::refresh(session, &replayed);
        Ok(())
    }

    /// Append one ply and replay, without saving.
    fn with_ply(
        session: &Session,
        shot: &str,
        answer: &str,
        reveal: Option<(&str, &str)>,
    ) -> Result<(Session, Replayed), String> {
        let mut candidate = session.clone();
        let meta = &mut candidate.metadata;
        let mut shots = meta_string_list(meta, "shots");
        let mut answers = meta_string_list(meta, "answers");
        shots.push(shot.into());
        answers.push(answer.into());
        meta.insert("shots".into(), string_list_value(&shots));
        meta.insert("answers".into(), string_list_value(&answers));
        if let Some((fleet, salt)) = reveal {
            meta.insert("their_fleet".into(), JsonValue::String(fleet.into()));
            meta.insert("their_salt".into(), JsonValue::String(salt.into()));
        }
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
            CMD_DECLINE | CMD_RESIGN | CMD_DRAW_OFFER | CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE => {
                if !payload.is_empty() {
                    return Err(format!("{command} payload must be empty"));
                }
            }
            CMD_CHALLENGE => {
                if !has_exact_keys(payload, &[KEY_COMMIT])
                    || !string(KEY_COMMIT).is_some_and(is_commit)
                {
                    return Err("challenge payload must contain exactly a 64-character h".into());
                }
            }
            CMD_ACCEPT => {
                if !has_exact_keys(payload, &[KEY_BLACK, KEY_COMMIT])
                    || !string(KEY_COMMIT).is_some_and(is_commit)
                {
                    return Err("accept payload must contain exactly b and a 64-character h".into());
                }
                let first = string(KEY_BLACK).ok_or("accept b must be a string")?;
                if first != identity_id && first != sender_hash {
                    return Err("accept b is not a bound participant".into());
                }
            }
            CMD_MOVE => {
                let terminal = string(KEY_TERMINAL).ok_or("move x must be a string")?;
                let shape: &[&str] = if terminal == "win" {
                    &[KEY_FLEET, KEY_SALT, KEY_PLY, KEY_TERMINAL, KEY_WINNER]
                } else if terminal.is_empty() && payload.contains_key(KEY_SHOT) {
                    &[KEY_SHOT, KEY_ANSWER, KEY_PLY, KEY_TERMINAL]
                } else if terminal.is_empty() {
                    &[KEY_ANSWER, KEY_FLEET, KEY_SALT, KEY_PLY, KEY_TERMINAL]
                } else {
                    return Err(format!("unsupported terminal marker '{terminal}'"));
                };
                if !has_exact_keys(payload, shape) {
                    return Err("move payload has invalid keys".into());
                }
                let typed = payload.get(KEY_PLY).and_then(value_as_u64).is_some()
                    && shape
                        .iter()
                        .filter(|key| **key != KEY_PLY)
                        .all(|key| string(key).is_some());
                if !typed {
                    return Err("move payload contains a value with the wrong type".into());
                }
                if string(KEY_SHOT).is_some_and(|s| parse_square(s, SIZE).is_none())
                    || string(KEY_FLEET).is_some_and(|f| parse_fleet(f).is_none())
                    || string(KEY_SALT).is_some_and(|k| !is_salt(k))
                    || string(KEY_WINNER).is_some_and(str::is_empty)
                {
                    return Err("move payload contains an invalid value".into());
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

    /// Validate an inbound wire move and return the session with it applied.
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
        let string = |key: &str| payload.get(key).and_then(value_as_str).unwrap_or("");
        let ply = payload.get(KEY_PLY).and_then(value_as_u64);
        let expected = meta_string_list(&session.metadata, "shots").len() as u64;
        if ply != Some(expected) {
            return Err(format!("Ply mismatch: expected {expected}, got {ply:?}"));
        }
        let phase = meta_str(&session.metadata, "phase");
        let reveal = payload
            .contains_key(KEY_FLEET)
            .then(|| (string(KEY_FLEET), string(KEY_SALT)));
        let (shot, answer) = (string(KEY_SHOT), string(KEY_ANSWER));
        match string(KEY_TERMINAL) {
            "win" if phase != PHASE_REVEAL => return Err("Nothing to reveal for".into()),
            "" if phase == PHASE_REVEAL => return Err("The winner must reveal".into()),
            _ => {}
        }
        let (candidate, replayed) = Self::with_ply(session, shot, answer, reveal)?;
        match (&replayed.winner, string(KEY_TERMINAL)) {
            (Some(winner), "win") if winner == sender_hash && string(KEY_WINNER) == winner => {}
            (None, "") => {}
            _ => return Err("Terminal claim does not match the replayed game".into()),
        }
        Ok((candidate, replayed))
    }

    /// Semantic hydration check.
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
            "last_move",
            "winner",
            "terminal",
            "terminal_reason",
            "phase",
            "my_fleet",
            "my_salt",
            "my_commit",
            "their_commit",
            "their_fleet",
            "their_salt",
            "my_board",
            "their_board",
        ] {
            if !meta.get(key).is_some_and(JsonValue::is_string) {
                return Err(invalid(&format!("restored {key} must be a string")));
            }
        }
        for key in ["shots", "answers"] {
            let strings = meta
                .get(key)
                .and_then(JsonValue::as_array)
                .is_some_and(|items| items.iter().all(JsonValue::is_string));
            if !strings {
                return Err(invalid(&format!(
                    "restored {key} must be an array of strings"
                )));
            }
        }
        let text = |key: &str| meta_str(meta, key);
        let my_fleet = text("my_fleet");
        if !my_fleet.is_empty()
            && (parse_fleet(&my_fleet).is_none()
                || !is_salt(&text("my_salt"))
                || commitment(&my_fleet, &text("my_salt")) != text("my_commit"))
        {
            return Err(invalid("restored fleet does not match its commitment"));
        }
        let their_commit = text("their_commit");
        if !their_commit.is_empty() && !is_commit(&their_commit) {
            return Err(invalid("restored opponent commitment is malformed"));
        }
        let plies = meta_string_list(meta, "shots").len();

        match session.status.as_str() {
            STATUS_PENDING | STATUS_DECLINED => {
                let started = plies > 0
                    || !meta_string_list(meta, "answers").is_empty()
                    || [
                        "first_turn",
                        "my_color",
                        "turn",
                        "terminal",
                        "winner",
                        "their_fleet",
                    ]
                    .iter()
                    .any(|key| !text(key).is_empty());
                if started {
                    return Err(invalid(
                        "pending or declined session contains game progress",
                    ));
                }
                let challenger = session.initiator == session.identity_id;
                if session.status == STATUS_PENDING
                    && (if challenger {
                        my_fleet.is_empty()
                    } else {
                        their_commit.is_empty()
                    })
                {
                    return Err(invalid("pending session is missing its commitment"));
                }
            }
            STATUS_ACTIVE | STATUS_COMPLETED | STATUS_EXPIRED => {
                let first = text("first_turn");
                if first != session.identity_id && first != session.contact_hash {
                    return Err(invalid("restored first_turn is not a bound participant"));
                }
                let color = if first == session.identity_id {
                    "b"
                } else {
                    "w"
                };
                if text("my_color") != color {
                    return Err(invalid("restored my_color does not match first_turn"));
                }
                if their_commit.is_empty() {
                    return Err(invalid("restored session has no opponent commitment"));
                }
                let replayed = replay(session)
                    .map_err(|e| invalid(&format!("restored game is invalid: {e}")))?;
                let derived_matches = meta.get("move_count").and_then(JsonValue::as_u64)
                    == Some(plies as u64)
                    && text("last_move") == replayed.last_move
                    && text("my_board") == replayed.my_board
                    && text("their_board") == replayed.their_board;
                if !derived_matches {
                    return Err(invalid("restored metadata does not match the shots"));
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
                    (STATUS_COMPLETED, Some(sunk_winner)) => {
                        turn.is_empty()
                            && terminal == "win"
                            && reason == R_SUNK
                            && winner == *sunk_winner
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
                        "restored terminal metadata does not match the game",
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

    fn handle_challenge_in(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
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
        let commit = payload.get(KEY_COMMIT).and_then(value_as_str).unwrap_or("");
        session
            .metadata
            .insert("their_commit".into(), JsonValue::String(commit.into()));
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
        let first = payload.get(KEY_BLACK).and_then(value_as_str).unwrap_or("");
        if first != identity_id && first != sender_hash {
            return error_result(
                ERR_PROTOCOL_ERROR,
                "ACCEPT b is not one of the bound participants",
            );
        }
        let commit = payload.get(KEY_COMMIT).and_then(value_as_str).unwrap_or("");
        session
            .metadata
            .insert("their_commit".into(), JsonValue::String(commit.into()));
        if let Err(error) = Self::start_game(&mut session, first) {
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

    fn handle_challenge_out(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> OutgoingResult {
        let (fleet, salt) = match chosen_fleet(payload) {
            Ok(chosen) => chosen,
            Err(error) => return self.rejected(error),
        };
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
        Self::set_my_fleet(&mut session, &fleet, &salt);
        self.save_session(&session);
        let commit = meta_str(&session.metadata, "my_commit");
        OutgoingResult {
            payload: HashMap::from([(KEY_COMMIT.to_string(), rmpv::Value::String(commit.into()))]),
            fallback_text: self.render_fallback_inner(CMD_CHALLENGE, &HashMap::new()),
        }
    }

    fn handle_accept_out(
        &self,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> OutgoingResult {
        let Some(mut session) = self.get_session(session_id, identity_id) else {
            return self.rejected("Session not found");
        };
        let (fleet, salt) = match chosen_fleet(payload) {
            Ok(chosen) => chosen,
            Err(error) => return self.rejected(error),
        };
        if let Err(error) = SessionStateMachine::apply_command(&mut session, CMD_ACCEPT, false) {
            return self.rejected(error);
        }
        let first = if flip_responder_coin() {
            session.initiator.clone()
        } else {
            identity_id.to_string()
        };
        Self::set_my_fleet(&mut session, &fleet, &salt);
        if let Err(error) = Self::start_game(&mut session, &first) {
            return self.rejected(error);
        }
        self.save_session(&session);
        let commit = meta_str(&session.metadata, "my_commit");
        OutgoingResult {
            payload: HashMap::from([
                (KEY_BLACK.to_string(), rmpv::Value::String(first.into())),
                (KEY_COMMIT.to_string(), rmpv::Value::String(commit.into())),
            ]),
            fallback_text: self.render_fallback_inner(CMD_ACCEPT, &HashMap::new()),
        }
    }

    /// Check a local move intent and return the session with it applied and
    /// the wire payload.
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
        let ply = rmpv::Value::Integer(
            (meta_string_list(&session.metadata, "shots").len() as u64).into(),
        );
        let text = |value: &str| rmpv::Value::String(value.into());
        let meta = &session.metadata;
        match replayed.phase {
            PHASE_SHOOT => {
                let shot = payload
                    .get(KEY_SHOT)
                    .and_then(value_as_str)
                    .filter(|_| has_exact_keys(payload, &[KEY_SHOT]))
                    .ok_or("a shot must be exactly {s}")?;
                if parse_square(shot, SIZE).is_none() {
                    return Err(format!("Invalid square: {shot}"));
                }
                let (candidate, after) = Self::with_ply(session, shot, replayed.answer_due, None)?;
                let wire = HashMap::from([
                    (KEY_SHOT.to_string(), text(shot)),
                    (KEY_ANSWER.to_string(), text(replayed.answer_due)),
                    (KEY_PLY.to_string(), ply),
                    (KEY_TERMINAL.to_string(), text("")),
                ]);
                Ok((candidate, after, wire))
            }
            PHASE_CONCEDE | PHASE_REVEAL => {
                if !payload.is_empty() {
                    return Err(format!("{} takes an empty intent", replayed.phase));
                }
                let (fleet, salt) = (meta_str(meta, "my_fleet"), meta_str(meta, "my_salt"));
                let mut wire = HashMap::from([
                    (KEY_FLEET.to_string(), text(&fleet)),
                    (KEY_SALT.to_string(), text(&salt)),
                    (KEY_PLY.to_string(), ply),
                ]);
                let (candidate, after) = if replayed.phase == PHASE_CONCEDE {
                    wire.insert(KEY_ANSWER.into(), text(replayed.answer_due));
                    wire.insert(KEY_TERMINAL.into(), text(""));
                    Self::with_ply(session, "", replayed.answer_due, None)?
                } else {
                    wire.insert(KEY_TERMINAL.into(), text("win"));
                    wire.insert(KEY_WINNER.into(), text(identity_id));
                    Self::with_ply(session, "", "", None)?
                };
                Ok((candidate, after, wire))
            }
            _ => Err("The game is over".into()),
        }
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
        match command {
            CMD_CHALLENGE => format!("{prefix} Sent a challenge!"),
            CMD_ACCEPT => format!("{prefix} Challenge accepted"),
            CMD_DECLINE => format!("{prefix} Challenge declined"),
            CMD_MOVE => match (string(KEY_SHOT), string(KEY_TERMINAL)) {
                (Some(shot), _) => format!("{prefix} Fired at {shot}"),
                (None, Some("win")) => format!("{prefix} Fleet revealed (game won)"),
                _ => format!("{prefix} Fleet sunk"),
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

impl GameApp for BattleshipApp {
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
            CMD_CHALLENGE => {
                self.handle_challenge_in(session_id, payload, sender_hash, identity_id)
            }
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
            CMD_CHALLENGE => self.handle_challenge_out(session_id, payload, identity_id),
            CMD_ACCEPT => self.handle_accept_out(session_id, payload, identity_id),
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
            CMD_CHALLENGE | CMD_ACCEPT => {
                if let Err(error) = chosen_fleet(payload) {
                    return (false, Some(error));
                }
            }
            CMD_DECLINE | CMD_RESIGN | CMD_DRAW_OFFER | CMD_DRAW_ACCEPT | CMD_DRAW_DECLINE => {
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
                message: "session app/version does not match Battleship".into(),
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

    const ALICE_FLEET: &str = "a1h a2h a3h a4h a5h";
    const BOB_FLEET: &str = "a10h a9h a8h a7h a6h";
    const ALICE_SALT: &str = "00112233445566778899aabbccddeeff";
    const BOB_SALT: &str = "ffeeddccbbaa99887766554433221100";

    static COIN_LOCK: Mutex<()> = Mutex::new(());

    struct Pin(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl Drop for Pin {
        fn drop(&mut self) {
            force_coin(None);
        }
    }

    fn pin(challenger_first: bool) -> Pin {
        let guard = COIN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        force_coin(Some(challenger_first));
        Pin(guard)
    }

    fn text(value: &str) -> rmpv::Value {
        rmpv::Value::String(value.into())
    }

    fn fleet_intent(fleet: &str, salt: &str) -> HashMap<String, rmpv::Value> {
        HashMap::from([
            (KEY_FLEET.into(), text(fleet)),
            (KEY_SALT.into(), text(salt)),
        ])
    }

    fn shot(square: &str) -> HashMap<String, rmpv::Value> {
        HashMap::from([(KEY_SHOT.into(), text(square))])
    }

    fn other(identity: &str) -> &'static str {
        if identity == ALICE { BOB } else { ALICE }
    }

    fn meta(app: &BattleshipApp, identity: &str) -> HashMap<String, JsonValue> {
        app.get_session(SID, identity).unwrap().metadata
    }

    /// Alice challenges with her fleet and Bob accepts with his; the coin
    /// decides who shoots first.
    fn setup(challenger_first: bool) -> (BattleshipApp, Pin) {
        let coin = pin(challenger_first);
        let app = BattleshipApp::new();
        let challenge = app.handle_outgoing(
            SID,
            CMD_CHALLENGE,
            &fleet_intent(ALICE_FLEET, ALICE_SALT),
            ALICE,
        );
        assert_eq!(
            challenge.payload[KEY_COMMIT],
            text("c64fbc70ca15a8f7ab30eb4855d65df26150d34d7016b6a1e44f57396e0a8aeb")
        );
        app.bind_session_peer(SID, ALICE, BOB).unwrap();
        let received = app.handle_incoming(SID, CMD_CHALLENGE, &challenge.payload, ALICE, BOB);
        assert!(received.error.is_none(), "{:?}", received.error);
        let accept = app.handle_outgoing(SID, CMD_ACCEPT, &fleet_intent(BOB_FLEET, BOB_SALT), BOB);
        assert!(has_exact_keys(&accept.payload, &[KEY_BLACK, KEY_COMMIT]));
        let accepted = app.handle_incoming(SID, CMD_ACCEPT, &accept.payload, BOB, ALICE);
        assert!(accepted.error.is_none(), "{:?}", accepted.error);
        (app, coin)
    }

    fn send(
        app: &BattleshipApp,
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

    /// Bob's ship squares, ship by ship, so each ship sinks on its last square.
    const BOB_SQUARES: [&str; 17] = [
        "a10", "b10", "c10", "d10", "e10", "a9", "b9", "c9", "d9", "a8", "b8", "c8", "a7", "b7",
        "c7", "a6", "b6",
    ];
    /// Bob's shots, all misses on Alice's fleet.
    const BOB_MISSES: [&str; 16] = [
        "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "g1", "g2", "g3", "g4", "g5",
        "g6",
    ];

    /// Play Alice's winning game up to Bob's concession; returns the wire payloads.
    fn play_to_concession(app: &BattleshipApp) -> Vec<HashMap<String, rmpv::Value>> {
        let mut wires = Vec::new();
        for (index, square) in BOB_SQUARES.iter().enumerate() {
            wires.push(send(app, &shot(square), ALICE));
            if index < BOB_MISSES.len() {
                wires.push(send(app, &shot(BOB_MISSES[index]), BOB));
            }
        }
        assert_eq!(meta_str(&meta(app, BOB), "phase"), PHASE_CONCEDE);
        wires.push(send(app, &HashMap::new(), BOB));
        wires
    }

    #[test]
    fn fleets_are_strict() {
        let ships = parse_fleet(ALICE_FLEET).unwrap();
        assert_eq!(ships.iter().map(Vec::len).collect::<Vec<_>>(), FLEET);
        assert_eq!(ships[0], [0, 1, 2, 3, 4]);
        assert_eq!(parse_fleet("a1v b1v c1v d1v e1v").unwrap()[4], [4, 14]);
        for bad in [
            "",
            "a1h a2h a3h a4h",
            "a1h a2h a3h a4h a5h a6h",
            "a1h  a2h a3h a4h a5h",
            "a1h a1v a3h a4h a5h", // overlap
            "g1h a2h a3h a4h a5h", // off the board
            "a7v a2h a3h a4h a5h", // carrier from a7 up is off the board
            "a1x a2h a3h a4h a5h",
            "A1h a2h a3h a4h a5h",
            "a1h a2h a3h a4h a5h ",
        ] {
            assert!(parse_fleet(bad).is_none(), "accepted {bad:?}");
        }
        for _ in 0..50 {
            assert!(parse_fleet(&random_fleet()).is_some());
        }
        assert!(is_salt(ALICE_SALT));
        assert!(!is_salt("00112233445566778899AABBCCDDEEFF"));
        assert!(is_salt(&random_salt()));
    }

    #[test]
    fn a_whole_game_reveals_both_fleets() {
        let (app, _coin) = setup(true);
        for identity in [ALICE, BOB] {
            let m = meta(&app, identity);
            assert_eq!(meta_str(&m, "turn"), ALICE);
            assert_eq!(meta_str(&m, "first_turn"), ALICE);
            assert_eq!(meta_str(&m, "phase"), PHASE_SHOOT);
            assert_eq!(meta_string_list(&m, "legal_moves").len(), CELLS);
        }
        let first = send(&app, &shot("a10"), ALICE);
        assert!(has_exact_keys(
            &first,
            &[KEY_SHOT, KEY_ANSWER, KEY_PLY, KEY_TERMINAL]
        ));
        assert_eq!(first[KEY_ANSWER], text(""));
        let reply = send(&app, &shot("f1"), BOB);
        assert_eq!(reply[KEY_ANSWER], text(HIT));
        app.rollback_session(SID, ALICE, None);

        let (app, _coin) = {
            drop(_coin);
            setup(true)
        };
        let wires = play_to_concession(&app);
        // Bob's answers: hits, with a sinking on each ship's last square.
        let answers: Vec<&str> = wires
            .iter()
            .skip(1)
            .step_by(2)
            .map(|w| value_as_str(&w[KEY_ANSWER]).unwrap())
            .collect();
        assert_eq!(answers.iter().filter(|a| **a == SUNK).count(), 5);
        assert!(answers.iter().all(|a| *a != MISS));

        let concession = wires.last().unwrap();
        assert!(has_exact_keys(
            concession,
            &[KEY_ANSWER, KEY_FLEET, KEY_SALT, KEY_PLY, KEY_TERMINAL]
        ));
        assert_eq!(concession[KEY_FLEET], text(BOB_FLEET));
        assert_eq!(concession[KEY_PLY], rmpv::Value::from(33));
        let alice = meta(&app, ALICE);
        assert_eq!(meta_str(&alice, "phase"), PHASE_REVEAL);
        assert_eq!(meta_str(&alice, "their_fleet"), BOB_FLEET);
        assert_eq!(alice["their_sunk"], JsonValue::from(5));
        assert_eq!(app.get_session(SID, ALICE).unwrap().status, STATUS_ACTIVE);

        let last = send(&app, &HashMap::new(), ALICE);
        assert!(has_exact_keys(
            &last,
            &[KEY_FLEET, KEY_SALT, KEY_PLY, KEY_TERMINAL, KEY_WINNER]
        ));
        assert_eq!(last[KEY_WINNER], text(ALICE));
        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(session.status, STATUS_COMPLETED);
            assert_eq!(meta_str(&session.metadata, "winner"), ALICE);
            assert_eq!(meta_str(&session.metadata, "terminal_reason"), R_SUNK);
            assert_eq!(meta_str(&session.metadata, "turn"), "");
            BattleshipApp::new().upsert_session(session).unwrap();
        }
        let bob = meta(&app, BOB);
        assert_eq!(meta_str(&bob, "their_fleet"), ALICE_FLEET);
        assert_eq!(bob["my_sunk"], JsonValue::from(5));
        assert!(
            meta_str(&bob, "my_board")
                .starts_with("xxxxx_____xxxx______xxx_______xxx_______xx________")
        );
        assert!(
            !app.validate_outgoing_action(SID, CMD_MOVE, &shot("j1"), BOB)
                .0
        );
    }

    #[test]
    fn the_coin_can_let_the_responder_shoot_first() {
        let (app, _coin) = setup(false);
        let m = meta(&app, ALICE);
        assert_eq!(meta_str(&m, "turn"), BOB);
        assert_eq!(meta_str(&m, "my_color"), "w");
        assert!(
            !app.validate_outgoing_action(SID, CMD_MOVE, &shot("a1"), ALICE)
                .0
        );
        send(&app, &shot("a1"), BOB);
        let reply = send(&app, &shot("a10"), ALICE);
        assert_eq!(reply[KEY_ANSWER], text(HIT));
        assert_eq!(
            meta_str(&meta(&app, BOB), "their_board").chars().nth(90),
            Some('x')
        );
        assert_eq!(
            meta_str(&meta(&app, BOB), "my_board").chars().next(),
            Some('x')
        );
    }

    #[test]
    fn bad_intents_are_refused_before_anything_is_sent() {
        let (app, _coin) = setup(true);
        for bad in [
            HashMap::new(),
            shot("k1"),
            shot("a0"),
            fleet_intent(ALICE_FLEET, ALICE_SALT),
        ] {
            assert!(
                !app.validate_outgoing_action(SID, CMD_MOVE, &bad, ALICE).0,
                "{bad:?}"
            );
            assert!(
                app.handle_outgoing(SID, CMD_MOVE, &bad, ALICE)
                    .payload
                    .is_empty()
            );
        }
        assert!(
            !app.validate_outgoing_action(SID, CMD_MOVE, &shot("a1"), BOB)
                .0
        );
        send(&app, &shot("a1"), ALICE);
        send(&app, &shot("j1"), BOB);
        let (valid, message) = app.validate_outgoing_action(SID, CMD_MOVE, &shot("a1"), ALICE);
        assert!(!valid);
        assert!(message.unwrap().contains("already"));

        for bad in [
            fleet_intent("a1h", ALICE_SALT),
            fleet_intent(ALICE_FLEET, "xyz"),
            HashMap::from([(KEY_SALT.into(), text(ALICE_SALT))]),
            HashMap::from([("z".into(), text(""))]),
        ] {
            assert!(
                !app.validate_outgoing_action("1111111111111111", CMD_CHALLENGE, &bad, ALICE)
                    .0,
                "{bad:?}"
            );
            assert!(
                app.handle_outgoing("1111111111111111", CMD_CHALLENGE, &bad, ALICE)
                    .payload
                    .is_empty()
            );
        }
        // A challenge with no fleet picks one at random.
        let random = app.handle_outgoing("2222222222222222", CMD_CHALLENGE, &HashMap::new(), ALICE);
        assert!(is_commit(
            value_as_str(&random.payload[KEY_COMMIT]).unwrap()
        ));
        let m = app.get_session("2222222222222222", ALICE).unwrap().metadata;
        assert!(parse_fleet(&meta_str(&m, "my_fleet")).is_some());
    }

    fn wire(entries: &[(&str, rmpv::Value)]) -> HashMap<String, rmpv::Value> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn forged_moves_are_rejected_without_mutation() {
        let (app, _coin) = setup(true);
        let before = meta(&app, BOB);
        let n = |ply: u64| rmpv::Value::from(ply);
        let cases = [
            wire(&[
                (KEY_SHOT, text("a1")),
                (KEY_ANSWER, text("")),
                (KEY_PLY, n(1)),
                (KEY_TERMINAL, text("")),
            ]),
            wire(&[
                (KEY_SHOT, text("a1")),
                (KEY_ANSWER, text("h")),
                (KEY_PLY, n(0)),
                (KEY_TERMINAL, text("")),
            ]),
            wire(&[
                (KEY_SHOT, text("k1")),
                (KEY_ANSWER, text("")),
                (KEY_PLY, n(0)),
                (KEY_TERMINAL, text("")),
            ]),
            wire(&[
                (KEY_SHOT, text("a1")),
                (KEY_ANSWER, text("")),
                (KEY_PLY, n(0)),
                (KEY_TERMINAL, text("draw")),
            ]),
            wire(&[
                (KEY_SHOT, text("a1")),
                (KEY_PLY, n(0)),
                (KEY_TERMINAL, text("")),
            ]),
            wire(&[
                (KEY_SHOT, text("a1")),
                (KEY_ANSWER, text("")),
                (KEY_PLY, n(0)),
                (KEY_TERMINAL, text("")),
                ("z", text("")),
            ]),
            wire(&[
                (KEY_SHOT, text("a1")),
                (KEY_ANSWER, text("")),
                (KEY_PLY, rmpv::Value::Boolean(true)),
                (KEY_TERMINAL, text("")),
            ]),
            // Conceding at once, or revealing with nothing to answer.
            wire(&[
                (KEY_ANSWER, text("s")),
                (KEY_FLEET, text(ALICE_FLEET)),
                (KEY_SALT, text(ALICE_SALT)),
                (KEY_PLY, n(0)),
                (KEY_TERMINAL, text("")),
            ]),
            wire(&[
                (KEY_FLEET, text(ALICE_FLEET)),
                (KEY_SALT, text(ALICE_SALT)),
                (KEY_PLY, n(0)),
                (KEY_TERMINAL, text("win")),
                (KEY_WINNER, text(ALICE)),
            ]),
        ];
        for payload in &cases {
            let result = app.handle_incoming(SID, CMD_MOVE, payload, ALICE, BOB);
            assert!(result.error.is_some(), "accepted {payload:?}");
        }
        // Bob moving out of turn.
        let bob_shot = wire(&[
            (KEY_SHOT, text("a1")),
            (KEY_ANSWER, text("")),
            (KEY_PLY, n(0)),
            (KEY_TERMINAL, text("")),
        ]);
        assert!(
            app.handle_incoming(SID, CMD_MOVE, &bob_shot, BOB, ALICE)
                .error
                .is_some()
        );
        assert_eq!(meta(&app, BOB), before);

        // Junk challenges create nothing.
        for junk in [HashMap::new(), wire(&[(KEY_COMMIT, text("00"))])] {
            assert!(
                app.handle_incoming("3333333333333333", CMD_CHALLENGE, &junk, ALICE, BOB)
                    .error
                    .is_some()
            );
        }
        assert!(app.get_session("3333333333333333", BOB).is_none());
    }

    #[test]
    fn false_answers_are_caught_by_the_reveal() {
        let (app, _coin) = setup(true);
        send(&app, &shot("a10"), ALICE);
        // Bob claims a miss on Alice's a10 shot, which hit his carrier.
        let lie = wire(&[
            (KEY_SHOT, text("f1")),
            (KEY_ANSWER, text(MISS)),
            (KEY_PLY, rmpv::Value::from(1)),
            (KEY_TERMINAL, text("")),
        ]);
        let received = app.handle_incoming(SID, CMD_MOVE, &lie, BOB, ALICE);
        assert!(
            received.error.is_none(),
            "a lie can't be seen before the reveal"
        );

        // Once Bob's fleet is revealed, the lie no longer replays.
        let mut session = app.get_session(SID, ALICE).unwrap();
        session
            .metadata
            .insert("their_fleet".into(), JsonValue::String(BOB_FLEET.into()));
        session
            .metadata
            .insert("their_salt".into(), JsonValue::String(BOB_SALT.into()));
        let error = replay(&session).err().unwrap();
        assert!(error.contains("false"), "{error}");

        // A reveal that doesn't match the commitment is refused.
        session
            .metadata
            .insert("their_fleet".into(), JsonValue::String(ALICE_FLEET.into()));
        assert!(replay(&session).err().unwrap().contains("commitment"));
    }

    #[test]
    fn a_concession_must_be_true() {
        let (app, _coin) = setup(true);
        send(&app, &shot("a10"), ALICE);
        let concede = wire(&[
            (KEY_ANSWER, text(SUNK)),
            (KEY_FLEET, text(BOB_FLEET)),
            (KEY_SALT, text(BOB_SALT)),
            (KEY_PLY, rmpv::Value::from(1)),
            (KEY_TERMINAL, text("")),
        ]);
        let result = app.handle_incoming(SID, CMD_MOVE, &concede, BOB, ALICE);
        assert!(result.error.is_some(), "conceded with ships afloat");
        // And Bob's own client offers only shots while his fleet floats.
        assert!(
            !app.validate_outgoing_action(SID, CMD_MOVE, &HashMap::new(), BOB)
                .0
        );
    }

    #[test]
    fn resign_draw_and_decline() {
        let (app, _coin) = setup(true);
        app.handle_outgoing(SID, CMD_RESIGN, &HashMap::new(), ALICE);
        app.handle_incoming(SID, CMD_RESIGN, &HashMap::new(), ALICE, BOB);
        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(session.status, STATUS_COMPLETED);
            assert_eq!(meta_str(&session.metadata, "winner"), BOB);
            assert_eq!(meta_str(&session.metadata, "terminal_reason"), R_RESIGN);
            BattleshipApp::new().upsert_session(session).unwrap();
        }
        drop(_coin);

        let (app, _coin) = setup(true);
        app.handle_outgoing(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE);
        app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE, BOB);
        assert!(
            !app.validate_outgoing_action(SID, CMD_DRAW_ACCEPT, &HashMap::new(), ALICE)
                .0
        );
        assert!(
            app.handle_incoming(SID, CMD_DRAW_OFFER, &HashMap::new(), ALICE, BOB)
                .error
                .is_some()
        );
        assert!(
            app.validate_outgoing_action(SID, CMD_DRAW_ACCEPT, &HashMap::new(), BOB)
                .0
        );
        app.handle_outgoing(SID, CMD_DRAW_ACCEPT, &HashMap::new(), BOB);
        app.handle_incoming(SID, CMD_DRAW_ACCEPT, &HashMap::new(), BOB, ALICE);
        for identity in [ALICE, BOB] {
            let session = app.get_session(SID, identity).unwrap();
            assert_eq!(meta_str(&session.metadata, "terminal"), "draw");
            BattleshipApp::new().upsert_session(session).unwrap();
        }

        let fresh = BattleshipApp::new();
        let challenge = fresh.handle_outgoing(SID, CMD_CHALLENGE, &HashMap::new(), ALICE);
        fresh.handle_incoming(SID, CMD_CHALLENGE, &challenge.payload, ALICE, BOB);
        let out = fresh.handle_outgoing(SID, CMD_DECLINE, &HashMap::new(), BOB);
        assert_eq!(out.fallback_text, "[LRGP Battleship] Challenge declined");
        fresh.handle_incoming(SID, CMD_DECLINE, &HashMap::new(), BOB, ALICE);
        for identity in [ALICE, BOB] {
            let session = fresh.get_session(SID, identity).unwrap();
            assert_eq!(session.status, STATUS_DECLINED);
            BattleshipApp::new().upsert_session(session).unwrap();
        }
        let cancel =
            fresh.handle_outgoing("4444444444444444", CMD_CHALLENGE, &HashMap::new(), ALICE);
        assert!(!cancel.payload.is_empty());
        let out = fresh.handle_outgoing("4444444444444444", CMD_DECLINE, &HashMap::new(), ALICE);
        assert_eq!(out.fallback_text, "[LRGP Battleship] Challenge cancelled");
    }

    #[test]
    fn hydration_replays_and_rejects_tampering() {
        let (app, _coin) = setup(true);
        for square in ["a10", "f1", "b10"] {
            let mover = meta_str(&meta(&app, ALICE), "turn");
            send(&app, &shot(square), &mover);
        }
        for identity in [ALICE, BOB] {
            let record = app.get_session(SID, identity).unwrap();
            let fresh = BattleshipApp::new();
            fresh.upsert_session(record.clone()).unwrap();
            assert_eq!(
                fresh.get_session(SID, identity).unwrap().metadata,
                record.metadata
            );
        }
        let record = app.get_session(SID, BOB).unwrap();
        let tamper = |key: &str, value: JsonValue| {
            let mut copy = record.clone();
            copy.metadata.insert(key.into(), value);
            BattleshipApp::new().upsert_session(copy)
        };
        assert!(tamper("my_fleet", JsonValue::String(ALICE_FLEET.into())).is_err());
        assert!(tamper("my_salt", JsonValue::String(ALICE_SALT.into())).is_err());
        assert!(tamper("turn", JsonValue::String(ALICE.into())).is_err());
        assert!(tamper("my_color", JsonValue::String("b".into())).is_err());
        assert!(tamper("move_count", JsonValue::from(1)).is_err());
        assert!(tamper("my_board", JsonValue::String("_".repeat(CELLS))).is_err());
        assert!(tamper("answers", JsonValue::from(vec!["", "m", "m"])).is_err());
        assert!(tamper("shots", JsonValue::from(vec!["a10", "f1", "a10"])).is_err());
        assert!(tamper("their_commit", JsonValue::String("".into())).is_err());
        assert!(tamper("terminal", JsonValue::String("win".into())).is_err());

        let mut bad_app = record.clone();
        bad_app.app_id = "chess".into();
        assert!(BattleshipApp::new().upsert_session(bad_app).is_err());
        let mut expired = record;
        expired.last_action_at = 0.0;
        let fresh = BattleshipApp::new();
        fresh.upsert_session(expired).unwrap();
        assert_eq!(fresh.get_session(SID, BOB).unwrap().status, STATUS_EXPIRED);
        assert!(fresh.authorize_incoming(SID, CMD_MOVE, ALICE, BOB).is_err());
    }

    #[test]
    fn authorization_and_rollback() {
        let (app, _coin) = setup(true);
        assert!(app.authorize_incoming(SID, CMD_MOVE, BOB, ALICE).is_ok());
        assert!(app.authorize_incoming(SID, CMD_MOVE, "cc", ALICE).is_err());
        assert!(
            app.authorize_incoming("ffffffffffffffff", CMD_CHALLENGE, BOB, ALICE)
                .is_ok()
        );
        assert!(
            app.authorize_incoming("ffffffffffffffff", CMD_MOVE, BOB, ALICE)
                .is_err()
        );
        assert!(app.bind_session_peer(SID, ALICE, "cc").is_err());

        let snap = app.snapshot_session(SID, ALICE);
        send(&app, &shot("a10"), ALICE);
        app.rollback_session(SID, ALICE, snap);
        assert_eq!(meta(&app, ALICE)["move_count"], JsonValue::from(0));
        assert_eq!(app.list_session_records(Some(ALICE)).len(), 1);
        assert!(app.remove_session(SID, ALICE));
    }

    #[test]
    fn manifest_and_fallback() {
        let app = BattleshipApp::new();
        let manifest = app.manifest();
        assert_eq!(manifest.app_id, APP_ID);
        assert_eq!(manifest.session_type, SESSION_TURN_BASED);
        assert_eq!(manifest.preferred_delivery[CMD_RESIGN], "direct");
        assert_eq!(
            app.render_fallback(CMD_MOVE, &shot("e5")),
            "[LRGP Battleship] Fired at e5"
        );
        let reveal = wire(&[(KEY_TERMINAL, text("win"))]);
        assert_eq!(
            app.render_fallback(CMD_MOVE, &reveal),
            "[LRGP Battleship] Fleet revealed (game won)"
        );
        assert_eq!(
            app.render_fallback(CMD_MOVE, &HashMap::new()),
            "[LRGP Battleship] Fleet sunk"
        );
    }

    #[test]
    fn worst_case_envelopes_fit_the_budget() {
        let fleet = "a10h a9h a8h a7h a6h";
        let payload = wire(&[
            (KEY_FLEET, text(fleet)),
            (KEY_SALT, text(BOB_SALT)),
            (KEY_PLY, rmpv::Value::from(199)),
            (KEY_TERMINAL, text("win")),
            (KEY_WINNER, text(ALICE)),
        ]);
        let envelope = pack_envelope(APP_ID, 1, CMD_MOVE, SID, Some(payload), None).unwrap();
        let size = validate_envelope_size(&envelope).expect("envelope size OK");
        assert!(size <= 180, "worst-case reveal envelope {size} bytes");
        let accept = wire(&[
            (KEY_BLACK, text(ALICE)),
            (KEY_COMMIT, text(&commitment(fleet, BOB_SALT))),
        ]);
        let envelope = pack_envelope(APP_ID, 1, CMD_ACCEPT, SID, Some(accept), None).unwrap();
        assert!(validate_envelope_size(&envelope).is_ok());
    }
}
