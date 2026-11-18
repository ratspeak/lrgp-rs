//! Dots and Boxes on a 5x5-box grid (6x6 dots). Completing a box claims it
//! and the mover draws again; the game ends when every box is claimed and the
//! side with more boxes wins. Wire moves are `{m, n, x}` with `m` a line such
//! as `c3h`; see SPEC.md Appendix F.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use super::placement::{self, EMPTY, Outcome, PlacementRules, Side, parse_square, square_name};

pub const APP_ID: &str = "dots_and_boxes";
pub const APP_VERSION: u32 = 1;
/// Boxes along each edge; the grid has one more dot than this per edge.
pub const BOXES: usize = 5;

const DOTS: usize = BOXES + 1;
/// Lines of each orientation: horizontal `rank * BOXES + file`, vertical
/// `rank * DOTS + file`.
const LINES: usize = BOXES * DOTS;
const GRID: usize = 2 * BOXES + 1;
const R_COUNT: &str = "cnt";

placement::placement_coin!();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Line {
    /// From the dot to its right neighbour.
    Horizontal { file: usize, rank: usize },
    /// From the dot to the neighbour above it.
    Vertical { file: usize, rank: usize },
}

/// `c3h` is the line from dot c3 to d3, `c3v` the line from c3 to c4.
fn parse_line(notation: &str) -> Option<Line> {
    let (dot, direction) = notation.split_at_checked(notation.len().checked_sub(1)?)?;
    let index = parse_square(dot, DOTS)?;
    let (file, rank) = (index % DOTS, index / DOTS);
    match direction {
        "h" if file < BOXES => Some(Line::Horizontal { file, rank }),
        "v" if rank < BOXES => Some(Line::Vertical { file, rank }),
        _ => None,
    }
}

fn line_name(line: Line) -> String {
    match line {
        Line::Horizontal { file, rank } => format!("{}h", square_name(rank * DOTS + file, DOTS)),
        Line::Vertical { file, rank } => format!("{}v", square_name(rank * DOTS + file, DOTS)),
    }
}

/// Every line in canonical order: by dot (rank, then file), `h` before `v`.
fn all_lines() -> impl Iterator<Item = Line> {
    (0..DOTS * DOTS).flat_map(|dot| {
        let (file, rank) = (dot % DOTS, dot / DOTS);
        [
            (file < BOXES).then_some(Line::Horizontal { file, rank }),
            (rank < BOXES).then_some(Line::Vertical { file, rank }),
        ]
        .into_iter()
        .flatten()
    })
}

#[derive(Clone, Debug)]
pub(crate) struct DotsPosition {
    horizontal: [bool; LINES],
    vertical: [bool; LINES],
    /// Box owners, `rank * BOXES + file`.
    boxes: [u8; BOXES * BOXES],
    side: Side,
    outcome: Outcome,
    /// The last move completed a box, so the mover draws again.
    extra_turn: bool,
}

impl DotsPosition {
    fn drawn(&self, line: Line) -> bool {
        match line {
            Line::Horizontal { file, rank } => self.horizontal[rank * BOXES + file],
            Line::Vertical { file, rank } => self.vertical[rank * DOTS + file],
        }
    }

    fn box_complete(&self, file: usize, rank: usize) -> bool {
        self.horizontal[rank * BOXES + file]
            && self.horizontal[(rank + 1) * BOXES + file]
            && self.vertical[rank * DOTS + file]
            && self.vertical[rank * DOTS + file + 1]
    }

    fn count(&self, side: Side) -> usize {
        self.boxes
            .iter()
            .filter(|cell| **cell == side.stone())
            .count()
    }
}

/// Boxes on either side of a line, as `(file, rank)`.
fn adjacent_boxes(line: Line) -> Vec<(usize, usize)> {
    let mut boxes = Vec::with_capacity(2);
    match line {
        Line::Horizontal { file, rank } => {
            if rank > 0 {
                boxes.push((file, rank - 1));
            }
            if rank < BOXES {
                boxes.push((file, rank));
            }
        }
        Line::Vertical { file, rank } => {
            if file > 0 {
                boxes.push((file - 1, rank));
            }
            if file < BOXES {
                boxes.push((file, rank));
            }
        }
    }
    boxes
}

