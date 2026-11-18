//! Pente on a 19x19 board: Gomoku with captures. Flanking exactly two
//! opposing stones in a line removes them; five in a row or five captured
//! pairs wins. Wire moves are `{m, n, x}` with `m` a square such as `j10`;
//! SPEC.md Appendix G.7 defines the rules.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use super::placement::{
    self, EMPTY, Outcome, PlacementRules, Side, parse_square, render_grid, square_name,
};

pub const APP_ID: &str = "pente";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 19;
/// Captured pairs that win the game.
pub const CAPTURES_TO_WIN: usize = 5;

const CELLS: usize = SIZE * SIZE;
const R_FIVE: &str = "5ir";
const R_CAPTURES: &str = "cap";
const R_FULL: &str = "ful";

/// `(file, rank)` line directions; each is also walked backwards.
const LINES: [(isize, isize); 4] = [(1, 0), (0, 1), (1, 1), (1, -1)];

placement::placement_coin!();

#[derive(Clone, Debug)]
pub(crate) struct PentePosition {
    cells: [u8; CELLS],
    ply: usize,
    /// Pairs captured: Black's, then White's.
    captures: [usize; 2],
    outcome: Outcome,
    reason: &'static str,
}

/// The square `steps` away from `index` along `(df, dr)`, if on the board.
fn walk(index: usize, (df, dr): (isize, isize), steps: isize) -> Option<usize> {
    let file = (index % SIZE) as isize + df * steps;
    let rank = (index / SIZE) as isize + dr * steps;
    let range = 0..SIZE as isize;
    (range.contains(&file) && range.contains(&rank)).then(|| rank as usize * SIZE + file as usize)
}

fn line_length(cells: &[u8; CELLS], index: usize, direction: (isize, isize)) -> usize {
    let stone = cells[index];
    let mut length = 1;
    for sign in [1, -1] {
        let mut steps = 1;
        while walk(index, direction, sign * steps).is_some_and(|s| cells[s] == stone) {
            length += 1;
            steps += 1;
        }
    }
    length
}

pub(crate) struct PenteRules;

impl PlacementRules for PenteRules {
    type Position = PentePosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Pente";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "s19";
    #[cfg(test)]
    const MAX_PLIES: usize = 4 * CELLS;
    const CAN_DRAW: bool = true;
    const WIN_REASON: &'static str = R_FIVE;
    const DRAW_REASON: &'static str = R_FULL;

    fn start() -> PentePosition {
        PentePosition {
            cells: [EMPTY; CELLS],
            ply: 0,
            captures: [0, 0],
            outcome: Outcome::Ongoing,
            reason: "",
        }
    }

    fn side_to_move(position: &PentePosition) -> Side {
        if position.ply & 1 == 0 {
            Side::Black
        } else {
            Side::White
        }
    }

    fn is_canonical(notation: &str) -> bool {
        parse_square(notation, SIZE).is_some()
    }

    fn legal_moves(position: &PentePosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        (0..CELLS)
            .filter(|&index| position.cells[index] == EMPTY)
            .map(|index| square_name(index, SIZE))
            .collect()
    }

    fn play(position: &mut PentePosition, notation: &str) -> Result<(), String> {
        let index = parse_square(notation, SIZE)
            .ok_or_else(|| format!("Invalid move notation: {notation}"))?;
        if position.cells[index] != EMPTY {
            return Err(format!("Square {notation} is occupied"));
        }
        let side = Self::side_to_move(position);
        let (own, theirs) = (side.stone(), side.other().stone());
        position.cells[index] = own;
        position.ply += 1;

        let me = if side == Side::Black { 0 } else { 1 };
        for (df, dr) in LINES {
            for direction in [(df, dr), (-df, -dr)] {
                let span: Vec<Option<usize>> =
                    (1..=3).map(|steps| walk(index, direction, steps)).collect();
                if let [Some(a), Some(b), Some(c)] = span[..] {
                    if position.cells[a] == theirs
                        && position.cells[b] == theirs
                        && position.cells[c] == own
                    {
                        position.cells[a] = EMPTY;
                        position.cells[b] = EMPTY;
                        position.captures[me] += 1;
                    }
                }
            }
        }

        if LINES
            .iter()
            .any(|&d| line_length(&position.cells, index, d) >= 5)
        {
            position.outcome = Outcome::Win(side);
            position.reason = R_FIVE;
        } else if position.captures[me] >= CAPTURES_TO_WIN {
            position.outcome = Outcome::Win(side);
            position.reason = R_CAPTURES;
        } else if !position.cells.contains(&EMPTY) {
            position.outcome = Outcome::Draw;
            position.reason = R_FULL;
        }
        Ok(())
    }

