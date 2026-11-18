//! Ultimate Tic-Tac-Toe: nine Tic-Tac-Toe boards in a 3x3 grid on one 9x9
//! board. The cell a player takes picks the small board the opponent plays
//! in next; three small boards won in a row wins. Wire moves are `{m, n, x}`
//! with `m` a square such as `e5`; SPEC.md Appendix G defines the rules.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use super::placement::{
    self, EMPTY, Outcome, PlacementRules, Side, parse_square, render_grid, square_name,
};

pub const APP_ID: &str = "ultimate_ttt";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 9;

const CELLS: usize = SIZE * SIZE;
const R_LINE: &str = "3ib";
const R_FULL: &str = "ful";
/// A small board that ended without a line; it counts for nobody.
const DEAD: u8 = b'd';

/// Lines through a 3x3 grid, as `file + 3 * rank` indices.
const LINES: [[usize; 3]; 8] = [
    [0, 1, 2],
    [3, 4, 5],
    [6, 7, 8],
    [0, 3, 6],
    [1, 4, 7],
    [2, 5, 8],
    [0, 4, 8],
    [2, 4, 6],
];

placement::placement_coin!();

#[derive(Clone, Debug)]
pub(crate) struct UltimatePosition {
    cells: [u8; CELLS],
    /// Each small board's state (`_`, `b`, `w`, or `d`), `file + 3 * rank`.
    boards: [u8; 9],
    /// The small board the next move must be in, if it is still open.
    target: Option<usize>,
    ply: usize,
    outcome: Outcome,
}

/// The small board holding square `index`.
fn board_of(index: usize) -> usize {
    (index % SIZE) / 3 + 3 * ((index / SIZE) / 3)
}

/// The position of square `index` inside its small board.
fn cell_of(index: usize) -> usize {
    (index % SIZE) % 3 + 3 * ((index / SIZE) % 3)
}

/// The squares of small board `board`, in square order.
fn squares_of(board: usize) -> impl Iterator<Item = usize> {
    let (file, rank) = (3 * (board % 3), 3 * (board / 3));
    (0..9).map(move |cell| (rank + cell / 3) * SIZE + file + cell % 3)
}

fn has_line(grid: &[u8; 9], mark: u8) -> bool {
    LINES
        .iter()
        .any(|line| line.iter().all(|&i| grid[i] == mark))
}

impl UltimatePosition {
    fn open(&self, index: usize) -> bool {
        self.cells[index] == EMPTY
            && self.boards[board_of(index)] == EMPTY
            && self.target.is_none_or(|board| board == board_of(index))
    }
}

pub(crate) struct UltimateRules;

impl PlacementRules for UltimateRules {
    type Position = UltimatePosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Ultimate Tic-Tac-Toe";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "i9";
    #[cfg(test)]
    const MAX_PLIES: usize = CELLS;
    const CAN_DRAW: bool = true;
    const WIN_REASON: &'static str = R_LINE;
    const DRAW_REASON: &'static str = R_FULL;

    fn start() -> UltimatePosition {
        UltimatePosition {
            cells: [EMPTY; CELLS],
            boards: [EMPTY; 9],
            target: None,
            ply: 0,
            outcome: Outcome::Ongoing,
        }
    }

    fn side_to_move(position: &UltimatePosition) -> Side {
        if position.ply & 1 == 0 {
            Side::Black
        } else {
            Side::White
        }
    }

    fn is_canonical(notation: &str) -> bool {
        parse_square(notation, SIZE).is_some()
    }

    fn legal_moves(position: &UltimatePosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        (0..CELLS)
            .filter(|&index| position.open(index))
            .map(|index| square_name(index, SIZE))
            .collect()
    }

    fn play(position: &mut UltimatePosition, notation: &str) -> Result<(), String> {
        let index = parse_square(notation, SIZE)
            .ok_or_else(|| format!("Invalid move notation: {notation}"))?;
        if position.cells[index] != EMPTY {
            return Err(format!("Square {notation} is occupied"));
        }
        if position.boards[board_of(index)] != EMPTY {
            return Err(format!("The small board holding {notation} is finished"));
        }
        if !position.open(index) {
            return Err(format!(
                "{notation} is not in the small board you were sent to"
            ));
        }
        let side = Self::side_to_move(position);
        position.cells[index] = side.stone();
        position.ply += 1;

        let board = board_of(index);
        let mut grid = [EMPTY; 9];
        for (cell, square) in squares_of(board).enumerate() {
            grid[cell] = position.cells[square];
        }
        if has_line(&grid, side.stone()) {
            position.boards[board] = side.stone();
        } else if !grid.contains(&EMPTY) {
            position.boards[board] = DEAD;
        }

        let next = cell_of(index);
        position.target = (position.boards[next] == EMPTY).then_some(next);
        if has_line(&position.boards, side.stone()) {
            position.outcome = Outcome::Win(side);
        } else if !position.boards.contains(&EMPTY) {
            position.outcome = Outcome::Draw;
        }
        Ok(())
    }

    fn outcome(position: &UltimatePosition) -> Outcome {
        position.outcome
    }

    fn render(position: &UltimatePosition) -> String {
        render_grid(&position.cells, SIZE)
    }

