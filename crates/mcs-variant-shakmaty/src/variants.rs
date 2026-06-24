//! The concrete variant specifications.
//!
//! Each variant is a zero-sized marker type implementing
//! [`VariantSpec`](crate::spec::VariantSpec). Their only job is to pin a
//! shakmaty position type, declare an id and display name, build the starting
//! position, and describe how a decisive board ending maps to an
//! [`EndReason`]. All gameplay lives in
//! [`ShakmatyGame`](crate::game::ShakmatyGame).

use std::str::FromStr;

use mcs_core::{Color, EndReason, GameError, VariantOptions};
use serde::Deserialize;
use shakmaty::fen::Fen;
use shakmaty::variant;
use shakmaty::{CastlingMode, Chess, Position};

use crate::spec::VariantSpec;

/// Builds the fixed default starting position for a variant whose
/// [`VariantSpec::Position`] implements [`Default`], ignoring any options.
fn default_start<P: Default>(_options: &VariantOptions) -> Result<P, GameError> {
    Ok(P::default())
}

/// King of the Hill: a `winner` reached the centre (or delivered mate).
///
/// shakmaty does not tell us *which* of the two decisive conditions fired, and
/// both are wins for the same side, so we report the variant-defining reason.
#[derive(Debug, Default, Clone, Copy)]
pub struct KingOfTheHill;

impl VariantSpec for KingOfTheHill {
    type Position = variant::KingOfTheHill;
    const ID: &'static str = "kingofthehill";
    const DISPLAY_NAME: &'static str = "King of the Hill";

    fn starting_position(options: &VariantOptions) -> Result<Self::Position, GameError> {
        default_start(options)
    }

    fn decisive_reason(_winner: Color, position: &Self::Position) -> EndReason {
        if position.variant_outcome().is_some() {
            // No exact enum case exists for "king reached the centre".
            EndReason::Other("king_in_the_center".to_owned())
        } else {
            EndReason::Checkmate
        }
    }
}

/// Three-check: `winner` delivered the third check (or mate).
#[derive(Debug, Default, Clone, Copy)]
pub struct ThreeCheck;

impl VariantSpec for ThreeCheck {
    type Position = variant::ThreeCheck;
    const ID: &'static str = "threecheck";
    const DISPLAY_NAME: &'static str = "Three-check";

    fn starting_position(options: &VariantOptions) -> Result<Self::Position, GameError> {
        default_start(options)
    }

    fn decisive_reason(_winner: Color, position: &Self::Position) -> EndReason {
        if position.variant_outcome().is_some() {
            EndReason::Other("three_checks".to_owned())
        } else {
            EndReason::Checkmate
        }
    }
}

/// Atomic chess: capturing detonates, and `winner` exploded the enemy king (or
/// delivered mate).
#[derive(Debug, Default, Clone, Copy)]
pub struct Atomic;

impl VariantSpec for Atomic {
    type Position = variant::Atomic;
    const ID: &'static str = "atomic";
    const DISPLAY_NAME: &'static str = "Atomic";

    fn starting_position(options: &VariantOptions) -> Result<Self::Position, GameError> {
        default_start(options)
    }

    fn decisive_reason(_winner: Color, position: &Self::Position) -> EndReason {
        if position.variant_outcome().is_some() {
            // The enemy king was caught in an explosion.
            EndReason::Other("king_exploded".to_owned())
        } else {
            // Ordinary atomic checkmate.
            EndReason::Checkmate
        }
    }
}

/// Antichess: the goal is inverted — `winner` lost all their pieces or was
/// stalemated, which shakmaty reports as a decisive result.
#[derive(Debug, Default, Clone, Copy)]
pub struct Antichess;

impl VariantSpec for Antichess {
    type Position = variant::Antichess;
    const ID: &'static str = "antichess";
    const DISPLAY_NAME: &'static str = "Antichess";

    fn starting_position(options: &VariantOptions) -> Result<Self::Position, GameError> {
        default_start(options)
    }

    fn decisive_reason(_winner: Color, _position: &Self::Position) -> EndReason {
        // Antichess has no king: every decisive ending is the inverted goal —
        // the winner has lost all pieces or has no move. shakmaty surfaces both
        // through the standard no-legal-moves path, so there is nothing to
        // branch on here.
        EndReason::Other("antichess_goal".to_owned())
    }
}