    fn outcome(position: &PentePosition) -> Outcome {
        position.outcome
    }

    fn reason(position: &PentePosition) -> &'static str {
        position.reason
    }

    fn render(position: &PentePosition) -> String {
        render_grid(&position.cells, SIZE)
    }

    fn extra_metadata(position: &PentePosition, metadata: &mut HashMap<String, JsonValue>) {
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
    /// The built-in Pente LRGP application.
    PenteApp,
    PenteRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn game(moves: &[&str]) -> PentePosition {
        let moves: Vec<String> = moves.iter().map(|m| m.to_string()).collect();
        replay::<PenteRules>(&moves).unwrap()
    }

    fn at(position: &PentePosition, name: &str) -> u8 {
        position.cells[parse_square(name, SIZE).unwrap()]
    }

    #[test]
    fn notation_is_strict() {
        assert!(PenteRules::is_canonical("a1"));
        assert!(PenteRules::is_canonical("s19"));
        for bad in ["", "t1", "a20", "a01", "J10", "pass"] {
            assert!(!PenteRules::is_canonical(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn flanking_a_pair_captures_it() {
        // Black j10, White k10 l10, Black m10 captures both.
        let position = game(&["j10", "k10", "a1", "l10", "m10"]);
        assert_eq!(at(&position, "k10"), EMPTY);
        assert_eq!(at(&position, "l10"), EMPTY);
        assert_eq!(position.captures, [1, 0]);
        // Diagonally too.
        let position = game(&["e5", "f6", "a1", "g7", "h8"]);
        assert_eq!(position.captures, [1, 0]);
    }

    #[test]
    fn one_or_three_stones_are_not_captured_and_moving_in_is_safe() {
        let position = game(&["j10", "k10", "a1", "l10", "a2", "m10", "n10"]);
        assert_eq!(position.captures, [0, 0], "three in a row is safe");
        // White's l10 completes W W between Black stones, which is safe.
        let position = game(&["j10", "k10", "m10", "l10"]);
        assert_eq!(at(&position, "l10"), b'w');
        assert_eq!(position.captures, [0, 0]);
    }

    #[test]
    fn five_in_a_row_or_five_pairs_wins() {
        let position = game(&["a1", "a19", "b1", "b19", "c1", "c19", "d1", "d19", "e1"]);
        assert_eq!(PenteRules::outcome(&position), Outcome::Win(Side::Black));
        assert_eq!(PenteRules::reason(&position), R_FIVE);

        let mut position = PenteRules::start();
        position.captures = [4, 0];
        position.cells[parse_square("j10", SIZE).unwrap()] = b'b';
        for name in ["k10", "l10"] {
            position.cells[parse_square(name, SIZE).unwrap()] = b'w';
        }
        play_checked::<PenteRules>(&mut position, "m10").unwrap();
        assert_eq!(PenteRules::outcome(&position), Outcome::Win(Side::Black));
        assert_eq!(PenteRules::reason(&position), R_CAPTURES);
        assert!(PenteRules::legal_moves(&position).is_empty());
    }

    #[test]
    fn metadata_counts_captured_pairs() {
        let position = game(&["j10", "k10", "a1", "l10", "m10"]);
        let mut metadata = HashMap::new();
        PenteRules::extra_metadata(&position, &mut metadata);
        assert_eq!(metadata["black_captures"], JsonValue::from(1));
        assert_eq!(metadata["white_captures"], JsonValue::from(0));
    }

    placement::placement_lifecycle_tests!(crate::apps::pente::PenteRules);
}
