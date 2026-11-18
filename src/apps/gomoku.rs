//! Freestyle Gomoku on a 15x15 board: five or more in a row wins and a full
//! board is a draw. Wire moves are `{m, n, x}` with `m` a square such as
//! `h8`; SPEC.md Appendix E defines the shared wire format.

use super::placement::{
    self, EMPTY, Outcome, PlacementRules, Side, parse_square, render_grid, square_name,
};

pub const APP_ID: &str = "gomoku";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 15;

const CELLS: usize = SIZE * SIZE;
const R_FIVE: &str = "5ir";
const R_FULL: &str = "ful";

/// `(file, rank)` line directions; each is also walked backwards.
const LINES: [(isize, isize); 4] = [(1, 0), (0, 1), (1, 1), (1, -1)];

placement::placement_coin!();

#[derive(Clone, Debug)]
pub(crate) struct GomokuPosition {
    cells: [u8; CELLS],
    ply: usize,
    outcome: Outcome,
}

/// Length of the line of `stone` through `index` along `(df, dr)`.
fn line_length(cells: &[u8; CELLS], index: usize, (df, dr): (isize, isize)) -> usize {
    let stone = cells[index];
    let mut length = 1;
    for sign in [1, -1] {
        let (mut file, mut rank) = ((index % SIZE) as isize, (index / SIZE) as isize);
        loop {
            file += df * sign;
            rank += dr * sign;
            let range = 0..SIZE as isize;
            if !range.contains(&file)
                || !range.contains(&rank)
                || cells[rank as usize * SIZE + file as usize] != stone
            {
                break;
            }
            length += 1;
        }
    }
    length
}

fn makes_five(cells: &[u8; CELLS], index: usize) -> bool {
    LINES.iter().any(|dir| line_length(cells, index, *dir) >= 5)
}

pub(crate) struct GomokuRules;

impl PlacementRules for GomokuRules {
    type Position = GomokuPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Gomoku";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "o15";
    #[cfg(test)]
    const MAX_PLIES: usize = 225;
    const CAN_DRAW: bool = true;
    const WIN_REASON: &'static str = R_FIVE;
    const DRAW_REASON: &'static str = R_FULL;

    fn start() -> GomokuPosition {
        GomokuPosition {
            cells: [EMPTY; CELLS],
            ply: 0,
            outcome: Outcome::Ongoing,
        }
    }

    fn side_to_move(position: &GomokuPosition) -> Side {
        if position.ply & 1 == 0 {
            Side::Black
        } else {
            Side::White
        }
    }

    fn is_canonical(notation: &str) -> bool {
        parse_square(notation, SIZE).is_some()
    }

    fn legal_moves(position: &GomokuPosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        (0..CELLS)
            .filter(|index| position.cells[*index] == EMPTY)
            .map(|index| square_name(index, SIZE))
            .collect()
    }

    fn play(position: &mut GomokuPosition, notation: &str) -> Result<(), String> {
        let index = parse_square(notation, SIZE)
            .ok_or_else(|| format!("Invalid move notation: {notation}"))?;
        if position.cells[index] != EMPTY {
            return Err(format!("Square {notation} is occupied"));
        }
        let side = Self::side_to_move(position);
        position.cells[index] = side.stone();
        position.ply += 1;
        if makes_five(&position.cells, index) {
            position.outcome = Outcome::Win(side);
        } else if position.ply == CELLS {
            position.outcome = Outcome::Draw;
        }
        Ok(())
    }

    fn outcome(position: &GomokuPosition) -> Outcome {
        position.outcome
    }

