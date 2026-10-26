//! Nine Men's Morris. Players place nine pieces each, then move them along
//! the lines, flying anywhere once down to three. Three in a line (a mill)
//! removes an opponent piece. Moves are `d2`, `d2-d3`, or either with a
//! removal such as `d2-d3xg7`; SPEC.md Appendix G.4 defines the rules.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use super::placement::{self, EMPTY, Outcome, PlacementRules, Side};

pub const APP_ID: &str = "morris";
pub const APP_VERSION: u32 = 1;
pub const PIECES: usize = 9;
/// The 24 points, in canonical (byte) order.
pub const POINTS: [&str; 24] = [
    "a1", "a4", "a7", "b2", "b4", "b6", "c3", "c4", "c5", "d1", "d2", "d3", "d5", "d6", "d7", "e3",
    "e4", "e5", "f2", "f4", "f6", "g1", "g4", "g7",
];
/// Plies in a row without a removal, after which the game is drawn.
pub const QUIET_PLIES: usize = 100;

const R_PIECES: &str = "np";
const R_BLOCKED: &str = "nm";
const R_REPETITION: &str = "3fr";
const R_QUIET: &str = "50m";

/// The sixteen mills, as indices into `POINTS`.
const MILLS: [[usize; 3]; 16] = [
    [0, 9, 21],   // a1-d1-g1
    [3, 10, 18],  // b2-d2-f2
    [6, 11, 15],  // c3-d3-e3
    [1, 4, 7],    // a4-b4-c4
    [16, 19, 22], // e4-f4-g4
    [8, 12, 17],  // c5-d5-e5
    [5, 13, 20],  // b6-d6-f6
    [2, 14, 23],  // a7-d7-g7
    [0, 1, 2],    // a1-a4-a7
    [3, 4, 5],    // b2-b4-b6
    [6, 7, 8],    // c3-c4-c5
    [9, 10, 11],  // d1-d2-d3
    [12, 13, 14], // d5-d6-d7
    [15, 16, 17], // e3-e4-e5
    [18, 19, 20], // f2-f4-f6
    [21, 22, 23], // g1-g4-g7
];

placement::placement_coin!();

fn point(name: &str) -> Option<usize> {
    POINTS.iter().position(|p| *p == name)
}

/// Points joined to `index` by a line.
fn adjacent(index: usize) -> Vec<usize> {
    let mut found = Vec::new();
    for mill in MILLS {
        for pair in [[mill[0], mill[1]], [mill[1], mill[2]]] {
            if pair[0] == index {
                found.push(pair[1]);
            } else if pair[1] == index {
                found.push(pair[0]);
            }
        }
    }
    found
}