pub(crate) struct DotsAndBoxesRules;

impl PlacementRules for DotsAndBoxesRules {
    type Position = DotsPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Dots and Boxes";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "f5v";
    #[cfg(test)]
    const MAX_PLIES: usize = 2 * LINES;
    // An odd number of boxes cannot split evenly.
    const CAN_DRAW: bool = false;
    const WIN_REASON: &'static str = R_COUNT;
    const DRAW_REASON: &'static str = R_COUNT;

    fn start() -> DotsPosition {
        DotsPosition {
            horizontal: [false; LINES],
            vertical: [false; LINES],
            boxes: [EMPTY; BOXES * BOXES],
            side: Side::Black,
            outcome: Outcome::Ongoing,
            extra_turn: false,
        }
    }

    fn side_to_move(position: &DotsPosition) -> Side {
        position.side
    }

    fn is_canonical(notation: &str) -> bool {
        parse_line(notation).is_some()
    }

    fn legal_moves(position: &DotsPosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        all_lines()
            .filter(|line| !position.drawn(*line))
            .map(line_name)
            .collect()
    }

    fn play(position: &mut DotsPosition, notation: &str) -> Result<(), String> {
        let line =
            parse_line(notation).ok_or_else(|| format!("Invalid move notation: {notation}"))?;
        if position.drawn(line) {
            return Err(format!("Line {notation} is already drawn"));
        }
        match line {
            Line::Horizontal { file, rank } => position.horizontal[rank * BOXES + file] = true,
            Line::Vertical { file, rank } => position.vertical[rank * DOTS + file] = true,
        }

        let side = position.side;
        let mut claimed = false;
        for (file, rank) in adjacent_boxes(line) {
            if position.box_complete(file, rank) {
                position.boxes[rank * BOXES + file] = side.stone();
                claimed = true;
            }
        }
        position.extra_turn = claimed;
        if !claimed {
            position.side = side.other();
        }

        if position.boxes.iter().all(|cell| *cell != EMPTY) {
            let (black, white) = (position.count(Side::Black), position.count(Side::White));
            position.outcome = match black.cmp(&white) {
                std::cmp::Ordering::Greater => Outcome::Win(Side::Black),
                std::cmp::Ordering::Less => Outcome::Win(Side::White),
                std::cmp::Ordering::Equal => Outcome::Draw,
            };
        }
        Ok(())
    }

    fn outcome(position: &DotsPosition) -> Outcome {
        position.outcome
    }

    /// An 11x11 picture from the top-left dot: `+` dots, `-` and `|` drawn
    /// lines, a space for an undrawn line, and `_` / `b` / `w` box owners.
    fn render(position: &DotsPosition) -> String {
        let mut out = String::with_capacity(GRID * GRID);
        for row in (0..GRID).rev() {
            let rank = row / 2;
            for column in 0..GRID {
                let file = column / 2;
                out.push(match (row % 2, column % 2) {
                    (0, 0) => '+',
                    (0, _) if position.horizontal[rank * BOXES + file] => '-',
                    (_, 0) if position.vertical[rank * DOTS + file] => '|',
                    (1, 1) => position.boxes[rank * BOXES + file] as char,
                    _ => ' ',
                });
            }
        }
        out
    }

