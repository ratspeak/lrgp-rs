//! Mancala, Kalah rules with six pits per side and four seeds per pit. Seeds
//! are sown counter-clockwise; ending in your own store earns another move,
//! and ending in an empty pit of your own captures the opposite pit. Wire
//! moves are `{m, n, x}` with `m` a pit letter; see SPEC.md Appendix F.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use super::placement::{self, Outcome, PlacementRules, Side};

pub const APP_ID: &str = "mancala";
pub const APP_VERSION: u32 = 1;
/// Pits per side.
pub const PITS: usize = 6;
/// Seeds in each pit at the start.
pub const SEEDS: u8 = 4;

/// Sowing order: Black's pits `a`–`f`, Black's store, White's pits `g`–`l`,
/// White's store.
const HOLES: usize = 2 * (PITS + 1);
const BLACK_STORE: usize = PITS;
const WHITE_STORE: usize = HOLES - 1;
const R_COUNT: &str = "cnt";

placement::placement_coin!();

fn store(side: Side) -> usize {
    match side {
        Side::Black => BLACK_STORE,
        Side::White => WHITE_STORE,
    }
}

fn pits(side: Side) -> std::ops::Range<usize> {
    match side {
        Side::Black => 0..PITS,
        Side::White => PITS + 1..WHITE_STORE,
    }
}

/// The pit facing `hole` across the board (`a` faces `l`, `f` faces `g`).
fn opposite(hole: usize) -> usize {
    2 * PITS - hole
}

/// Pit letters skip the stores: `a`–`f` are holes 0–5, `g`–`l` holes 7–12.
fn parse_pit(notation: &str) -> Option<usize> {
    let &[letter] = notation.as_bytes() else {
        return None;
    };
    let index = letter.checked_sub(b'a')? as usize;
    match index {
        0..PITS => Some(index),
        PITS..12 => Some(index + 1),
        _ => None,
    }
}

fn pit_name(hole: usize) -> String {
    let index = if hole < PITS { hole } else { hole - 1 };
    ((b'a' + index as u8) as char).to_string()
}

#[derive(Clone, Debug)]
pub(crate) struct MancalaPosition {
    holes: [u8; HOLES],
    side: Side,
    outcome: Outcome,
    /// The last seed landed in the mover's store, so the mover goes again.
    extra_turn: bool,
}

impl MancalaPosition {
    fn side_empty(&self, side: Side) -> bool {
        self.holes[pits(side)].iter().all(|seeds| *seeds == 0)
    }
}

pub(crate) struct MancalaRules;

impl PlacementRules for MancalaRules {
    type Position = MancalaPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Mancala";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "l";
    #[cfg(test)]
    const MAX_PLIES: usize = 1_000;
    const CAN_DRAW: bool = true;
    const WIN_REASON: &'static str = R_COUNT;
    const DRAW_REASON: &'static str = R_COUNT;

    fn start() -> MancalaPosition {
        let mut holes = [SEEDS; HOLES];
        holes[BLACK_STORE] = 0;
        holes[WHITE_STORE] = 0;
        MancalaPosition {
            holes,
            side: Side::Black,
            outcome: Outcome::Ongoing,
            extra_turn: false,
        }
    }

    fn side_to_move(position: &MancalaPosition) -> Side {
        position.side
    }

    fn is_canonical(notation: &str) -> bool {
        parse_pit(notation).is_some()
    }

    fn legal_moves(position: &MancalaPosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        pits(position.side)
            .filter(|hole| position.holes[*hole] > 0)
            .map(pit_name)
            .collect()
    }