/// Racing Kings: `winner` raced their king to the eighth rank.
#[derive(Debug, Default, Clone, Copy)]
pub struct RacingKings;

impl VariantSpec for RacingKings {
    type Position = variant::RacingKings;
    const ID: &'static str = "racingkings";
    const DISPLAY_NAME: &'static str = "Racing Kings";

    fn starting_position(options: &VariantOptions) -> Result<Self::Position, GameError> {
        default_start(options)
    }

    fn decisive_reason(_winner: Color, position: &Self::Position) -> EndReason {
        if position.variant_outcome().is_some() {
            EndReason::Other("king_reached_rank8".to_owned())
        } else {
            EndReason::Checkmate
        }
    }
}

/// Horde: White commands a horde of pawns; `winner` either mated Black's army or
/// captured the entire horde.
#[derive(Debug, Default, Clone, Copy)]
pub struct Horde;

impl VariantSpec for Horde {
    type Position = variant::Horde;
    const ID: &'static str = "horde";
    const DISPLAY_NAME: &'static str = "Horde";

    fn starting_position(options: &VariantOptions) -> Result<Self::Position, GameError> {
        default_start(options)
    }

    fn decisive_reason(_winner: Color, position: &Self::Position) -> EndReason {
        if position.variant_outcome().is_some() {
            // Black captured the entire horde.
            EndReason::Other("horde_destroyed".to_owned())
        } else {
            // Either side delivered an ordinary checkmate.
            EndReason::Checkmate
        }
    }
}

/// Crazyhouse: captured pieces switch sides and can be dropped back in. The only
/// decisive board ending is checkmate, so the default reason applies.
#[derive(Debug, Default, Clone, Copy)]
pub struct Crazyhouse;

impl VariantSpec for Crazyhouse {
    type Position = variant::Crazyhouse;
    const ID: &'static str = "crazyhouse";
    const DISPLAY_NAME: &'static str = "Crazyhouse";

    fn starting_position(options: &VariantOptions) -> Result<Self::Position, GameError> {
        default_start(options)
    }
    // decisive_reason defaults to Checkmate, which is correct for Crazyhouse.
}

/// Options accepted by [`Chess960::new_game`](crate::ShakmatyVariant::new_game).
///
/// All fields are optional and mutually exclusive in spirit; if both are given,
/// `fen` takes precedence. With neither (the default), the standard chess
/// starting position (Chess960 number 518) is used.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Chess960Options {
    /// A Chess960 starting-position number in `0..=959` (Scharnagl numbering),
    /// where `518` is the standard chess setup.
    position: Option<u16>,
    /// An explicit starting FEN, parsed with Chess960 castling rules. Overrides
    /// `position` when present.
    fen: Option<String>,
}

/// Chess960 (Fischer Random).
///
/// Chess960 is not a distinct shakmaty position type: it is ordinary
/// [`Chess`] set up from one of 960 shuffled back-rank arrangements and played
/// with [`CastlingMode::Chess960`] so castling targets the rook's file rather
/// than fixed squares.
///
/// ## Starting position
///
/// `new_game` reads its [`VariantOptions`] as [`Chess960Options`]:
///
/// - `{ "position": <0..=959> }` selects a Scharnagl-numbered setup;
/// - `{ "fen": "<fen>" }` uses an explicit position (Chess960 castling);
/// - **the default** (empty/`null` options) is Chess960 number **518**, which is
///   the standard chess starting position. We deliberately default to a fixed,
///   reproducible position rather than a random one so that game creation is
///   deterministic; callers wanting a random game pick a random `position`
///   themselves.
#[derive(Debug, Default, Clone, Copy)]
pub struct Chess960;

impl Chess960 {
    /// The Chess960 number of the standard chess starting position.
    pub const STANDARD_POSITION: u16 = 518;
}

impl VariantSpec for Chess960 {
    type Position = Chess;
    const ID: &'static str = "chess960";
    const DISPLAY_NAME: &'static str = "Chess960";