    fn render(position: &GomokuPosition) -> String {
        render_grid(&position.cells, SIZE)
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Gomoku LRGP application.
    GomokuApp,
    GomokuRules
);

/// Stone for a full, five-free board: pairs of stones that shift by two
/// files each rank. Rows, columns, and both diagonals then change color at
/// least every four cells, and Black holds 113 of the 225 cells.
#[cfg(test)]
pub(crate) fn draw_pattern_is_black(file: usize, rank: usize) -> bool {
    ((file + 2 * rank) / 2) & 1 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn game(moves: &[&str]) -> GomokuPosition {
        let moves: Vec<String> = moves.iter().map(|m| m.to_string()).collect();
        replay::<GomokuRules>(&moves).unwrap()
    }

    /// Black plays `black` while White plays harmless far-away stones.
    fn line_game(black: &[&str], white: &[&str]) -> Vec<String> {
        let mut moves = Vec::new();
        for (index, stone) in black.iter().enumerate() {
            moves.push(stone.to_string());
            if let Some(reply) = white.get(index) {
                moves.push(reply.to_string());
            }
        }
        moves
    }

    #[test]
    fn notation_is_strict() {
        assert!(GomokuRules::is_canonical("a1"));
        assert!(GomokuRules::is_canonical("o15"));
        for bad in ["", "a0", "a16", "p1", "a01", "H8", "h 8", "swap"] {
            assert!(!GomokuRules::is_canonical(bad), "accepted {bad:?}");
        }
        assert_eq!(square_name(CELLS - 1, SIZE), "o15");
    }

    #[test]
    fn five_wins_in_every_direction() {
        let far = ["a15", "c15", "e15", "g15"];
        for line in [
            ["d4", "e4", "f4", "g4", "h4"],
            ["d4", "d5", "d6", "d7", "d8"],
            ["d4", "e5", "f6", "g7", "h8"],
            ["d8", "e7", "f6", "g5", "h4"],
        ] {
            let moves = line_game(&line, &far);
            let before = replay::<GomokuRules>(&moves[..moves.len() - 1]).unwrap();
            assert_eq!(GomokuRules::outcome(&before), Outcome::Ongoing, "{line:?}");
            let won = replay::<GomokuRules>(&moves).unwrap();
            assert_eq!(
                GomokuRules::outcome(&won),
                Outcome::Win(Side::Black),
                "{line:?}"
            );
            assert!(GomokuRules::legal_moves(&won).is_empty());
        }
    }

    #[test]
    fn overlines_win_in_freestyle() {
        // Filling the gap in d4 e4 f4 _ h4 i4 makes six in a row.
        let moves = line_game(
            &["d4", "e4", "f4", "h4", "i4", "g4"],
            &["a15", "c15", "e15", "g15", "i15"],
        );
        let won = replay::<GomokuRules>(&moves).unwrap();
        assert_eq!(GomokuRules::outcome(&won), Outcome::Win(Side::Black));
    }

    #[test]
    fn four_and_broken_lines_do_not_win() {
        let moves = line_game(
            &["d4", "e4", "f4", "g4", "i4"],
            &["a15", "c15", "e15", "g15"],
        );
        let position = replay::<GomokuRules>(&moves).unwrap();
        assert_eq!(GomokuRules::outcome(&position), Outcome::Ongoing);
    }

    #[test]
    fn white_can_win() {
        let moves = line_game(
            &["a1", "c1", "e1", "g1", "i1", "k1"],
            &["d8", "e8", "f8", "g8", "h8"],
        );
        let won = replay::<GomokuRules>(&moves[..moves.len() - 1]).unwrap();
        assert_eq!(GomokuRules::outcome(&won), Outcome::Win(Side::White));
    }

    #[test]
    fn occupied_squares_and_post_win_moves_are_illegal() {
        let mut position = game(&["h8"]);
        assert!(play_checked::<GomokuRules>(&mut position, "h8").is_err());
        let moves = line_game(
            &["d4", "e4", "f4", "g4", "h4"],
            &["a15", "c15", "e15", "g15"],
        );
        let mut won = replay::<GomokuRules>(&moves).unwrap();
        assert!(play_checked::<GomokuRules>(&mut won, "o1").is_err());
    }

    #[test]
    fn full_board_without_five_is_a_draw() {
        let (mut black, mut white) = (Vec::new(), Vec::new());
        for rank in 0..SIZE {
            for file in 0..SIZE {
                let name = square_name(rank * SIZE + file, SIZE);
                if draw_pattern_is_black(file, rank) {
                    black.push(name);
                } else {
                    white.push(name);
                }
            }
        }
        // Black moves first, so it places one more stone than White.
        assert_eq!(black.len(), white.len() + 1);
        let mut moves = Vec::new();
        for (index, stone) in black.iter().enumerate() {
            moves.push(stone.clone());
            if let Some(reply) = white.get(index) {
                moves.push(reply.clone());
            }
        }
        let before = replay::<GomokuRules>(&moves[..CELLS - 1]).unwrap();
        assert_eq!(GomokuRules::outcome(&before), Outcome::Ongoing);
        let drawn = replay::<GomokuRules>(&moves).unwrap();
        assert_eq!(GomokuRules::outcome(&drawn), Outcome::Draw);
    }

    #[test]
    fn board_renders_from_the_top_rank() {
        let board = GomokuRules::render(&game(&["a1", "o15"]));
        assert_eq!(board.len(), CELLS);
        assert!(board.starts_with("______________w"));
        assert!(board.ends_with("b______________"));
    }

    placement::placement_lifecycle_tests!(crate::apps::gomoku::GomokuRules);
}
