//! Quoridor on a 9x9 board with ten walls each. A move steps the pawn
//! (`e2`) or places a two-square wall (`c3h`, `c3v`); a wall may never cut
//! a pawn off from its goal. First to the far side wins, and a position seen
//! for the third time draws. SPEC.md Appendix G.3 defines the rules.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use super::placement::{self, EMPTY, Outcome, PlacementRules, Side, parse_square, square_name};

pub const APP_ID: &str = "quoridor";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 9;
pub const WALLS_EACH: usize = 10;

const CELLS: usize = SIZE * SIZE;
/// Wall centres: the corner above and right of squares `a1`–`h8`.
const SLOTS: usize = (SIZE - 1) * (SIZE - 1);
const R_GOAL: &str = "gol";
const R_REPETITION: &str = "3fr";

placement::placement_coin!();

#[derive(Clone, Debug)]
pub(crate) struct QuoridorPosition {
    /// Pawn squares: Black's, then White's.
    pawns: [usize; 2],
    /// Walls left: Black's, then White's.
    walls_left: [usize; 2],
    horizontal: [bool; SLOTS],
    vertical: [bool; SLOTS],
    ply: usize,
    /// How often each position has occurred, for threefold repetition.
    seen: HashMap<Vec<u8>, usize>,
    outcome: Outcome,
}

fn slot(file: usize, rank: usize) -> usize {
    rank * (SIZE - 1) + file
}

fn index_of(side: Side) -> usize {
    match side {
        Side::Black => 0,
        Side::White => 1,
    }
}

fn goal_rank(side: Side) -> usize {
    match side {
        Side::Black => SIZE - 1,
        Side::White => 0,
    }
}

impl QuoridorPosition {
    /// Whether a wall blocks the step from `from` by `(df, dr)` (one of the
    /// four orthogonal steps), or the step leaves the board.
    fn blocked(&self, from: usize, (df, dr): (isize, isize)) -> bool {
        let (file, rank) = ((from % SIZE) as isize, (from / SIZE) as isize);
        let (to_file, to_rank) = (file + df, rank + dr);
        let range = 0..SIZE as isize;
        if !range.contains(&to_file) || !range.contains(&to_rank) {
            return true;
        }
        let wall = |walls: &[bool; SLOTS], f: isize, r: isize| {
            (0..(SIZE - 1) as isize).contains(&f)
                && (0..(SIZE - 1) as isize).contains(&r)
                && walls[slot(f as usize, r as usize)]
        };
        if dr != 0 {
            // Crossing between ranks `low` and `low + 1` on this file.
            let low = rank.min(to_rank);
            wall(&self.horizontal, file, low) || wall(&self.horizontal, file - 1, low)
        } else {
            let low = file.min(to_file);
            wall(&self.vertical, low, rank) || wall(&self.vertical, low, rank - 1)
        }
    }

    fn step(from: usize, (df, dr): (isize, isize)) -> usize {
        (from as isize + df + dr * SIZE as isize) as usize
    }

    /// Whether `side`'s pawn can still reach its goal, ignoring pawns.
    fn has_path(&self, side: Side) -> bool {
        let mut seen = [false; CELLS];
        let start = self.pawns[index_of(side)];
        let mut queue = vec![start];
        seen[start] = true;
        let mut next = 0;
        while next < queue.len() {
            let square = queue[next];
            if square / SIZE == goal_rank(side) {
                return true;
            }
            for direction in DIRECTIONS {
                if !self.blocked(square, direction) {
                    let to = Self::step(square, direction);
                    if !seen[to] {
                        seen[to] = true;
                        queue.push(to);
                    }
                }
            }
            next += 1;
        }
        false
    }

