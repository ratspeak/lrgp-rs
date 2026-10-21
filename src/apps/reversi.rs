//! Reversi on an 8x8 board. A move must flip at least one disc; a player with
//! no legal move passes automatically, and the game ends when neither side can
//! move. Wire moves are `{m, n, x}` with `m` a square such as `d3`; see
//! SPEC.md Appendix E for the shared wire format.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use super::placement::{
    self, EMPTY, Outcome, PlacementRules, Side, parse_square, render_grid, square_name,
};

pub const APP_ID: &str = "reversi";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 8;

const CELLS: usize = SIZE * SIZE;
const R_COUNT: &str = "cnt";

/// `(file, rank)` steps in all eight directions.
const DIRECTIONS: [(isize, isize); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

placement::placement_coin!();

#[derive(Clone, Debug)]
pub(crate) struct ReversiPosition {
    cells: [u8; CELLS],
    side: Side,
    outcome: Outcome,
    /// The last move left the opponent without a move, so the mover goes again.
    skipped: bool,
}

/// Discs that placing `side` on `index` would flip; empty if illegal.
fn flips(cells: &[u8; CELLS], index: usize, side: Side) -> Vec<usize> {
    if cells[index] != EMPTY {
        return Vec::new();
    }
    let (own, theirs) = (side.stone(), side.other().stone());
    let mut flipped = Vec::new();
    for (df, dr) in DIRECTIONS {
        let (mut file, mut rank) = ((index % SIZE) as isize, (index / SIZE) as isize);
        let mut run = Vec::new();
        loop {
            file += df;
            rank += dr;
            let range = 0..SIZE as isize;
            if !range.contains(&file) || !range.contains(&rank) {
                run.clear();
                break;
            }
            let square = rank as usize * SIZE + file as usize;
            if cells[square] == theirs {
                run.push(square);
            } else {
                if cells[square] != own {
                    run.clear();
                }
                break;
            }
        }
        flipped.extend(run);
    }
    flipped
}

fn has_move(cells: &[u8; CELLS], side: Side) -> bool {
    (0..CELLS).any(|index| !flips(cells, index, side).is_empty())
}

fn count(cells: &[u8; CELLS], side: Side) -> usize {
    cells.iter().filter(|cell| **cell == side.stone()).count()
}

pub(crate) struct ReversiRules;

impl PlacementRules for ReversiRules {
    type Position = ReversiPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Reversi";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "h8";
    #[cfg(test)]
    const MAX_PLIES: usize = 60;
    const CAN_DRAW: bool = true;
    const WIN_REASON: &'static str = R_COUNT;
    const DRAW_REASON: &'static str = R_COUNT;

    fn start() -> ReversiPosition {
        let mut cells = [EMPTY; CELLS];
        for (name, side) in [
            ("d4", Side::White),
            ("e4", Side::Black),
            ("d5", Side::Black),
            ("e5", Side::White),
        ] {
            cells[parse_square(name, SIZE).unwrap()] = side.stone();
        }
        ReversiPosition {
            cells,
            side: Side::Black,
            outcome: Outcome::Ongoing,
            skipped: false,
        }
    }

    fn side_to_move(position: &ReversiPosition) -> Side {
        position.side
    }

    fn is_canonical(notation: &str) -> bool {
        parse_square(notation, SIZE).is_some()
    }

    fn legal_moves(position: &ReversiPosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        (0..CELLS)
            .filter(|index| !flips(&position.cells, *index, position.side).is_empty())
            .map(|index| square_name(index, SIZE))
            .collect()
    }

    fn play(position: &mut ReversiPosition, notation: &str) -> Result<(), String> {
        let index = parse_square(notation, SIZE)
            .ok_or_else(|| format!("Invalid move notation: {notation}"))?;
        if position.cells[index] != EMPTY {
            return Err(format!("Square {notation} is occupied"));
        }
        let flipped = flips(&position.cells, index, position.side);
        if flipped.is_empty() {
            return Err(format!("{notation} does not flip any disc"));
        }
        let side = position.side;
        position.cells[index] = side.stone();
        for square in flipped {
            position.cells[square] = side.stone();
        }

        position.skipped = false;
        if has_move(&position.cells, side.other()) {
            position.side = side.other();
        } else if has_move(&position.cells, side) {
            position.skipped = true;
        } else {
            let (black, white) = (
                count(&position.cells, Side::Black),
                count(&position.cells, Side::White),
            );
            position.outcome = match black.cmp(&white) {
                std::cmp::Ordering::Greater => Outcome::Win(Side::Black),
                std::cmp::Ordering::Less => Outcome::Win(Side::White),
                std::cmp::Ordering::Equal => Outcome::Draw,
            };
        }
        Ok(())
    }

    fn outcome(position: &ReversiPosition) -> Outcome {
        position.outcome
    }

    fn render(position: &ReversiPosition) -> String {
        render_grid(&position.cells, SIZE)
    }

    fn extra_metadata(position: &ReversiPosition, metadata: &mut HashMap<String, JsonValue>) {
        metadata.insert(
            "black_count".into(),
            JsonValue::from(count(&position.cells, Side::Black)),
        );
        metadata.insert(
            "white_count".into(),
            JsonValue::from(count(&position.cells, Side::White)),
        );
        metadata.insert("skipped_turn".into(), JsonValue::Bool(position.skipped));
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Reversi LRGP application.
    ReversiApp,
    ReversiRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn position(side: Side, black: &[&str], white: &[&str]) -> ReversiPosition {
        let mut cells = [EMPTY; CELLS];
        for name in black {
            cells[parse_square(name, SIZE).unwrap()] = Side::Black.stone();
        }
        for name in white {
            cells[parse_square(name, SIZE).unwrap()] = Side::White.stone();
        }
        ReversiPosition {
            cells,
            side,
            outcome: Outcome::Ongoing,
            skipped: false,
        }
    }

    fn cell(position: &ReversiPosition, name: &str) -> u8 {
        position.cells[parse_square(name, SIZE).unwrap()]
    }

    #[test]
    fn opening_position_and_moves() {
        let start = ReversiRules::start();
        assert_eq!(
            ReversiRules::render(&start),
            "___________________________bw______wb___________________________"
        );
        assert_eq!(ReversiRules::legal_moves(&start), ["d3", "c4", "f5", "e6"]);
        assert_eq!(ReversiRules::side_to_move(&start), Side::Black);
    }

    #[test]
    fn notation_is_strict() {
        assert!(ReversiRules::is_canonical("a1"));
        assert!(ReversiRules::is_canonical("h8"));
        for bad in ["", "a0", "a9", "i1", "a01", "D3", "d 3"] {
            assert!(!ReversiRules::is_canonical(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn a_move_flips_every_bracketed_line() {
        // d3 brackets d4 vertically, and nothing else.
        let mut game = ReversiRules::start();
        play_checked::<ReversiRules>(&mut game, "d3").unwrap();
        assert_eq!(cell(&game, "d4"), b'b');
        assert_eq!(cell(&game, "e5"), b'w');
        assert_eq!(ReversiRules::side_to_move(&game), Side::White);

        // e1 closes three lines at once: west to a1, north to e5, east to h1.
        let mut game = position(
            Side::Black,
            &["a1", "e5", "h1"],
            &["b1", "c1", "d1", "e2", "e3", "e4", "f1", "g1"],
        );
        play_checked::<ReversiRules>(&mut game, "e1").unwrap();
        for name in ["b1", "c1", "d1", "e2", "e3", "e4", "f1", "g1"] {
            assert_eq!(cell(&game, name), b'b', "{name}");
        }
    }

    #[test]
    fn unbracketed_and_occupied_squares_are_illegal() {
        let mut game = ReversiRules::start();
        assert!(play_checked::<ReversiRules>(&mut game, "a1").is_err());
        assert!(play_checked::<ReversiRules>(&mut game, "d4").is_err());
        // A line that runs into the edge without an own disc does not flip.
        let mut edge = position(Side::Black, &["d5"], &["b1", "c1"]);
        assert!(play_checked::<ReversiRules>(&mut edge, "d1").is_err());
    }

    #[test]
    fn opponent_without_a_move_passes_automatically() {
        // After a3 captures a2, White's lone g7 cannot bracket anything, but
        // Black can still capture it with f6, so Black moves again.
        let mut game = position(Side::Black, &["a1", "h8"], &["a2", "g7"]);
        play_checked::<ReversiRules>(&mut game, "a3").unwrap();
        assert!(!has_move(&game.cells, Side::White));
        assert_eq!(ReversiRules::outcome(&game), Outcome::Ongoing);
        assert_eq!(ReversiRules::side_to_move(&game), Side::Black);
        assert_eq!(ReversiRules::legal_moves(&game), ["f6"]);
        assert!(game.skipped);
        let mut metadata = HashMap::new();
        ReversiRules::extra_metadata(&game, &mut metadata);
        assert_eq!(metadata["skipped_turn"], JsonValue::Bool(true));

        play_checked::<ReversiRules>(&mut game, "f6").unwrap();
        assert!(!game.skipped);
    }

    #[test]
    fn game_ends_when_neither_side_can_move() {
        let mut wipeout = position(Side::Black, &["a1"], &["a2"]);
        play_checked::<ReversiRules>(&mut wipeout, "a3").unwrap();
        assert_eq!(ReversiRules::outcome(&wipeout), Outcome::Win(Side::Black));
        assert!(ReversiRules::legal_moves(&wipeout).is_empty());
        assert!(play_checked::<ReversiRules>(&mut wipeout, "h8").is_err());
    }

    #[test]
    fn the_last_mover_can_lose_on_count() {
        // White's a3 captures a2 and ends the game 3 White to 4 Black.
        let mut game = position(Side::White, &["a2", "h5", "h6", "h7", "h8"], &["a1"]);
        play_checked::<ReversiRules>(&mut game, "a3").unwrap();
        assert_eq!(count(&game.cells, Side::White), 3);
        assert_eq!(count(&game.cells, Side::Black), 4);
        assert_eq!(ReversiRules::outcome(&game), Outcome::Win(Side::Black));
    }

    #[test]
    fn equal_counts_end_in_a_draw() {
        let mut game = position(Side::White, &["a2", "h6", "h7", "h8"], &["a1"]);
        play_checked::<ReversiRules>(&mut game, "a3").unwrap();
        assert_eq!(ReversiRules::outcome(&game), Outcome::Draw);
    }

    #[test]
    fn full_replay_matches_incremental_play() {
        let moves: Vec<String> = ["f5", "d6", "c3", "d3", "c4"]
            .iter()
            .map(|m| m.to_string())
            .collect();
        let game = replay::<ReversiRules>(&moves).unwrap();
        assert_eq!(
            count(&game.cells, Side::Black) + count(&game.cells, Side::White),
            4 + moves.len()
        );
    }

    placement::placement_lifecycle_tests!(crate::apps::reversi::ReversiRules);
}