    fn starting_position(options: &VariantOptions) -> Result<Self::Position, GameError> {
        let opts: Chess960Options = if options.as_value().is_null() {
            Chess960Options::default()
        } else {
            options.to_typed().map_err(|e| {
                GameError::InvalidActionPayload(format!("invalid chess960 options: {e}"))
            })?
        };

        let fen = match opts.fen {
            Some(fen) => Fen::from_str(&fen)
                .map_err(|e| GameError::InvalidActionPayload(format!("invalid FEN: {e}")))?,
            None => {
                let number = opts.position.unwrap_or(Self::STANDARD_POSITION);
                if number > 959 {
                    return Err(GameError::InvalidActionPayload(format!(
                        "chess960 position {number} is out of range (expected 0..=959)"
                    )));
                }
                Fen::from_str(&chess960_start_fen(number))
                    .expect("generated chess960 FEN is always well-formed")
            }
        };

        fen.into_position::<Chess>(CastlingMode::Chess960)
            .map_err(|e| GameError::InvalidActionPayload(format!("illegal chess960 position: {e}")))
    }
    // decisive_reason defaults to Checkmate, which is correct for Chess960.
}

/// Produces the full starting FEN for Chess960 position `number` (`0..=959`),
/// using the standard Scharnagl numbering.
///
/// The back rank is derived as follows (per the Chess960 numbering scheme):
/// the number is decomposed to place the two bishops on opposite colours, the
/// queen, then the two knights, and finally the king and rooks (R K R) into the
/// three remaining squares so that the king always sits between its rooks.
fn chess960_start_fen(number: u16) -> String {
    let rank = chess960_back_rank(number);
    let white: String = rank.iter().map(|c| c.to_ascii_uppercase()).collect();
    let black: String = rank.iter().collect();
    // Castling rights use the KQkq shorthand; shakmaty maps it onto the actual
    // rook files for Chess960 castling.
    format!("{black}/pppppppp/8/8/8/8/PPPPPPPP/{white} w KQkq - 0 1")
}

/// Computes the eight back-rank piece letters (lowercase) for Chess960
/// `number`, ordered a-file to h-file.
fn chess960_back_rank(number: u16) -> [char; 8] {
    let mut rank = ['\0'; 8];
    let n = number;

    // Light-squared bishop: files b, d, f, h (the four light squares of the
    // back rank), indexed by n % 4.
    let n2 = n / 4;
    let b1 = n % 4;
    rank[[1, 3, 5, 7][b1 as usize]] = 'b';

    // Dark-squared bishop: files a, c, e, g, indexed by n2 % 4.
    let n3 = n2 / 4;
    let b2 = n2 % 4;
    rank[[0, 2, 4, 6][b2 as usize]] = 'b';

    // Queen goes on the (n3 % 6)-th empty square.
    let n4 = n3 / 6;
    let q = n3 % 6;
    place_on_nth_empty(&mut rank, q as usize, 'q');

    // The two knights occupy two of the five remaining empty squares, chosen by
    // the standard lookup table over the 10 (= C(5,2)) combinations.
    const KNIGHT_TABLE: [(usize, usize); 10] = [
        (0, 1),
        (0, 2),
        (0, 3),
        (0, 4),
        (1, 2),
        (1, 3),
        (1, 4),
        (2, 3),
        (2, 4),
        (3, 4),
    ];
    let (k1, k2) = KNIGHT_TABLE[n4 as usize];
    // Place the second knight first so the first index is not shifted by the
    // earlier insertion.
    place_on_nth_empty(&mut rank, k2, 'n');
    place_on_nth_empty(&mut rank, k1, 'n');

    // The three squares that remain take R K R, left to right. Each placement
    // fills the first remaining empty square, so the index is always 0.
    for piece in ['r', 'k', 'r'] {
        place_on_nth_empty(&mut rank, 0, piece);
    }

    rank
}

/// Places `piece` on the `index`-th still-empty (`'\0'`) square of `rank`.
fn place_on_nth_empty(rank: &mut [char; 8], index: usize, piece: char) {
    let mut seen = 0;
    for slot in rank.iter_mut() {
        if *slot == '\0' {
            if seen == index {
                *slot = piece;
                return;
            }
            seen += 1;
        }
    }
    unreachable!("nth-empty index {index} out of range for a back rank");
}
