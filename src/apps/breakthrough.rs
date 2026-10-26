//! Breakthrough on an 8x8 board. Each side has two rows of pieces that step
//! one square forward, straight or diagonally, and capture only diagonally.
//! The first piece to reach the far rank wins. Wire moves are `{m, n, x}`
//! with `m` a step such as `a2b3`; SPEC.md Appendix G.5 defines the rules.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use super::placement::{
    self, EMPTY, Outcome, PlacementRules, Side, parse_square, render_grid, square_name,
};

pub const APP_ID: &str = "breakthrough";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 8;

const CELLS: usize = SIZE * SIZE;
const R_GOAL: &str = "gol";
const R_PIECES: &str = "np";

placement::placement_coin!();

#[derive(Clone, Debug)]
pub(crate) struct BreakthroughPosition {
    cells: [u8; CELLS],
    ply: usize,
    outcome: Outcome,
    reason: &'static str,
}

fn forward(side: Side) -> isize {
    match side {
        Side::Black => 1,
        Side::White => -1,
    }
}

/// A step: two squares written together, such as `a2b3`.
fn parse_step(notation: &str) -> Option<(usize, usize)> {
    if notation.len() != 4 || !notation.is_ascii() {
        return None;
    }
    let (from, to) = notation.split_at(2);
    Some((parse_square(from, SIZE)?, parse_square(to, SIZE)?))
}

impl BreakthroughPosition {
    fn steps(&self, side: Side) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        for from in (0..CELLS).filter(|&s| self.cells[s] == side.stone()) {
            let (file, rank) = ((from % SIZE) as isize, (from / SIZE) as isize);
            let to_rank = rank + forward(side);
            if !(0..SIZE as isize).contains(&to_rank) {
                continue;
            }
            for df in [-1, 0, 1] {
                let to_file = file + df;
                if !(0..SIZE as isize).contains(&to_file) {
                    continue;
                }
                let to = to_rank as usize * SIZE + to_file as usize;
                let target = self.cells[to];
                if target == EMPTY || (df != 0 && target == side.other().stone()) {
                    found.push((from, to));
                }
            }
        }
        found
    }

    fn count(&self, side: Side) -> usize {
        self.cells.iter().filter(|&&c| c == side.stone()).count()
    }
}

pub(crate) struct BreakthroughRules;

impl PlacementRules for BreakthroughRules {
    type Position = BreakthroughPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Breakthrough";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "h7h8";
    #[cfg(test)]
    const MAX_PLIES: usize = 2 * CELLS;
    const CAN_DRAW: bool = false;
    const WIN_REASON: &'static str = R_GOAL;
    const DRAW_REASON: &'static str = "";

    fn start() -> BreakthroughPosition {
        let mut cells = [EMPTY; CELLS];
        for square in 0..2 * SIZE {
            cells[square] = Side::Black.stone();
            cells[CELLS - 1 - square] = Side::White.stone();
        }
        BreakthroughPosition {
            cells,
            ply: 0,
            outcome: Outcome::Ongoing,
            reason: "",
        }
    }

    fn side_to_move(position: &BreakthroughPosition) -> Side {
        if position.ply & 1 == 0 {
            Side::Black
        } else {
            Side::White
        }
    }

    fn is_canonical(notation: &str) -> bool {
        parse_step(notation).is_some()
    }

    fn legal_moves(position: &BreakthroughPosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        let mut moves: Vec<String> = position
            .steps(Self::side_to_move(position))
            .into_iter()
            .map(|(from, to)| format!("{}{}", square_name(from, SIZE), square_name(to, SIZE)))
            .collect();
        moves.sort_unstable();
        moves
    }

    fn play(position: &mut BreakthroughPosition, notation: &str) -> Result<(), String> {
        let side = Self::side_to_move(position);
        let step =
            parse_step(notation).ok_or_else(|| format!("Invalid move notation: {notation}"))?;
        if !position.steps(side).contains(&step) {
            return Err(format!("Illegal move: {notation}"));
        }
        position.cells[step.0] = EMPTY;
        position.cells[step.1] = side.stone();
        position.ply += 1;
        let far = match side {
            Side::Black => SIZE - 1,
            Side::White => 0,
        };
        let opponent = side.other();
        let reason = if step.1 / SIZE == far {
            R_GOAL
        } else if position.count(opponent) == 0 {
            R_PIECES
        } else {
            // A side with pieces always has a move: its most advanced piece
            // can step to a diagonal square no piece of its own holds.
            return Ok(());
        };
        position.outcome = Outcome::Win(side);
        position.reason = reason;
        Ok(())
    }

