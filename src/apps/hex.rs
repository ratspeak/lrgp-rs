//! Hex on an 11x11 board with the swap rule. Black joins ranks 1 and 11,
//! White joins files a and k. Wire moves are `{m, n, x}` with `m` a square
//! such as `f6` or `swap`; SPEC.md Appendix E defines the shared wire format.

use super::placement::{
    self, EMPTY, Outcome, PlacementRules, Side, parse_square, render_grid, square_name,
};

pub const APP_ID: &str = "hex";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 11;
/// White's optional second move: mirror Black's first stone as a White stone.
pub const SWAP: &str = "swap";

const CELLS: usize = SIZE * SIZE;
const R_CONNECTED: &str = "con";

/// `(file, rank)` steps to the six neighbours of a cell.
const NEIGHBORS: [(isize, isize); 6] = [(-1, 0), (1, 0), (0, -1), (0, 1), (1, -1), (-1, 1)];

placement::placement_coin!();

#[derive(Clone, Debug)]
pub(crate) struct HexPosition {
    cells: [u8; CELLS],
    ply: usize,
    winner: Option<Side>,
}

fn neighbor(index: usize, (df, dr): (isize, isize)) -> Option<usize> {
    let file = (index % SIZE) as isize + df;
    let rank = (index / SIZE) as isize + dr;
    let range = 0..SIZE as isize;
    (range.contains(&file) && range.contains(&rank))
        .then_some((rank * SIZE as isize + file) as usize)
}

/// `true` when the group containing `from` touches both of `side`'s edges.
fn connects(cells: &[u8; CELLS], from: usize, side: Side) -> bool {
    let stone = side.stone();
    let edge = |index: usize| match side {
        Side::Black => index / SIZE,
        Side::White => index % SIZE,
    };
    let mut seen = [false; CELLS];
    let mut stack = vec![from];
    seen[from] = true;
    let (mut low, mut high) = (false, false);
    while let Some(index) = stack.pop() {
        low |= edge(index) == 0;
        high |= edge(index) == SIZE - 1;
        for dir in NEIGHBORS {
            if let Some(next) = neighbor(index, dir) {
                if !seen[next] && cells[next] == stone {
                    seen[next] = true;
                    stack.push(next);
                }
            }
        }
    }
    low && high
}

pub(crate) struct HexRules;

impl PlacementRules for HexRules {
    type Position = HexPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Hex";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "k11";
    #[cfg(test)]
    const MAX_PLIES: usize = 122; // every cell, plus the swap
    const CAN_DRAW: bool = false;
    const WIN_REASON: &'static str = R_CONNECTED;
    const DRAW_REASON: &'static str = "";

    fn start() -> HexPosition {
        HexPosition {
            cells: [EMPTY; CELLS],
            ply: 0,
            winner: None,
        }
    }

    fn side_to_move(position: &HexPosition) -> Side {
        if position.ply & 1 == 0 {
            Side::Black
        } else {
            Side::White
        }
    }

    fn is_canonical(notation: &str) -> bool {
        notation == SWAP || parse_square(notation, SIZE).is_some()
    }

    fn legal_moves(position: &HexPosition) -> Vec<String> {
        if position.winner.is_some() {
            return Vec::new();
        }
        let mut moves: Vec<String> = (0..CELLS)
            .filter(|index| position.cells[*index] == EMPTY)
            .map(|index| square_name(index, SIZE))
            .collect();
        if position.ply == 1 {
            moves.push(SWAP.into());
        }
        moves
    }

    fn play(position: &mut HexPosition, notation: &str) -> Result<(), String> {
        if notation == SWAP {
            if position.ply != 1 {
                return Err("Swap is only legal as White's first move".into());
            }
            let black = position
                .cells
                .iter()
                .position(|cell| *cell == Side::Black.stone())
                .ok_or_else(|| "No stone to swap".to_string())?;
            let (file, rank) = (black % SIZE, black / SIZE);
            position.cells[black] = EMPTY;
            // The mirrored square swaps file and rank.
            position.cells[file * SIZE + rank] = Side::White.stone();
            position.ply += 1;
            return Ok(());
        }
        let index = parse_square(notation, SIZE)
            .ok_or_else(|| format!("Invalid move notation: {notation}"))?;
        if position.cells[index] != EMPTY {
            return Err(format!("Square {notation} is occupied"));
        }
        let side = Self::side_to_move(position);
        position.cells[index] = side.stone();
        position.ply += 1;
        if connects(&position.cells, index, side) {
            position.winner = Some(side);
        }
        Ok(())
    }

    fn outcome(position: &HexPosition) -> Outcome {
        position.winner.map_or(Outcome::Ongoing, Outcome::Win)
    }

