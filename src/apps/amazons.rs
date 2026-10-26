//! The Game of the Amazons on a 10x10 board. A move slides one of your four
//! amazons like a chess queen and then shoots an arrow from where it landed,
//! burning that square for good. The player left without a move loses. Wire
//! moves are `{m, n, x}` with `m` such as `d1d7/g7`; SPEC.md Appendix G.6
//! defines the rules.

use super::placement::{
    self, EMPTY, Outcome, PlacementRules, Side, parse_square, render_grid, square_name,
};

pub const APP_ID: &str = "amazons";
pub const APP_VERSION: u32 = 1;
pub const SIZE: usize = 10;

const CELLS: usize = SIZE * SIZE;
const ARROW: u8 = b'x';
const R_BLOCKED: &str = "nm";

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
pub(crate) struct AmazonsPosition {
    cells: [u8; CELLS],
    ply: usize,
    outcome: Outcome,
}

/// Squares a queen on `from` reaches over empty squares.
fn reach(cells: &[u8; CELLS], from: usize) -> Vec<usize> {
    let mut found = Vec::new();
    for (df, dr) in DIRECTIONS {
        let (mut file, mut rank) = ((from % SIZE) as isize, (from / SIZE) as isize);
        loop {
            file += df;
            rank += dr;
            let range = 0..SIZE as isize;
            if !range.contains(&file) || !range.contains(&rank) {
                break;
            }
            let square = rank as usize * SIZE + file as usize;
            if cells[square] != EMPTY {
                break;
            }
            found.push(square);
        }
    }
    found
}

/// `<from><to>/<arrow>`, each a square with no separator between the first two.
fn parse_move(notation: &str) -> Option<(usize, usize, usize)> {
    let (slide, arrow) = notation.split_once('/')?;
    let split = slide
        .char_indices()
        .skip(1)
        .find(|(_, c)| c.is_ascii_lowercase())?
        .0;
    let (from, to) = slide.split_at(split);
    Some((
        parse_square(from, SIZE)?,
        parse_square(to, SIZE)?,
        parse_square(arrow, SIZE)?,
    ))
}

fn notation((from, to, arrow): (usize, usize, usize)) -> String {
    format!(
        "{}{}/{}",
        square_name(from, SIZE),
        square_name(to, SIZE),
        square_name(arrow, SIZE)
    )
}

impl AmazonsPosition {
    fn moves(&self, side: Side) -> Vec<(usize, usize, usize)> {
        let mut found = Vec::new();
        for from in (0..CELLS).filter(|&s| self.cells[s] == side.stone()) {
            let mut lifted = self.cells;
            lifted[from] = EMPTY;
            for to in reach(&lifted, from) {
                for arrow in reach(&lifted, to) {
                    found.push((from, to, arrow));
                }
            }
        }
        found
    }

    fn has_move(&self, side: Side) -> bool {
        // Any queen with an empty neighbour can move there and shoot back.
        (0..CELLS)
            .filter(|&s| self.cells[s] == side.stone())
            .any(|from| !reach(&self.cells, from).is_empty())
    }
}

pub(crate) struct AmazonsRules;

impl PlacementRules for AmazonsRules {
    type Position = AmazonsPosition;

    const APP_ID: &'static str = APP_ID;
    const DISPLAY_NAME: &'static str = "Amazons";
    #[cfg(test)]
    const LONGEST_MOVE: &'static str = "a10j10/a10";
    #[cfg(test)]
    const MAX_PLIES: usize = CELLS;
    const CAN_DRAW: bool = false;
    const WIN_REASON: &'static str = R_BLOCKED;
    const DRAW_REASON: &'static str = "";

    fn start() -> AmazonsPosition {
        let mut cells = [EMPTY; CELLS];
        for name in ["a4", "d1", "g1", "j4"] {
            cells[parse_square(name, SIZE).unwrap_or_default()] = Side::Black.stone();
        }
        for name in ["a7", "d10", "g10", "j7"] {
            cells[parse_square(name, SIZE).unwrap_or_default()] = Side::White.stone();
        }
        AmazonsPosition {
            cells,
            ply: 0,
            outcome: Outcome::Ongoing,
        }
    }

    fn side_to_move(position: &AmazonsPosition) -> Side {
        if position.ply & 1 == 0 {
            Side::Black
        } else {
            Side::White
        }
    }

    fn is_canonical(text: &str) -> bool {
        parse_move(text).is_some_and(|mv| notation(mv) == text)
    }

    fn legal_moves(position: &AmazonsPosition) -> Vec<String> {
        if position.outcome != Outcome::Ongoing {
            return Vec::new();
        }
        let mut moves: Vec<String> = position
            .moves(Self::side_to_move(position))
            .into_iter()
            .map(notation)
            .collect();
        moves.sort_unstable();
        moves
    }