fn in_mill(cells: &[u8; 24], index: usize) -> bool {
    let stone = cells[index];
    stone != EMPTY
        && MILLS
            .iter()
            .any(|mill| mill.contains(&index) && mill.iter().all(|&p| cells[p] == stone))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Move {
    from: Option<usize>,
    to: usize,
    remove: Option<usize>,
}

fn parse_move(notation: &str) -> Option<Move> {
    let (body, remove) = match notation.split_once('x') {
        Some((body, removed)) => (body, Some(point(removed)?)),
        None => (notation, None),
    };
    let (from, to) = match body.split_once('-') {
        Some((from, to)) => (Some(point(from)?), point(to)?),
        None => (None, point(body)?),
    };
    Some(Move { from, to, remove })
}

fn notation(mv: &Move) -> String {
    let mut text = String::new();
    if let Some(from) = mv.from {
        text.push_str(POINTS[from]);
        text.push('-');
    }
    text.push_str(POINTS[mv.to]);
    if let Some(removed) = mv.remove {
        text.push('x');
        text.push_str(POINTS[removed]);
    }
    text
}

#[derive(Clone, Debug)]
pub(crate) struct MorrisPosition {
    cells: [u8; 24],
    /// Pieces still to place: Black's, then White's.
    in_hand: [usize; 2],
    ply: usize,
    /// Plies since the last removal.
    quiet: usize,
    seen: HashMap<Vec<u8>, usize>,
    outcome: Outcome,
    reason: &'static str,
}

fn slot(side: Side) -> usize {
    match side {
        Side::Black => 0,
        Side::White => 1,
    }
}

impl MorrisPosition {
    fn on_board(&self, side: Side) -> usize {
        self.cells.iter().filter(|&&c| c == side.stone()).count()
    }

    fn key(&self) -> Vec<u8> {
        let mut key = self.cells.to_vec();
        key.extend([
            self.in_hand[0] as u8,
            self.in_hand[1] as u8,
            (self.ply & 1) as u8,
        ]);
        key
    }

    /// Every legal move for `side`, unsorted.
    fn moves(&self, side: Side) -> Vec<Move> {
        let own = side.stone();
        let empty: Vec<usize> = (0..24).filter(|&p| self.cells[p] == EMPTY).collect();
        let mut steps = Vec::new();
        if self.in_hand[slot(side)] > 0 {
            steps.extend(empty.iter().map(|&to| (None, to)));
        } else {
            let flying = self.on_board(side) == 3;
            for from in (0..24).filter(|&p| self.cells[p] == own) {
                let targets = if flying {
                    empty.clone()
                } else {
                    adjacent(from)
                };
                for to in targets {
                    if self.cells[to] == EMPTY {
                        steps.push((Some(from), to));
                    }
                }
            }
        }
        let theirs = side.other().stone();
        let mut found = Vec::new();
        for (from, to) in steps {
            let mut after = self.cells;
            if let Some(from) = from {
                after[from] = EMPTY;
            }
            after[to] = own;
            if !in_mill(&after, to) {
                found.push(Move {
                    from,
                    to,
                    remove: None,
                });
                continue;
            }
            let pieces: Vec<usize> = (0..24).filter(|&p| after[p] == theirs).collect();
            let free: Vec<usize> = pieces
                .iter()
                .copied()
                .filter(|&p| !in_mill(&after, p))
                .collect();
            for remove in if free.is_empty() { pieces } else { free } {
                found.push(Move {
                    from,
                    to,
                    remove: Some(remove),
                });
            }
        }
        found
    }
}

pub(crate) struct MorrisRules;

impl PlacementRules for MorrisRules {
    type Position = MorrisPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Nine Men's Morris";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "a1-a4xg7";
    #[cfg(test)]
    const MAX_PLIES: usize = 2000;
    const CAN_DRAW: bool = true;
    const WIN_REASON: &'static str = R_PIECES;
    const DRAW_REASON: &'static str = R_REPETITION;

    fn start() -> MorrisPosition {
        let mut position = MorrisPosition {
            cells: [EMPTY; 24],
            in_hand: [PIECES; 2],
            ply: 0,
            quiet: 0,
            seen: HashMap::new(),
            outcome: Outcome::Ongoing,
            reason: "",
        };
        position.seen.insert(position.key(), 1);
        position
    }

    fn side_to_move(position: &MorrisPosition) -> Side {
        if position.ply & 1 == 0 {
            Side::Black
        } else {
            Side::White
        }
    }

    fn is_canonical(text: &str) -> bool {
        parse_move(text).is_some_and(|mv| notation(&mv) == text)
    }

    fn legal_moves(position: &MorrisPosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        let mut moves: Vec<String> = position
            .moves(Self::side_to_move(position))
            .iter()
            .map(notation)
            .collect();
        moves.sort_unstable();
        moves
    }

    fn play(position: &mut MorrisPosition, text: &str) -> Result<(), String> {
        let side = Self::side_to_move(position);
        let mv = parse_move(text).ok_or_else(|| format!("Invalid move notation: {text}"))?;
        if !position.moves(side).contains(&mv) {
            return Err(format!("Illegal move: {text}"));
        }
        match mv.from {
            Some(from) => position.cells[from] = EMPTY,
            None => position.in_hand[slot(side)] -= 1,
        }
        position.cells[mv.to] = side.stone();
        position.ply += 1;
        position.quiet += 1;
        if let Some(removed) = mv.remove {
            position.cells[removed] = EMPTY;
            position.quiet = 0;
        }

        let opponent = side.other();
        if position.on_board(opponent) + position.in_hand[slot(opponent)] < 3 {
            position.outcome = Outcome::Win(side);
            position.reason = R_PIECES;
        } else if position.moves(opponent).is_empty() {
            position.outcome = Outcome::Win(side);
            position.reason = R_BLOCKED;
        } else {
            let count = position.seen.entry(position.key()).or_insert(0);
            *count += 1;
            if *count >= 3 {
                position.outcome = Outcome::Draw;
                position.reason = R_REPETITION;
            } else if position.quiet >= QUIET_PLIES {
                position.outcome = Outcome::Draw;
                position.reason = R_QUIET;
            }
        }
        Ok(())
    }

    fn outcome(position: &MorrisPosition) -> Outcome {
        position.outcome
    }

    fn reason(position: &MorrisPosition) -> &'static str {
        position.reason
    }

    fn render(position: &MorrisPosition) -> String {
        position.cells.iter().map(|&c| c as char).collect()
    }

    fn extra_metadata(position: &MorrisPosition, metadata: &mut HashMap<String, JsonValue>) {
        metadata.insert("black_in_hand".into(), JsonValue::from(position.in_hand[0]));
        metadata.insert("white_in_hand".into(), JsonValue::from(position.in_hand[1]));
        metadata.insert(
            "black_count".into(),
            JsonValue::from(position.on_board(Side::Black)),
        );
        metadata.insert(
            "white_count".into(),
            JsonValue::from(position.on_board(Side::White)),
        );
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Nine Men's Morris LRGP application.
    MorrisApp,
    MorrisRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn game(moves: &[&str]) -> Result<MorrisPosition, String> {
        let moves: Vec<String> = moves.iter().map(|m| m.to_string()).collect();
        replay::<MorrisRules>(&moves)
    }

    /// A position with pieces on `black` and `white`, nothing in hand, and
    /// Black to move.
    fn setup(black: &[&str], white: &[&str]) -> MorrisPosition {
        let mut position = MorrisRules::start();
        position.in_hand = [0, 0];
        for name in black {
            position.cells[point(name).unwrap()] = b'b';
        }
        for name in white {
            position.cells[point(name).unwrap()] = b'w';
        }
        position.ply = 18;
        position.seen.clear();
        position
    }

    #[test]
    fn notation_is_strict() {
        for good in ["a1", "d2xg7", "a1-a4", "a1-a4xg7"] {
            assert!(MorrisRules::is_canonical(good), "rejected {good:?}");
        }
        for bad in [
            "",
            "a2",
            "d4",
            "a1-",
            "a1-a4x",
            "A1",
            "a1a4",
            "a1-a4xg7x",
            "a1 ",
        ] {
            assert!(!MorrisRules::is_canonical(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn every_point_is_in_two_mills() {
        for (index, name) in POINTS.iter().enumerate() {
            let count = MILLS.iter().filter(|mill| mill.contains(&index)).count();
            assert_eq!(count, 2, "{name}");
        }
    }

    #[test]
    fn every_point_has_two_to_four_neighbours() {
        for (index, name) in POINTS.iter().enumerate() {
            let count = adjacent(index).len();
            assert!((2..=4).contains(&count), "{name} has {count}");
        }
        assert_eq!(adjacent(point("d2").unwrap()).len(), 4);
        assert_eq!(adjacent(point("a1").unwrap()).len(), 2);
    }

    #[test]
    fn placing_a_mill_removes_a_piece() {
        // Black a1 a4, White d6 d7, then Black's a7 closes a1-a4-a7.
        let mut position = game(&["a1", "d6", "a4", "d7"]).unwrap();
        let legal = MorrisRules::legal_moves(&position);
        assert!(legal.contains(&"a7xd6".to_string()));
        assert!(!legal.contains(&"a7".to_string()), "a mill must remove");
        assert!(play_checked::<MorrisRules>(&mut position.clone(), "a7").is_err());
        play_checked::<MorrisRules>(&mut position, "a7xd7").unwrap();
        assert_eq!(
            MorrisRules::render(&position).as_bytes()[point("d7").unwrap()],
            EMPTY
        );
        assert_eq!(position.in_hand, [6, 7]);
    }

    #[test]
    fn pieces_in_mills_are_safe_unless_all_are() {
        // White's e3-e4-e5 is a mill and b6 is not: only b6 can go.
        let mut position = setup(&["a1", "a4", "d7", "g1"], &["e3", "e4", "e5", "b6"]);
        let legal = MorrisRules::legal_moves(&position);
        assert!(legal.contains(&"d7-a7xb6".to_string()));
        assert!(!legal.contains(&"d7-a7xe4".to_string()));
        assert!(!legal.contains(&"d7-a7".to_string()));
        // With b6 gone, the mill's pieces can be taken.
        position.cells[point("b6").unwrap()] = EMPTY;
        let legal = MorrisRules::legal_moves(&position);
        assert!(legal.contains(&"d7-a7xe4".to_string()));
    }

    #[test]
    fn three_pieces_fly() {
        let position = setup(&["a1", "d2", "g7"], &["b4", "c4", "e5", "f6"]);
        let legal = MorrisRules::legal_moves(&position);
        assert!(
            legal.contains(&"a1-e3".to_string()),
            "flying to any empty point"
        );
        let mut white = setup(&["a1", "d2", "g7", "g1"], &["b4", "c4", "e5", "f6"]);
        white.ply = 19;
        assert!(!MorrisRules::legal_moves(&white).contains(&"b4-e3".to_string()));
    }

    #[test]
    fn two_pieces_or_no_move_lose() {
        let mut position = setup(&["a1", "a4", "d7", "d2"], &["b6", "f6", "g1"]);
        play_checked::<MorrisRules>(&mut position, "d7-a7xb6").unwrap();
        assert_eq!(MorrisRules::outcome(&position), Outcome::Win(Side::Black));
        assert_eq!(MorrisRules::reason(&position), R_PIECES);
        assert!(MorrisRules::legal_moves(&position).is_empty());

        // White's a1, a4, a7, and d1 have nowhere to go once g1 is taken.
        let mut position = setup(
            &["b4", "d2", "d7", "g4", "b6", "c5"],
            &["a1", "a4", "a7", "d1"],
        );
        play_checked::<MorrisRules>(&mut position, "g4-g1").unwrap();
        assert_eq!(MorrisRules::outcome(&position), Outcome::Win(Side::Black));
        assert_eq!(MorrisRules::reason(&position), R_BLOCKED);
    }

    #[test]
    fn repetition_and_quiet_moves_draw() {
        let mut position = setup(&["a1", "d2", "g7", "c3"], &["b4", "c4", "e5", "f6"]);
        position.seen.insert(position.key(), 1);
        for (index, step) in [
            "a1-d1", "b4-b2", "d1-a1", "b2-b4", "a1-d1", "b4-b2", "d1-a1", "b2-b4",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(
                MorrisRules::outcome(&position),
                Outcome::Ongoing,
                "before {index}"
            );
            play_checked::<MorrisRules>(&mut position, step).unwrap();
        }
        assert_eq!(MorrisRules::outcome(&position), Outcome::Draw);
        assert_eq!(MorrisRules::reason(&position), R_REPETITION);

        let mut position = setup(&["a1", "d2", "g7", "c3"], &["b4", "c4", "e5", "f6"]);
        position.quiet = QUIET_PLIES - 1;
        play_checked::<MorrisRules>(&mut position, "a1-d1").unwrap();
        assert_eq!(MorrisRules::reason(&position), R_QUIET);
    }

    placement::placement_lifecycle_tests!(crate::apps::morris::MorrisRules);
}