    fn extra_metadata(position: &DotsPosition, metadata: &mut HashMap<String, JsonValue>) {
        metadata.insert(
            "black_boxes".into(),
            JsonValue::from(position.count(Side::Black)),
        );
        metadata.insert(
            "white_boxes".into(),
            JsonValue::from(position.count(Side::White)),
        );
        metadata.insert("extra_turn".into(), JsonValue::Bool(position.extra_turn));
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Dots and Boxes LRGP application.
    DotsAndBoxesApp,
    DotsAndBoxesRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn play_all(position: &mut DotsPosition, moves: &[&str]) {
        for notation in moves {
            play_checked::<DotsAndBoxesRules>(position, notation).unwrap();
        }
    }

    #[test]
    fn notation_is_strict() {
        for good in ["a1h", "a1v", "e6h", "f5v", "c3h"] {
            assert!(DotsAndBoxesRules::is_canonical(good), "rejected {good:?}");
        }
        for bad in [
            "", "h", "a1", "f1h", "a6v", "g1h", "a7h", "a0h", "a01h", "A1h", "a1H", "a1x", "a1 h",
            "a1hv",
        ] {
            assert!(!DotsAndBoxesRules::is_canonical(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn opening_lists_every_line_in_canonical_order() {
        let legal = DotsAndBoxesRules::legal_moves(&DotsAndBoxesRules::start());
        assert_eq!(legal.len(), 2 * LINES);
        assert_eq!(&legal[..4], ["a1h", "a1v", "b1h", "b1v"]);
        // f1 has no line to its right; a6 has none above it.
        assert_eq!(&legal[9..12], ["e1v", "f1v", "a2h"]);
        assert_eq!(&legal[legal.len() - 2..], ["d6h", "e6h"]);
    }

    #[test]
    fn completing_a_box_claims_it_and_moves_again() {
        let mut game = DotsAndBoxesRules::start();
        play_all(&mut game, &["a1h", "a1v", "a2h"]);
        assert_eq!(DotsAndBoxesRules::side_to_move(&game), Side::White);
        assert!(!game.extra_turn);

        play_checked::<DotsAndBoxesRules>(&mut game, "b1v").unwrap();
        assert_eq!(game.boxes[0], b'w');
        assert!(game.extra_turn);
        assert_eq!(DotsAndBoxesRules::side_to_move(&game), Side::White);
        let mut metadata = HashMap::new();
        DotsAndBoxesRules::extra_metadata(&game, &mut metadata);
        assert_eq!(metadata["white_boxes"], JsonValue::from(1));
        assert_eq!(metadata["extra_turn"], JsonValue::Bool(true));

        play_checked::<DotsAndBoxesRules>(&mut game, "c1v").unwrap();
        assert!(!game.extra_turn);
        assert_eq!(DotsAndBoxesRules::side_to_move(&game), Side::Black);
        assert!(play_checked::<DotsAndBoxesRules>(&mut game, "a1h").is_err());
    }

    #[test]
    fn one_line_can_complete_two_boxes() {
        // b1v is the shared side of a1 and b1.
        let mut game = DotsAndBoxesRules::start();
        play_all(
            &mut game,
            &["a1h", "a1v", "a2h", "b1h", "b2h", "c1v", "b1v"],
        );
        assert_eq!(game.boxes[0], b'b');
        assert_eq!(game.boxes[1], b'b');
        assert_eq!(game.count(Side::Black), 2);
        assert_eq!(DotsAndBoxesRules::side_to_move(&game), Side::Black);
    }

    #[test]
    fn render_draws_lines_and_owners() {
        let mut game = DotsAndBoxesRules::start();
        play_all(&mut game, &["a1h", "a1v", "a2h", "b1v"]);
        let board = DotsAndBoxesRules::render(&game);
        assert_eq!(board.len(), GRID * GRID);
        let rows: Vec<&str> = (0..GRID).map(|r| &board[r * GRID..][..GRID]).collect();
        assert_eq!(rows[0], "+ + + + + +");
        assert_eq!(rows[1], " _ _ _ _ _ ");
        assert_eq!(rows[8], "+-+ + + + +");
        assert_eq!(rows[9], "|w|_ _ _ _ ");
        assert_eq!(rows[10], "+-+ + + + +");
    }

    #[test]
    fn the_game_ends_when_every_box_is_claimed() {
        let all: Vec<String> = all_lines().map(line_name).collect();
        let game = replay::<DotsAndBoxesRules>(&all).unwrap();
        match DotsAndBoxesRules::outcome(&game) {
            Outcome::Win(_) => {}
            other => panic!("unexpected outcome {other:?}"),
        }
        assert_eq!(game.count(Side::Black) + game.count(Side::White), 25);
        assert!(DotsAndBoxesRules::legal_moves(&game).is_empty());
        assert!(play_checked::<DotsAndBoxesRules>(&mut game.clone(), "a1h").is_err());
    }

    placement::placement_lifecycle_tests!(crate::apps::dots_and_boxes::DotsAndBoxesRules);
}