    fn render(position: &HexPosition) -> String {
        render_grid(&position.cells, SIZE)
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Hex LRGP application.
    HexApp,
    HexRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn game(moves: &[&str]) -> HexPosition {
        let moves: Vec<String> = moves.iter().map(|m| m.to_string()).collect();
        replay::<HexRules>(&moves).unwrap()
    }

    fn cell(position: &HexPosition, name: &str) -> u8 {
        position.cells[parse_square(name, SIZE).unwrap()]
    }

    #[test]
    fn notation_is_strict() {
        assert!(HexRules::is_canonical("a1"));
        assert!(HexRules::is_canonical("k11"));
        assert!(HexRules::is_canonical(SWAP));
        for bad in ["", "a0", "a12", "l1", "a01", "A1", "k 11", "Swap", "a1 "] {
            assert!(!HexRules::is_canonical(bad), "accepted {bad:?}");
        }
        assert_eq!(square_name(CELLS - 1, SIZE), "k11");
    }

    #[test]
    fn neighbours_follow_the_rhombus() {
        let index = parse_square("b2", SIZE).unwrap();
        let mut names: Vec<String> = NEIGHBORS
            .iter()
            .filter_map(|dir| neighbor(index, *dir))
            .map(|n| square_name(n, SIZE))
            .collect();
        names.sort();
        assert_eq!(names, ["a2", "a3", "b1", "b3", "c1", "c2"]);
        let corner = parse_square("a1", SIZE).unwrap();
        assert_eq!(
            NEIGHBORS
                .iter()
                .filter_map(|d| neighbor(corner, *d))
                .count(),
            2
        );
    }

    #[test]
    fn black_connects_ranks_and_white_connects_files() {
        // Black plays the f-file rank by rank; White answers up the a-file,
        // which never reaches White's far edge.
        let mut moves = Vec::new();
        for rank in 1..=SIZE {
            moves.push(format!("f{rank}"));
            if rank < SIZE {
                moves.push(format!("a{rank}"));
            }
        }
        let moves: Vec<&str> = moves.iter().map(String::as_str).collect();
        let before = game(&moves[..moves.len() - 1]);
        assert_eq!(HexRules::outcome(&before), Outcome::Ongoing);
        let won = game(&moves);
        assert_eq!(HexRules::outcome(&won), Outcome::Win(Side::Black));
        assert!(HexRules::legal_moves(&won).is_empty());

        // A White chain from file a to file k across rank 6.
        let mut cells = [EMPTY; CELLS];
        for file in 0..SIZE {
            cells[5 * SIZE + file] = Side::White.stone();
        }
        assert!(connects(&cells, 5 * SIZE + 3, Side::White));
        assert!(!connects(&cells, 5 * SIZE + 3, Side::Black));
    }

    #[test]
    fn zigzag_chain_uses_hex_diagonals() {
        // k1, j2, ..., a11 is connected only through the (-1, +1) diagonal.
        let mut cells = [EMPTY; CELLS];
        for rank in 0..SIZE {
            cells[rank * SIZE + (SIZE - 1 - rank)] = Side::Black.stone();
        }
        assert!(connects(&cells, SIZE - 1, Side::Black));
        // The other diagonal is not adjacent in Hex.
        let mut cells = [EMPTY; CELLS];
        for rank in 0..SIZE {
            cells[rank * SIZE + rank] = Side::Black.stone();
        }
        assert!(!connects(&cells, 0, Side::Black));
    }

    #[test]
    fn swap_mirrors_blacks_first_stone_and_keeps_alternation() {
        let swapped = game(&["c7", SWAP]);
        assert_eq!(cell(&swapped, "c7"), EMPTY);
        assert_eq!(cell(&swapped, "g3"), Side::White.stone());
        assert_eq!(HexRules::side_to_move(&swapped), Side::Black);
        assert!(!HexRules::legal_moves(&swapped).contains(&SWAP.to_string()));

        let diagonal = game(&["f6", SWAP]);
        assert_eq!(cell(&diagonal, "f6"), Side::White.stone());

        assert_eq!(HexRules::legal_moves(&game(&["a1"])).last().unwrap(), SWAP);
        let mut start = HexRules::start();
        assert!(play_checked::<HexRules>(&mut start, SWAP).is_err());
        let mut later = game(&["a1", "b1"]);
        assert!(play_checked::<HexRules>(&mut later, SWAP).is_err());
    }

    #[test]
    fn occupied_cells_and_post_win_moves_are_illegal() {
        let mut position = game(&["e5"]);
        assert!(play_checked::<HexRules>(&mut position, "e5").is_err());
        assert_eq!(HexRules::legal_moves(&HexRules::start()).len(), CELLS);
    }

    #[test]
    fn full_boards_always_have_a_winner() {
        // Fill the board in a scrambled order; Hex cannot end in a draw.
        let mut position = HexRules::start();
        let mut index = 0usize;
        while HexRules::outcome(&position) == Outcome::Ongoing {
            index = (index + 37) % CELLS;
            let name = square_name(index, SIZE);
            if position.cells[index] == EMPTY {
                play_checked::<HexRules>(&mut position, &name).unwrap();
            }
        }
        assert!(matches!(HexRules::outcome(&position), Outcome::Win(_)));
    }

    #[test]
    fn board_renders_from_the_top_rank() {
        let position = game(&["a1", "k11"]);
        let board = HexRules::render(&position);
        assert_eq!(board.len(), CELLS);
        assert_eq!(&board[..SIZE], "__________w");
        assert_eq!(&board[CELLS - SIZE..], "b__________");
    }

    placement::placement_lifecycle_tests!(crate::apps::hex::HexRules);
}