    fn play(position: &mut MancalaPosition, notation: &str) -> Result<(), String> {
        let start =
            parse_pit(notation).ok_or_else(|| format!("Invalid move notation: {notation}"))?;
        let side = position.side;
        if !pits(side).contains(&start) {
            return Err(format!("Pit {notation} belongs to the opponent"));
        }
        let mut seeds = position.holes[start];
        if seeds == 0 {
            return Err(format!("Pit {notation} is empty"));
        }

        position.holes[start] = 0;
        let mut hole = start;
        while seeds > 0 {
            hole = (hole + 1) % HOLES;
            if hole == store(side.other()) {
                continue;
            }
            position.holes[hole] += 1;
            seeds -= 1;
        }

        position.extra_turn = hole == store(side);
        // The last seed landed in an empty pit of the mover's own.
        if pits(side).contains(&hole)
            && position.holes[hole] == 1
            && position.holes[opposite(hole)] > 0
        {
            let captured = position.holes[hole] + position.holes[opposite(hole)];
            position.holes[hole] = 0;
            position.holes[opposite(hole)] = 0;
            position.holes[store(side)] += captured;
        }

        if position.side_empty(Side::Black) || position.side_empty(Side::White) {
            for owner in [Side::Black, Side::White] {
                let remaining: u8 = pits(owner).map(|pit| position.holes[pit]).sum();
                pits(owner).for_each(|pit| position.holes[pit] = 0);
                position.holes[store(owner)] += remaining;
            }
            position.extra_turn = false;
            position.outcome = match position.holes[BLACK_STORE].cmp(&position.holes[WHITE_STORE]) {
                std::cmp::Ordering::Greater => Outcome::Win(Side::Black),
                std::cmp::Ordering::Less => Outcome::Win(Side::White),
                std::cmp::Ordering::Equal => Outcome::Draw,
            };
        } else if !position.extra_turn {
            position.side = side.other();
        }
        Ok(())
    }

    fn outcome(position: &MancalaPosition) -> Outcome {
        position.outcome
    }