    fn play(position: &mut AmazonsPosition, text: &str) -> Result<(), String> {
        let side = Self::side_to_move(position);
        let (from, to, arrow) =
            parse_move(text).ok_or_else(|| format!("Invalid move notation: {text}"))?;
        if position.cells[from] != side.stone() {
            return Err(format!("No amazon of yours on {}", square_name(from, SIZE)));
        }
        let mut lifted = position.cells;
        lifted[from] = EMPTY;
        if !reach(&lifted, from).contains(&to) {
            return Err(format!("The amazon cannot reach {}", square_name(to, SIZE)));
        }
        if !reach(&lifted, to).contains(&arrow) {
            return Err(format!(
                "The arrow cannot reach {}",
                square_name(arrow, SIZE)
            ));
        }
        position.cells = lifted;
        position.cells[to] = side.stone();
        position.cells[arrow] = ARROW;
        position.ply += 1;
        if !position.has_move(side.other()) {
            position.outcome = Outcome::Win(side);
        }
        Ok(())
    }

    fn outcome(position: &AmazonsPosition) -> Outcome {
        position.outcome
    }

    fn render(position: &AmazonsPosition) -> String {
        render_grid(&position.cells, SIZE)
    }

    #[cfg(any(test, feature = "test-helpers"))]
    fn coin_pin() -> &'static std::sync::Mutex<Option<bool>> {
        &FORCED_COIN
    }
}

placement::placement_app!(
    /// The built-in Amazons LRGP application.
    AmazonsApp,
    AmazonsRules
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::placement::play_checked;

    fn sq(name: &str) -> usize {
        parse_square(name, SIZE).unwrap()
    }

    #[test]
    fn notation_is_strict() {
        for good in ["d1d7/g7", "a10j10/a10", "a4a5/a4"] {
            assert!(AmazonsRules::is_canonical(good), "rejected {good:?}");
        }
        for bad in [
            "", "d1d7", "d1-d7/g7", "d1d7/", "d1d7/g7/", "d01d7/g7", "k1d7/g7", "D1d7/g7",
            "d1d11/g7",
        ] {
            assert!(!AmazonsRules::is_canonical(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_opening_has_2176_moves() {
        let start = AmazonsRules::start();
        let legal = AmazonsRules::legal_moves(&start);
        assert_eq!(legal.len(), 2176);
        let mut sorted = legal.clone();
        sorted.sort();
        assert_eq!(legal, sorted);
        assert!(legal.contains(&"d1d7/g7".to_string()));
        // The arrow may land on the square the amazon left.
        assert!(legal.contains(&"d1d2/d1".to_string()));
    }

    #[test]
    fn amazons_and_arrows_cannot_pass_through_anything() {
        let mut position = AmazonsRules::start();
        play_checked::<AmazonsRules>(&mut position, "d1d7/g7").unwrap();
        assert_eq!(position.cells[sq("g7")], ARROW);
        assert_eq!(position.cells[sq("d1")], EMPTY);
        // White's g10 can't slide through the arrow on g7.
        for blocked in ["g10g6/g5", "d10d6/d5", "a7c7/i7"] {
            assert!(
                play_checked::<AmazonsRules>(&mut position.clone(), blocked).is_err(),
                "{blocked}"
            );
        }
        assert!(play_checked::<AmazonsRules>(&mut position.clone(), "g10g8/g9").is_ok());
        // Not your amazon, not a queen line.
        assert!(play_checked::<AmazonsRules>(&mut position.clone(), "a4a5/a6").is_err());
        assert!(play_checked::<AmazonsRules>(&mut position, "a7b9/b10").is_err());
    }

    #[test]
    fn the_player_left_without_a_move_loses() {
        // Everything is burnt but White's a1, Black's c3, and a2 and b2.
        let mut position = AmazonsRules::start();
        position.cells = [ARROW; CELLS];
        position.cells[sq("a1")] = b'w';
        position.cells[sq("c3")] = b'b';
        position.cells[sq("a2")] = EMPTY;
        position.cells[sq("b2")] = EMPTY;
        assert!(position.has_move(Side::White));
        assert_eq!(AmazonsRules::legal_moves(&position), ["c3b2/a2", "c3b2/c3"]);

        let mut open = position.clone();
        play_checked::<AmazonsRules>(&mut open, "c3b2/c3").unwrap();
        assert_eq!(
            AmazonsRules::outcome(&open),
            Outcome::Ongoing,
            "a1 can still reach a2"
        );

        play_checked::<AmazonsRules>(&mut position, "c3b2/a2").unwrap();
        assert_eq!(AmazonsRules::outcome(&position), Outcome::Win(Side::Black));
        assert_eq!(AmazonsRules::reason(&position), R_BLOCKED);
        assert!(AmazonsRules::legal_moves(&position).is_empty());
    }

    placement::placement_lifecycle_tests!(crate::apps::amazons::AmazonsRules);
}