    fn extra_metadata(position: &UltimatePosition, metadata: &mut HashMap<String, JsonValue>) {
        // Small boards from the top row down, like the board itself.
        let boards: String = [6, 7, 8, 3, 4, 5, 0, 1, 2]
            .iter()
            .map(|&board| position.boards[board] as char)
            .collect();
        metadata.insert("boards".into(), JsonValue::String(boards));
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Ultimate Tic-Tac-Toe LRGP application.
    UltimateTicTacToeApp,
    UltimateRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn sq(name: &str) -> usize {
        parse_square(name, SIZE).unwrap()
    }

    fn boards(position: &UltimatePosition) -> String {
        let mut metadata = HashMap::new();
        UltimateRules::extra_metadata(position, &mut metadata);
        metadata["boards"].as_str().unwrap().to_string()
    }

    /// A position with `black` and `white` stones placed, small-board states
    /// `states`, no target, and Black to move.
    fn setup(black: &[&str], white: &[&str], states: &[u8; 9]) -> UltimatePosition {
        assert_eq!(black.len(), white.len());
        let mut position = UltimateRules::start();
        for name in black {
            position.cells[sq(name)] = b'b';
        }
        for name in white {
            position.cells[sq(name)] = b'w';
        }
        position.boards = *states;
        position.ply = black.len() * 2;
        position
    }

    fn legal_boards(position: &UltimatePosition) -> Vec<usize> {
        let mut found: Vec<usize> = UltimateRules::legal_moves(position)
            .iter()
            .map(|m| board_of(sq(m)))
            .collect();
        found.sort();
        found.dedup();
        found
    }

    #[test]
    fn notation_is_strict() {
        assert!(UltimateRules::is_canonical("a1"));
        assert!(UltimateRules::is_canonical("i9"));
        for bad in ["", "a0", "a10", "j1", "a01", "E5", "e 5"] {
            assert!(!UltimateRules::is_canonical(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn squares_map_to_boards_and_cells() {
        assert_eq!((board_of(sq("e5")), cell_of(sq("e5"))), (4, 4));
        assert_eq!((board_of(sq("i9")), cell_of(sq("i9"))), (8, 8));
        assert_eq!((board_of(sq("d3")), cell_of(sq("d3"))), (1, 6));
        assert!(squares_of(4).all(|s| board_of(s) == 4));
        assert_eq!(squares_of(4).count(), 9);
    }

    #[test]
    fn each_cell_sends_the_opponent_to_its_board() {
        let start = UltimateRules::start();
        assert_eq!(UltimateRules::legal_moves(&start).len(), CELLS);

        // e5 is the centre cell of the centre board.
        let position = replay::<UltimateRules>(&["e5".to_string()]).unwrap();
        let legal = UltimateRules::legal_moves(&position);
        let expected: Vec<String> = squares_of(4)
            .filter(|&s| s != sq("e5"))
            .map(|s| square_name(s, SIZE))
            .collect();
        assert_eq!(legal, expected);

        // a1 is the bottom-left cell, so White must play in a1–c3.
        let mut position = replay::<UltimateRules>(&["a1".to_string()]).unwrap();
        assert_eq!(legal_boards(&position), [0]);
        let error = play_checked::<UltimateRules>(&mut position.clone(), "e5").unwrap_err();
        assert!(error.contains("sent to"), "{error}");
        play_checked::<UltimateRules>(&mut position, "c3").unwrap();
        assert_eq!(legal_boards(&position), [8]);
    }

    #[test]
    fn a_finished_board_frees_the_next_move() {
        let mut position = setup(&["a1", "b2"], &["d4", "f6"], &[EMPTY; 9]);
        play_checked::<UltimateRules>(&mut position, "c3").unwrap();
        assert_eq!(boards(&position), "______b__");
        assert_eq!(legal_boards(&position), [8]);

        // g7 is the bottom-left cell of its board, sending Black to the won board 0.
        play_checked::<UltimateRules>(&mut position, "g7").unwrap();
        assert_eq!(position.target, None);
        assert_eq!(legal_boards(&position), [1, 2, 3, 4, 5, 6, 7, 8]);
        let error = play_checked::<UltimateRules>(&mut position, "a2").unwrap_err();
        assert!(error.contains("finished"), "{error}");
    }

    #[test]
    fn three_won_boards_in_a_row_win() {
        let states = [b'b', b'b', EMPTY, EMPTY, b'w', EMPTY, EMPTY, b'w', EMPTY];
        let mut position = setup(&["g1", "h2"], &["e5", "e8"], &states);
        play_checked::<UltimateRules>(&mut position, "i3").unwrap();
        assert_eq!(UltimateRules::outcome(&position), Outcome::Win(Side::Black));
        assert!(UltimateRules::legal_moves(&position).is_empty());
        assert!(play_checked::<UltimateRules>(&mut position, "e4").is_err());
    }

    #[test]
    fn all_boards_finished_without_a_line_is_a_draw() {
        let states = [b'b', b'w', b'b', b'b', b'w', b'w', b'w', b'b', EMPTY];
        let black = ["g7", "i7", "g8", "h9"];
        let white = ["h7", "h8", "i8", "g9"];
        let mut position = setup(&black, &white, &states);
        play_checked::<UltimateRules>(&mut position, "i9").unwrap();
        assert_eq!(boards(&position), "wbdbwwbwb");
        assert_eq!(UltimateRules::outcome(&position), Outcome::Draw);

        let mut grid = [EMPTY; 9];
        grid.copy_from_slice(b"bwbbwwwbb");
        assert!(!has_line(&grid, b'b') && !has_line(&grid, b'w'));
    }

    placement::placement_lifecycle_tests!(crate::apps::ultimate_ttt::UltimateRules);
}