    /// Seed counts in sowing order, comma-separated: `a`–`f`, Black's store,
    /// `g`–`l`, White's store.
    fn render(position: &MancalaPosition) -> String {
        position
            .holes
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    fn extra_metadata(position: &MancalaPosition, metadata: &mut HashMap<String, JsonValue>) {
        metadata.insert(
            "black_store".into(),
            JsonValue::from(position.holes[BLACK_STORE]),
        );
        metadata.insert(
            "white_store".into(),
            JsonValue::from(position.holes[WHITE_STORE]),
        );
        metadata.insert("extra_turn".into(), JsonValue::Bool(position.extra_turn));
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Mancala LRGP application.
    MancalaApp,
    MancalaRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::play_checked;

    fn position(side: Side, holes: [u8; HOLES]) -> MancalaPosition {
        MancalaPosition {
            holes,
            side,
            outcome: Outcome::Ongoing,
            extra_turn: false,
        }
    }

    #[test]
    fn notation_is_strict() {
        for (letter, hole) in [("a", 0), ("f", 5), ("g", 7), ("l", 12)] {
            assert_eq!(parse_pit(letter), Some(hole));
            assert_eq!(pit_name(hole), letter);
        }
        for bad in ["", "m", "A", "aa", "a1", " a", "1"] {
            assert!(!MancalaRules::is_canonical(bad), "accepted {bad:?}");
        }
        assert_eq!(opposite(0), 12);
        assert_eq!(opposite(5), 7);
    }

    #[test]
    fn opening_position_and_moves() {
        let start = MancalaRules::start();
        assert_eq!(MancalaRules::render(&start), "4,4,4,4,4,4,0,4,4,4,4,4,4,0");
        assert_eq!(
            MancalaRules::legal_moves(&start),
            ["a", "b", "c", "d", "e", "f"]
        );
    }

    #[test]
    fn sowing_passes_the_turn() {
        let mut game = MancalaRules::start();
        play_checked::<MancalaRules>(&mut game, "a").unwrap();
        assert_eq!(MancalaRules::render(&game), "0,5,5,5,5,4,0,4,4,4,4,4,4,0");
        assert_eq!(MancalaRules::side_to_move(&game), Side::White);
        assert_eq!(
            MancalaRules::legal_moves(&game),
            ["g", "h", "i", "j", "k", "l"]
        );
        assert!(play_checked::<MancalaRules>(&mut game, "a").is_err());
        assert!(play_checked::<MancalaRules>(&mut game, "b").is_err());
    }

    #[test]
    fn ending_in_the_own_store_moves_again() {
        // c holds four seeds: d, e, f, store.
        let mut game = MancalaRules::start();
        play_checked::<MancalaRules>(&mut game, "c").unwrap();
        assert_eq!(MancalaRules::render(&game), "4,4,0,5,5,5,1,4,4,4,4,4,4,0");
        assert!(game.extra_turn);
        assert_eq!(MancalaRules::side_to_move(&game), Side::Black);
        let mut metadata = HashMap::new();
        MancalaRules::extra_metadata(&game, &mut metadata);
        assert_eq!(metadata["black_store"], JsonValue::from(1));
        assert_eq!(metadata["extra_turn"], JsonValue::Bool(true));

        play_checked::<MancalaRules>(&mut game, "a").unwrap();
        assert!(!game.extra_turn);
        assert_eq!(MancalaRules::side_to_move(&game), Side::White);
    }

    #[test]
    fn sowing_skips_the_opponent_store() {
        // Ten seeds from l: White's store, a–f, (Black's store skipped), g, h, i.
        let mut holes = [1; HOLES];
        holes[BLACK_STORE] = 0;
        holes[WHITE_STORE] = 0;
        holes[12] = 10;
        let mut game = position(Side::White, holes);
        play_checked::<MancalaRules>(&mut game, "l").unwrap();
        assert_eq!(MancalaRules::render(&game), "2,2,2,2,2,2,0,2,2,2,1,1,0,1");
    }

    #[test]
    fn ending_in_an_own_empty_pit_captures_the_opposite_pit() {
        // a's single seed lands in empty b, facing k's five seeds.
        let mut game = position(Side::Black, [1, 0, 3, 0, 0, 0, 0, 2, 0, 0, 0, 5, 0, 0]);
        play_checked::<MancalaRules>(&mut game, "a").unwrap();
        assert_eq!(MancalaRules::render(&game), "0,0,3,0,0,0,6,2,0,0,0,0,0,0");
        assert_eq!(MancalaRules::side_to_move(&game), Side::White);

        // Facing an empty pit captures nothing.
        let mut game = position(Side::Black, [1, 0, 3, 0, 0, 0, 0, 2, 0, 0, 0, 0, 5, 0]);
        play_checked::<MancalaRules>(&mut game, "a").unwrap();
        assert_eq!(MancalaRules::render(&game), "0,1,3,0,0,0,0,2,0,0,0,0,5,0");
    }

    #[test]
    fn an_empty_side_ends_the_game_and_sweeps_the_rest() {
        // f's last seed reaches the store and empties Black's side; White
        // sweeps its six seeds and wins 7 to 6.
        let mut game = position(Side::Black, [0, 0, 0, 0, 0, 1, 5, 6, 0, 0, 0, 0, 0, 1]);
        play_checked::<MancalaRules>(&mut game, "f").unwrap();
        assert_eq!(MancalaRules::render(&game), "0,0,0,0,0,0,6,0,0,0,0,0,0,7");
        assert_eq!(MancalaRules::outcome(&game), Outcome::Win(Side::White));
        assert!(!game.extra_turn);
        assert!(MancalaRules::legal_moves(&game).is_empty());

        let mut draw = position(Side::Black, [0, 0, 0, 0, 0, 1, 6, 6, 0, 0, 0, 0, 0, 1]);
        play_checked::<MancalaRules>(&mut draw, "f").unwrap();
        assert_eq!(MancalaRules::outcome(&draw), Outcome::Draw);
    }

    placement::placement_lifecycle_tests!(crate::apps::mancala::MancalaRules);
}
