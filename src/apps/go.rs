//! Go on a 9x9 board with area scoring, positional superko, and 7.5 komi.
//! Two passes in a row end the game and every stone left on the board counts
//! as alive, so players capture dead stones before passing. Wire moves are
//! `{m, n, x}` with `m` a point such as `e5` or `pass`; SPEC.md Appendix G.2
//! defines the rules.

use std::collections::{HashMap, HashSet};

use serde_json::Value as JsonValue;

use super::placement::{
    self, EMPTY, Outcome, PlacementRules, Side, parse_square, render_grid, square_name,
};

pub const APP_ID: &str = "go";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 9;
pub const PASS: &str = "pass";
/// White's compensation for moving second, in half points.
pub const KOMI_HALF_POINTS: usize = 15;

const CELLS: usize = SIZE * SIZE;
const R_COUNT: &str = "cnt";

placement::placement_coin!();

#[derive(Clone, Debug)]
pub(crate) struct GoPosition {
    cells: [u8; CELLS],
    ply: usize,
    /// Passes in a row; two end the game.
    passes: usize,
    /// Every board seen so far, for positional superko.
    seen: HashSet<[u8; CELLS]>,
    /// Stones each side has captured: Black's, then White's.
    captures: [usize; 2],
    outcome: Outcome,
}

fn neighbors(index: usize) -> impl Iterator<Item = usize> {
    let (file, rank) = (index % SIZE, index / SIZE);
    [
        (file > 0).then(|| index - 1),
        (file + 1 < SIZE).then(|| index + 1),
        (rank > 0).then(|| index - SIZE),
        (rank + 1 < SIZE).then(|| index + SIZE),
    ]
    .into_iter()
    .flatten()
}

/// The chain through `index` and whether it has a liberty.
fn chain(cells: &[u8; CELLS], index: usize) -> (Vec<usize>, bool) {
    let stone = cells[index];
    let mut stones = vec![index];
    let mut seen = [false; CELLS];
    seen[index] = true;
    let mut free = false;
    let mut next = 0;
    while next < stones.len() {
        for neighbor in neighbors(stones[next]) {
            if cells[neighbor] == EMPTY {
                free = true;
            } else if cells[neighbor] == stone && !seen[neighbor] {
                seen[neighbor] = true;
                stones.push(neighbor);
            }
        }
        next += 1;
    }
    (stones, free)
}

/// The board after `side` plays on `index`, and the stones captured, or why
/// the move is illegal (superko is checked by the caller).
fn place(cells: &[u8; CELLS], index: usize, side: Side) -> Result<([u8; CELLS], usize), String> {
    if cells[index] != EMPTY {
        return Err(format!("Point {} is occupied", square_name(index, SIZE)));
    }
    let mut board = *cells;
    board[index] = side.stone();
    let mut captured = 0;
    for neighbor in neighbors(index) {
        if board[neighbor] == side.other().stone() {
            let (stones, free) = chain(&board, neighbor);
            if !free {
                captured += stones.len();
                for stone in stones {
                    board[stone] = EMPTY;
                }
            }
        }
    }
    if !chain(&board, index).1 {
        return Err(format!("{} would be suicide", square_name(index, SIZE)));
    }
    Ok((board, captured))
}