    /// Squares the pawn of the side to move can reach this ply.
    fn pawn_moves(&self, side: Side) -> Vec<usize> {
        let (own, theirs) = (
            self.pawns[index_of(side)],
            self.pawns[index_of(side.other())],
        );
        let mut found = Vec::new();
        for direction in DIRECTIONS {
            if self.blocked(own, direction) {
                continue;
            }
            let near = Self::step(own, direction);
            if near != theirs {
                found.push(near);
            } else if !self.blocked(near, direction) {
                found.push(Self::step(near, direction));
            } else {
                // Straight jump blocked: step diagonally around the pawn.
                for side_step in DIRECTIONS {
                    if side_step.0 * direction.0 + side_step.1 * direction.1 == 0
                        && !self.blocked(near, side_step)
                    {
                        found.push(Self::step(near, side_step));
                    }
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    /// Whether a wall fits at `(file, rank)`, ignoring the path rule.
    fn wall_fits(&self, file: usize, rank: usize, horizontal: bool) -> bool {
        let at = slot(file, rank);
        if self.horizontal[at] || self.vertical[at] {
            return false;
        }
        let walls = if horizontal {
            &self.horizontal
        } else {
            &self.vertical
        };
        let (df, dr) = if horizontal { (1, 0) } else { (0, 1) };
        let along = |sign: isize| {
            let (f, r) = (file as isize + df * sign, rank as isize + dr * sign);
            (0..(SIZE - 1) as isize).contains(&f)
                && (0..(SIZE - 1) as isize).contains(&r)
                && walls[slot(f as usize, r as usize)]
        };
        !along(1) && !along(-1)
    }

    fn key(&self) -> Vec<u8> {
        let mut key = vec![
            self.pawns[0] as u8,
            self.pawns[1] as u8,
            (self.ply & 1) as u8,
        ];
        key.extend(self.horizontal.iter().map(|&w| w as u8));
        key.extend(self.vertical.iter().map(|&w| w as u8));
        key
    }
}

const DIRECTIONS: [(isize, isize); 4] = [(0, 1), (0, -1), (1, 0), (-1, 0)];

/// A wall token: a square in `a1`–`h8` and `h` or `v`.
fn parse_wall(notation: &str) -> Option<(usize, usize, bool)> {
    let (square, kind) = notation.split_at(notation.len().checked_sub(1)?);
    let horizontal = match kind {
        "h" => true,
        "v" => false,
        _ => return None,
    };
    let index = parse_square(square, SIZE)?;
    let (file, rank) = (index % SIZE, index / SIZE);
    (file < SIZE - 1 && rank < SIZE - 1).then_some((file, rank, horizontal))
}

pub(crate) struct QuoridorRules;

impl QuoridorRules {
    fn wall_moves(position: &QuoridorPosition, side: Side) -> Vec<String> {
        if position.walls_left[index_of(side)] == 0 {
            return Vec::new();
        }
        let mut found = Vec::new();
        for horizontal in [true, false] {
            for rank in 0..SIZE - 1 {
                for file in 0..SIZE - 1 {
                    if position.wall_fits(file, rank, horizontal) {
                        let mut trial = position.clone();
                        let walls = if horizontal {
                            &mut trial.horizontal
                        } else {
                            &mut trial.vertical
                        };
                        walls[slot(file, rank)] = true;
                        if trial.has_path(Side::Black) && trial.has_path(Side::White) {
                            let kind = if horizontal { "h" } else { "v" };
                            found.push(format!("{}{kind}", square_name(rank * SIZE + file, SIZE)));
                        }
                    }
                }
            }
        }
        found
    }
}

impl PlacementRules for QuoridorRules {
    type Position = QuoridorPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Quoridor";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "h8v";
    #[cfg(test)]
    const MAX_PLIES: usize = 2000;
    const CAN_DRAW: bool = true;
    const WIN_REASON: &'static str = R_GOAL;
    const DRAW_REASON: &'static str = R_REPETITION;

    fn start() -> QuoridorPosition {
        let mut position = QuoridorPosition {
            pawns: [4, CELLS - 5],
            walls_left: [WALLS_EACH; 2],
            horizontal: [false; SLOTS],
            vertical: [false; SLOTS],
            ply: 0,
            seen: HashMap::new(),
            outcome: Outcome::Ongoing,
        };
        position.seen.insert(position.key(), 1);
        position
    }

    fn side_to_move(position: &QuoridorPosition) -> Side {
        if position.ply & 1 == 0 {
            Side::Black
        } else {
            Side::White
        }
    }

    fn is_canonical(notation: &str) -> bool {
        parse_square(notation, SIZE).is_some() || parse_wall(notation).is_some()
    }

    fn legal_moves(position: &QuoridorPosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        let side = Self::side_to_move(position);
        let mut moves: Vec<String> = position
            .pawn_moves(side)
            .into_iter()
            .map(|square| square_name(square, SIZE))
            .chain(Self::wall_moves(position, side))
            .collect();
        moves.sort_unstable();
        moves
    }

    fn play(position: &mut QuoridorPosition, notation: &str) -> Result<(), String> {
        let side = Self::side_to_move(position);
        let me = index_of(side);
        if let Some(square) = parse_square(notation, SIZE) {
            if !position.pawn_moves(side).contains(&square) {
                return Err(format!("The pawn cannot move to {notation}"));
            }
            position.pawns[me] = square;
        } else {
            let (file, rank, horizontal) =
                parse_wall(notation).ok_or_else(|| format!("Invalid move notation: {notation}"))?;
            if position.walls_left[me] == 0 {
                return Err("No walls left".into());
            }
            if !position.wall_fits(file, rank, horizontal) {
                return Err(format!("Wall {notation} overlaps or crosses another"));
            }
            let walls = if horizontal {
                &mut position.horizontal
            } else {
                &mut position.vertical
            };
            walls[slot(file, rank)] = true;
            if !position.has_path(Side::Black) || !position.has_path(Side::White) {
                let walls = if horizontal {
                    &mut position.horizontal
                } else {
                    &mut position.vertical
                };
                walls[slot(file, rank)] = false;
                return Err(format!(
                    "Wall {notation} would cut a pawn off from its goal"
                ));
            }
            position.walls_left[me] -= 1;
        }
        position.ply += 1;
        if position.pawns[me] / SIZE == goal_rank(side) {
            position.outcome = Outcome::Win(side);
            return Ok(());
        }
        let count = position.seen.entry(position.key()).or_insert(0);
        *count += 1;
        if *count >= 3 {
            position.outcome = Outcome::Draw;
        }
        Ok(())
    }

    fn outcome(position: &QuoridorPosition) -> Outcome {
        position.outcome
    }

    fn render(position: &QuoridorPosition) -> String {
        // A 17x17 picture from the top-left: squares on even rows and
        // columns, wall grooves between them.
        const SIDE: usize = 2 * SIZE - 1;
        let mut picture = [b' '; SIDE * SIDE];
        let mut put = |file2: usize, rank2: usize, value: u8| {
            picture[(SIDE - 1 - rank2) * SIDE + file2] = value;
        };
        for square in 0..CELLS {
            let stone = if square == position.pawns[0] {
                Side::Black.stone()
            } else if square == position.pawns[1] {
                Side::White.stone()
            } else {
                EMPTY
            };
            put(2 * (square % SIZE), 2 * (square / SIZE), stone);
        }
        for rank in 0..SIZE - 1 {
            for file in 0..SIZE - 1 {
                let (f2, r2) = (2 * file + 1, 2 * rank + 1);
                if position.horizontal[slot(file, rank)] {
                    put(f2 - 1, r2, b'-');
                    put(f2, r2, b'-');
                    put(f2 + 1, r2, b'-');
                }
                if position.vertical[slot(file, rank)] {
                    put(f2, r2 - 1, b'|');
                    put(f2, r2, b'|');
                    put(f2, r2 + 1, b'|');
                }
            }
        }
        String::from_utf8(picture.to_vec()).unwrap_or_default()
    }

    fn extra_metadata(position: &QuoridorPosition, metadata: &mut HashMap<String, JsonValue>) {
        let mut walls = Vec::new();
        for (kind, set) in [("h", &position.horizontal), ("v", &position.vertical)] {
            for rank in 0..SIZE - 1 {
                for file in 0..SIZE - 1 {
                    if set[slot(file, rank)] {
                        walls.push(format!("{}{kind}", square_name(rank * SIZE + file, SIZE)));
                    }
                }
            }
        }
        walls.sort_unstable();
        metadata.insert(
            "walls".into(),
            JsonValue::Array(walls.into_iter().map(JsonValue::String).collect()),
        );
        metadata.insert(
            "black_walls".into(),
            JsonValue::from(position.walls_left[0]),
        );
        metadata.insert(
            "white_walls".into(),
            JsonValue::from(position.walls_left[1]),
        );
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Quoridor LRGP application.
    QuoridorApp,
    QuoridorRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn game(moves: &[&str]) -> Result<QuoridorPosition, String> {
        let moves: Vec<String> = moves.iter().map(|m| m.to_string()).collect();
        replay::<QuoridorRules>(&moves)
    }

    fn pawn_moves(position: &QuoridorPosition) -> Vec<String> {
        QuoridorRules::legal_moves(position)
            .into_iter()
            .filter(|m| parse_square(m, SIZE).is_some())
            .collect()
    }

    #[test]
    fn notation_is_strict() {
        for good in ["a1", "i9", "a1h", "h8v", "e3h"] {
            assert!(QuoridorRules::is_canonical(good), "rejected {good:?}");
        }
        for bad in ["", "i1h", "a9v", "a1x", "A1h", "a1hv", "j1", "e3 h"] {
            assert!(!QuoridorRules::is_canonical(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_opening_lists_pawn_steps_and_every_wall() {
        let start = QuoridorRules::start();
        let board = QuoridorRules::render(&start);
        assert_eq!(board.len(), 17 * 17);
        assert_eq!(board.find('w'), Some(8));
        assert_eq!(board.rfind('b'), Some(16 * 17 + 8));
        let walled = QuoridorRules::render(&game(&["e1h", "a8v"]).unwrap());
        assert_eq!(&walled[15 * 17 + 8..15 * 17 + 11], "---");
        for row in 0..3 {
            assert_eq!(walled.as_bytes()[row * 17 + 1], b'|');
        }
        assert_eq!(pawn_moves(&start), ["d1", "e2", "f1"]);
        let legal = QuoridorRules::legal_moves(&start);
        assert_eq!(legal.len(), 3 + 2 * SLOTS);
        assert_eq!(legal[0], "a1h");
        let mut sorted = legal.clone();
        sorted.sort();
        assert_eq!(legal, sorted);
    }

    #[test]
    fn walls_block_steps_and_may_not_overlap_or_cross() {
        // e1h lies between e1/f1 and e2/f2, so Black can't step up from e1.
        let position = game(&["e1h"]).unwrap();
        let mut black = position.clone();
        black.ply = 0;
        assert_eq!(pawn_moves(&black), ["d1", "f1"]);
        for clash in ["e1h", "d1h", "f1h", "e1v"] {
            let mut position = game(&["e1h"]).unwrap();
            assert!(
                play_checked::<QuoridorRules>(&mut position, clash).is_err(),
                "{clash}"
            );
        }
        // Next to it, and the other way round elsewhere, are fine.
        let mut position = game(&["e1h"]).unwrap();
        play_checked::<QuoridorRules>(&mut position, "c1h").unwrap();
        play_checked::<QuoridorRules>(&mut position, "d1v").unwrap();
        assert_eq!(position.walls_left, [8, 9]);
    }

    #[test]
    fn a_wall_may_not_shut_a_pawn_in() {
        let sealed = QuoridorPosition {
            horizontal: {
                let mut h = [false; SLOTS];
                h[slot(0, 0)] = true;
                h[slot(2, 0)] = true;
                h[slot(4, 0)] = true;
                h[slot(6, 0)] = true;
                h
            },
            ..QuoridorRules::start()
        };
        // Rank 1 is walled off from a1 to h1; i1 is the last way up.
        let mut trial = sealed.clone();
        let error = play_checked::<QuoridorRules>(&mut trial, "h1v").unwrap_err();
        assert!(error.contains("cut"), "{error}");
        assert!(!QuoridorRules::legal_moves(&sealed).contains(&"h1v".to_string()));
    }

    #[test]
    fn pawns_jump_and_side_step() {
        let mut position = QuoridorRules::start();
        position.pawns = [
            parse_square("e4", SIZE).unwrap(),
            parse_square("e5", SIZE).unwrap(),
        ];
        assert_eq!(pawn_moves(&position), ["d4", "e3", "e6", "f4"]);
        // A wall behind White turns the jump into side steps.
        position.horizontal[slot(4, 4)] = true;
        assert_eq!(pawn_moves(&position), ["d4", "d5", "e3", "f4", "f5"]);
        // At the edge there is nothing behind, so side steps again.
        position.horizontal[slot(4, 4)] = false;
        position.pawns = [
            parse_square("e8", SIZE).unwrap(),
            parse_square("e9", SIZE).unwrap(),
        ];
        assert_eq!(pawn_moves(&position), ["d8", "d9", "e7", "f8", "f9"]);
    }

    #[test]
    fn reaching_the_far_rank_wins() {
        let mut position = QuoridorRules::start();
        position.pawns = [
            parse_square("a8", SIZE).unwrap(),
            parse_square("i8", SIZE).unwrap(),
        ];
        play_checked::<QuoridorRules>(&mut position, "a9").unwrap();
        assert_eq!(QuoridorRules::outcome(&position), Outcome::Win(Side::Black));
        assert!(QuoridorRules::legal_moves(&position).is_empty());
    }

    #[test]
    fn a_third_repetition_draws() {
        let shuffle = ["d1", "d9", "e1", "e9", "d1", "d9", "e1"];
        let position = game(&shuffle).unwrap();
        assert_eq!(QuoridorRules::outcome(&position), Outcome::Ongoing);
        let position = game(&[&shuffle[..], &["e9"]].concat()).unwrap();
        assert_eq!(QuoridorRules::outcome(&position), Outcome::Draw);
    }

    #[test]
    fn walls_run_out() {
        let mut position = QuoridorRules::start();
        position.walls_left = [0, 10];
        assert!(play_checked::<QuoridorRules>(&mut position, "a1h").is_err());
        assert_eq!(QuoridorRules::legal_moves(&position).len(), 3);
        let mut metadata = HashMap::new();
        QuoridorRules::extra_metadata(&game(&["e1h", "a8v"]).unwrap(), &mut metadata);
        assert_eq!(metadata["walls"], serde_json::json!(["a8v", "e1h"]));
        assert_eq!(metadata["black_walls"], JsonValue::from(9));
    }

    placement::placement_lifecycle_tests!(crate::apps::quoridor::QuoridorRules);
}