    fn outcome(position: &BreakthroughPosition) -> Outcome {
        position.outcome
    }

    fn reason(position: &BreakthroughPosition) -> &'static str {
        position.reason
    }

    fn render(position: &BreakthroughPosition) -> String {
        render_grid(&position.cells, SIZE)
    }

    fn extra_metadata(position: &BreakthroughPosition, metadata: &mut HashMap<String, JsonValue>) {
        metadata.insert(
            "black_count".into(),
            JsonValue::from(position.count(Side::Black)),
        );
        metadata.insert(
            "white_count".into(),
            JsonValue::from(position.count(Side::White)),
        );
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Breakthrough LRGP application.
    BreakthroughApp,
    BreakthroughRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::{play_checked, replay};

    fn sq(name: &str) -> usize {
        parse_square(name, SIZE).unwrap()
    }

    /// A position with only the given pieces and Black to move.
    fn setup(black: &[&str], white: &[&str]) -> BreakthroughPosition {
        let mut position = BreakthroughRules::start();
        position.cells = [EMPTY; CELLS];
        for name in black {
            position.cells[sq(name)] = b'b';
        }
        for name in white {
            position.cells[sq(name)] = b'w';
        }
        position
    }

    #[test]
    fn notation_is_strict() {
        assert!(BreakthroughRules::is_canonical("a2b3"));
        for bad in ["", "a2", "a2-b3", "a2b9", "i2b3", "A2b3", "a2b3 "] {
            assert!(!BreakthroughRules::is_canonical(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_opening_has_twenty_two_steps() {
        let start = BreakthroughRules::start();
        let legal = BreakthroughRules::legal_moves(&start);
        assert_eq!(legal.len(), 22);
        assert_eq!(legal[0], "a2a3");
        assert!(legal.contains(&"h2g3".to_string()));
        assert_eq!(&BreakthroughRules::render(&start)[..16], "wwwwwwwwwwwwwwww");
    }

    #[test]
    fn captures_are_diagonal_only() {
        let position = setup(&["d4"], &["d5", "e5", "a8"]);
        let legal = BreakthroughRules::legal_moves(&position);
        assert_eq!(legal, ["d4c5", "d4e5"]);
        let mut position = position;
        play_checked::<BreakthroughRules>(&mut position, "d4e5").unwrap();
        assert_eq!(position.count(Side::White), 2);
        assert!(play_checked::<BreakthroughRules>(&mut setup(&["d4"], &["d5"]), "d4d5").is_err());
        // Backwards is never allowed.
        assert!(play_checked::<BreakthroughRules>(&mut setup(&["d4"], &["h8"]), "d4d3").is_err());
    }

    #[test]
    fn reaching_the_far_rank_wins() {
        let mut position = setup(&["c7"], &["h8", "a8"]);
        play_checked::<BreakthroughRules>(&mut position, "c7c8").unwrap();
        assert_eq!(
            BreakthroughRules::outcome(&position),
            Outcome::Win(Side::Black)
        );
        assert_eq!(BreakthroughRules::reason(&position), R_GOAL);
        assert!(BreakthroughRules::legal_moves(&position).is_empty());

        // White heads for rank 1.
        let moves: Vec<String> = ["a2a3", "h7h6"].iter().map(|m| m.to_string()).collect();
        let position = replay::<BreakthroughRules>(&moves).unwrap();
        assert_eq!(BreakthroughRules::side_to_move(&position), Side::Black);
        assert!(BreakthroughRules::legal_moves(&position).contains(&"a3a4".to_string()));
        let mut white = setup(&["a1", "h4"], &["c2"]);
        white.ply = 1;
        play_checked::<BreakthroughRules>(&mut white, "c2b1").unwrap();
        assert_eq!(
            BreakthroughRules::outcome(&white),
            Outcome::Win(Side::White)
        );
    }

    #[test]
    fn capturing_everything_wins() {
        let mut position = setup(&["d4", "a1"], &["e5"]);
        play_checked::<BreakthroughRules>(&mut position, "d4e5").unwrap();
        assert_eq!(
            BreakthroughRules::outcome(&position),
            Outcome::Win(Side::Black)
        );
        assert_eq!(BreakthroughRules::reason(&position), R_PIECES);
    }

    placement::placement_lifecycle_tests!(crate::apps::breakthrough::BreakthroughRules);
}