/// Area score per side: stones plus empty regions that reach only that
/// side's stones.
fn area(cells: &[u8; CELLS]) -> [usize; 2] {
    let mut score = [0, 0];
    let mut seen = [false; CELLS];
    for index in 0..CELLS {
        match cells[index] {
            b'b' => score[0] += 1,
            b'w' => score[1] += 1,
            _ if !seen[index] => {
                let mut region = vec![index];
                seen[index] = true;
                let (mut black, mut white) = (false, false);
                let mut next = 0;
                while next < region.len() {
                    for neighbor in neighbors(region[next]) {
                        match cells[neighbor] {
                            b'b' => black = true,
                            b'w' => white = true,
                            _ if !seen[neighbor] => {
                                seen[neighbor] = true;
                                region.push(neighbor);
                            }
                            _ => {}
                        }
                    }
                    next += 1;
                }
                match (black, white) {
                    (true, false) => score[0] += region.len(),
                    (false, true) => score[1] += region.len(),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    score
}

pub(crate) struct GoRules;

impl GoRules {
    /// The board after `side` plays on `index`, if legal including superko.
    fn try_place(
        position: &GoPosition,
        index: usize,
        side: Side,
    ) -> Result<([u8; CELLS], usize), String> {
        let (board, captured) = place(&position.cells, index, side)?;
        if position.seen.contains(&board) {
            return Err(format!(
                "{} would repeat an earlier position",
                square_name(index, SIZE)
            ));
        }
        Ok((board, captured))
    }
}

impl PlacementRules for GoRules {
    type Position = GoPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Go";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = PASS;
    #[cfg(test)]
    const MAX_PLIES: usize = 2000;
    const CAN_DRAW: bool = false;
    const WIN_REASON: &'static str = R_COUNT;
    const DRAW_REASON: &'static str = R_COUNT;

    fn start() -> GoPosition {
        let cells = [EMPTY; CELLS];
        GoPosition {
            cells,
            ply: 0,
            passes: 0,
            seen: HashSet::from([cells]),
            captures: [0, 0],
            outcome: Outcome::Ongoing,
        }
    }

    fn side_to_move(position: &GoPosition) -> Side {
        if position.ply & 1 == 0 {
            Side::Black
        } else {
            Side::White
        }
    }

    fn is_canonical(notation: &str) -> bool {
        notation == PASS || parse_square(notation, SIZE).is_some()
    }

    fn legal_moves(position: &GoPosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        let side = Self::side_to_move(position);
        (0..CELLS)
            .filter(|&index| Self::try_place(position, index, side).is_ok())
            .map(|index| square_name(index, SIZE))
            .chain([PASS.to_string()])
            .collect()
    }

    fn play(position: &mut GoPosition, notation: &str) -> Result<(), String> {
        let side = Self::side_to_move(position);
        if notation == PASS {
            position.passes += 1;
        } else {
            let index = parse_square(notation, SIZE)
                .ok_or_else(|| format!("Invalid move notation: {notation}"))?;
            let (board, captured) = Self::try_place(position, index, side)?;
            position.cells = board;
            position.seen.insert(board);
            position.captures[if side == Side::Black { 0 } else { 1 }] += captured;
            position.passes = 0;
        }
        position.ply += 1;
        if position.passes == 2 {
            let [black, white] = area(&position.cells);
            position.outcome = if 2 * black > 2 * white + KOMI_HALF_POINTS {
                Outcome::Win(Side::Black)
            } else {
                Outcome::Win(Side::White)
            };
        }
        Ok(())
    }

    fn outcome(position: &GoPosition) -> Outcome {
        position.outcome
    }

    fn render(position: &GoPosition) -> String {
        render_grid(&position.cells, SIZE)
    }

    fn extra_metadata(position: &GoPosition, metadata: &mut HashMap<String, JsonValue>) {
        let [black, white] = area(&position.cells);
        metadata.insert("black_count".into(), JsonValue::from(black));
        metadata.insert("white_count".into(), JsonValue::from(white));
        metadata.insert(
            "komi".into(),
            JsonValue::from(KOMI_HALF_POINTS as f64 / 2.0),
        );
        metadata.insert(
            "black_captures".into(),
            JsonValue::from(position.captures[0]),
        );
        metadata.insert(
            "white_captures".into(),
            JsonValue::from(position.captures[1]),
        );
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Go LRGP application.
    GoApp,
    GoRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn game(moves: &[&str]) -> Result<GoPosition, String> {
        let moves: Vec<String> = moves.iter().map(|m| m.to_string()).collect();
        replay::<GoRules>(&moves)
    }

    fn at(position: &GoPosition, name: &str) -> u8 {
        position.cells[parse_square(name, SIZE).unwrap()]
    }

    #[test]
    fn notation_is_strict() {
        assert!(GoRules::is_canonical("a1"));
        assert!(GoRules::is_canonical("i9"));
        assert!(GoRules::is_canonical("pass"));
        for bad in ["", "j1", "a10", "PASS", "pass ", "a01", "E5"] {
            assert!(!GoRules::is_canonical(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn surrounded_stones_are_captured() {
        // White's a1 stone loses its last liberty when Black plays b1.
        let position = game(&["a2", "a1", "b1"]).unwrap();
        assert_eq!(at(&position, "a1"), EMPTY);
        assert_eq!(position.captures, [1, 0]);

        // A two-stone chain in the middle.
        let position = game(&[
            "d5", "e5", "e4", "f5", "f4", "a1", "e6", "a2", "f6", "a3", "g5",
        ])
        .unwrap();
        assert_eq!(at(&position, "e5"), EMPTY);
        assert_eq!(at(&position, "f5"), EMPTY);
        assert_eq!(position.captures, [2, 0]);
    }

    #[test]
    fn suicide_is_illegal_unless_it_captures() {
        // Black surrounds a1; White may not play there.
        let mut position = game(&["a2", "e5", "b1"]).unwrap();
        let error = play_checked::<GoRules>(&mut position, "a1").unwrap_err();
        assert!(error.contains("suicide"), "{error}");
        assert!(!GoRules::legal_moves(&position).contains(&"a1".to_string()));

        // Filling the last liberty is fine when it captures.
        let position = game(&["c1", "b1", "b2", "a2", "a3", "e5", "a1"]).unwrap();
        assert_eq!(at(&position, "a1"), b'b');
        assert_eq!(at(&position, "a2"), EMPTY);
        assert_eq!(at(&position, "b1"), EMPTY);
    }

    #[test]
    fn a_ko_cannot_be_retaken_at_once() {
        // Black a2 b1 b3 and White c1 c3 d2 make a ko at b2 and c2: Black
        // plays c2, and White takes it back by playing b2.
        let moves = ["b1", "c1", "a2", "d2", "b3", "c3", "c2", "b2"];
        let position = game(&moves).unwrap();
        // White b2 captured Black c2.
        assert_eq!(at(&position, "c2"), EMPTY);
        let mut retake = position.clone();
        let error = play_checked::<GoRules>(&mut retake, "c2").unwrap_err();
        assert!(error.contains("repeat"), "{error}");
        assert!(!GoRules::legal_moves(&position).contains(&"c2".to_string()));
        // After a move elsewhere Black may retake.
        let mut later = position;
        play_checked::<GoRules>(&mut later, "g7").unwrap();
        play_checked::<GoRules>(&mut later, "g8").unwrap();
        play_checked::<GoRules>(&mut later, "c2").unwrap();
        assert_eq!(at(&later, "b2"), EMPTY);
    }

    #[test]
    fn two_passes_end_the_game_by_area() {
        let position = game(&["pass", "pass"]).unwrap();
        // Empty board: 0 to 0, so komi gives White the game.
        assert_eq!(GoRules::outcome(&position), Outcome::Win(Side::White));
        assert!(GoRules::legal_moves(&position).is_empty());

        // One pass and a move do not end it.
        let position = game(&["pass", "e5", "pass"]).unwrap();
        assert_eq!(GoRules::outcome(&position), Outcome::Ongoing);

        // A lone Black stone owns the whole board: 81 to 0.
        let position = game(&["e5", "pass", "pass"]).unwrap();
        assert_eq!(area(&position.cells), [81, 0]);
        assert_eq!(GoRules::outcome(&position), Outcome::Win(Side::Black));
    }

    #[test]
    fn area_counts_stones_and_surrounded_points() {
        let mut cells = [EMPTY; CELLS];
        // A Black wall on file d and a White wall on file f.
        for rank in 0..SIZE {
            cells[rank * SIZE + 3] = b'b';
            cells[rank * SIZE + 5] = b'w';
        }
        // a–c (27) plus file d (9) for Black, file f plus g–i for White;
        // file e touches both and counts for nobody.
        assert_eq!(area(&cells), [36, 36]);
    }

    #[test]
    fn metadata_reports_score_and_captures() {
        let position = game(&["a2", "a1", "b1"]).unwrap();
        let mut metadata = HashMap::new();
        GoRules::extra_metadata(&position, &mut metadata);
        assert_eq!(metadata["black_count"], JsonValue::from(81));
        assert_eq!(metadata["white_count"], JsonValue::from(0));
        assert_eq!(metadata["komi"], JsonValue::from(7.5));
        assert_eq!(metadata["black_captures"], JsonValue::from(1));
    }

    #[test]
    fn pass_is_listed_last() {
        let legal = GoRules::legal_moves(&GoRules::start());
        assert_eq!(legal.len(), CELLS + 1);
        assert_eq!(legal.first().map(String::as_str), Some("a1"));
        assert_eq!(legal.last().map(String::as_str), Some(PASS));
    }

    placement::placement_lifecycle_tests!(crate::apps::go::GoRules);
}
