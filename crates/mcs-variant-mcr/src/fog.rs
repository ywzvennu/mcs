//! Per-player visibility redaction for Fog of War (Dark Chess).
//!
//! Fog of War keeps every standard-chess move but hides the board: each side
//! sees only the squares its own pieces occupy or **pseudo-attack**. mcr's
//! [`Game`](mcr::Game) seam serves the *full* position (a plain-chess FEN) and
//! the full legal-move list — it deliberately does **not** redact, because the
//! fog is a rendering concern with no effect on the rules or perft. The adapter
//! is therefore where the fog is drawn: this module recomputes the visibility
//! mask from the full FEN and blanks every opponent piece standing on a square
//! the viewer cannot see, mirroring mcr's own `FogOfWar::visible_squares`
//! (`by_color(color) | attacked_by(color, occupied)`).
//!
//! The visibility is **attack-based**, exactly as mcr defines it: a square is
//! visible to a colour when one of its pieces occupies it or pseudo-attacks it
//! (pawns use their diagonal *capture* pattern, so a square a pawn could only
//! push to is not, by itself, revealed). This module implements the standard
//! 8x8 attack patterns directly, since the `Game` seam exposes neither the board
//! nor the attack sets — only the FEN string, which for Fog of War is byte-for-
//! byte the standard chess dialect.

use mcs_core::Color;

/// The canonical mcr catalog name of Fog of War, the one hidden-information
/// variant this adapter redacts.
pub(crate) const FOG_OF_WAR_ID: &str = "fogofwar";

/// A standard chess piece kind, parsed from a FEN piece letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pawn,
    Knight,
    Bishop,
    Rook,
    Queen,
    King,
}

/// A piece on the board: its colour and kind.
#[derive(Debug, Clone, Copy)]
struct Piece {
    color: Color,
    kind: Kind,
}

/// A parsed 8x8 board. `squares[rank * 8 + file]` is the piece on that square,
/// with rank 0 the side-to-move-agnostic White home rank (FEN rank 1) and file 0
/// the a-file.
struct Board {
    squares: [Option<Piece>; 64],
}

/// The eight knight jumps, as `(file, rank)` deltas.
const KNIGHT_DELTAS: [(i32, i32); 8] = [
    (1, 2),
    (2, 1),
    (2, -1),
    (1, -2),
    (-1, -2),
    (-2, -1),
    (-2, 1),
    (-1, 2),
];

/// The eight king / queen steps, as `(file, rank)` deltas.
const KING_DELTAS: [(i32, i32); 8] = [
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
];

/// The four rook ray directions.
const ROOK_DIRS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

/// The four bishop ray directions.
const BISHOP_DIRS: [(i32, i32); 4] = [(1, 1), (1, -1), (-1, 1), (-1, -1)];

impl Piece {
    /// Parses a FEN piece letter into a [`Piece`], or `None` if it is not a
    /// standard piece letter (Fog of War fields only ever carry these).
    fn from_fen_char(ch: char) -> Option<Piece> {
        let color = if ch.is_ascii_uppercase() {
            Color::White
        } else {
            Color::Black
        };
        let kind = match ch.to_ascii_lowercase() {
            'p' => Kind::Pawn,
            'n' => Kind::Knight,
            'b' => Kind::Bishop,
            'r' => Kind::Rook,
            'q' => Kind::Queen,
            'k' => Kind::King,
            _ => return None,
        };
        Some(Piece { color, kind })
    }

    /// The FEN letter for this piece (uppercase for White, lowercase for Black).
    fn to_fen_char(self) -> char {
        let base = match self.kind {
            Kind::Pawn => 'p',
            Kind::Knight => 'n',
            Kind::Bishop => 'b',
            Kind::Rook => 'r',
            Kind::Queen => 'q',
            Kind::King => 'k',
        };
        if self.color == Color::White {
            base.to_ascii_uppercase()
        } else {
            base
        }
    }
}

impl Board {
    /// Parses the placement field (the first, `/`-separated field) of a standard
    /// FEN into a [`Board`]. Ranks are listed from 8 down to 1; unknown letters
    /// leave the square empty (defensive — Fog of War never emits them).
    fn parse(placement: &str) -> Board {
        let mut squares = [None; 64];
        // FEN lists rank 8 first, so the first row maps to board rank index 7.
        for (row, rank_field) in placement.split('/').enumerate() {
            if row >= 8 {
                break;
            }
            let rank = 7 - row;
            let mut file = 0usize;
            for ch in rank_field.chars() {
                if file >= 8 {
                    break;
                }
                if let Some(skip) = ch.to_digit(10) {
                    file += skip as usize;
                } else if let Some(piece) = Piece::from_fen_char(ch) {
                    squares[rank * 8 + file] = Some(piece);
                    file += 1;
                } else {
                    file += 1;
                }
            }
        }
        Board { squares }
    }

    /// Whether any piece stands on `(file, rank)`.
    fn occupied(&self, file: i32, rank: i32) -> bool {
        (0..8).contains(&file)
            && (0..8).contains(&rank)
            && self.squares[(rank * 8 + file) as usize].is_some()
    }

    /// Marks every square `color`'s piece at `(file, rank)` of `kind`
    /// pseudo-attacks into `visible` (stepping pieces by offset, sliders by ray
    /// until the first blocker inclusive). Occupancy is read from the board, so
    /// sliders stop correctly on the full position.
    fn mark_attacks(&self, file: i32, rank: i32, piece: Piece, visible: &mut [bool; 64]) {
        let mut mark = |f: i32, r: i32| {
            if (0..8).contains(&f) && (0..8).contains(&r) {
                visible[(r * 8 + f) as usize] = true;
            }
        };
        match piece.kind {
            Kind::Pawn => {
                // Pawns see their two diagonal capture squares (White forward is
                // +rank, Black forward is -rank), never the push square.
                let dr = if piece.color == Color::White { 1 } else { -1 };
                mark(file - 1, rank + dr);
                mark(file + 1, rank + dr);
            }
            Kind::Knight => {
                for (df, dr) in KNIGHT_DELTAS {
                    mark(file + df, rank + dr);
                }
            }
            Kind::King => {
                for (df, dr) in KING_DELTAS {
                    mark(file + df, rank + dr);
                }
            }
            Kind::Bishop | Kind::Rook | Kind::Queen => {
                let dirs: &[(i32, i32)] = match piece.kind {
                    Kind::Bishop => &BISHOP_DIRS,
                    Kind::Rook => &ROOK_DIRS,
                    _ => &KING_DELTAS, // queen: all eight directions
                };
                for &(df, dr) in dirs {
                    let (mut f, mut r) = (file + df, rank + dr);
                    while (0..8).contains(&f) && (0..8).contains(&r) {
                        mark(f, r);
                        // A ray stops at (and reveals) the first occupied square.
                        if self.occupied(f, r) {
                            break;
                        }
                        f += df;
                        r += dr;
                    }
                }
            }
        }
    }

    /// The visibility mask for `viewer`: every square its own pieces occupy, plus
    /// every square any of its pieces pseudo-attacks. Matches mcr's
    /// `FogOfWar::visible_squares`.
    fn visible_squares(&self, viewer: Color) -> [bool; 64] {
        let mut visible = [false; 64];
        for rank in 0..8 {
            for file in 0..8 {
                if let Some(piece) = self.squares[(rank * 8 + file) as usize] {
                    if piece.color == viewer {
                        // A piece always sees its own square...
                        visible[(rank * 8 + file) as usize] = true;
                        // ...and every square it pseudo-attacks.
                        self.mark_attacks(file, rank, piece, &mut visible);
                    }
                }
            }
        }
        visible
    }

    /// Renders the placement field showing only what `viewer` may see: all of
    /// `viewer`'s own pieces, plus any opponent piece standing on a visible
    /// square. Opponent pieces on unseen squares are blanked (rendered empty),
    /// so they never appear in the redacted bytes.
    fn redacted_placement(&self, viewer: Color, visible: &[bool; 64]) -> String {
        let mut out = String::new();
        for row in 0..8 {
            let rank = 7 - row;
            let mut empties = 0u32;
            for file in 0..8 {
                let sq = rank * 8 + file;
                let shown = match self.squares[sq] {
                    Some(piece) if piece.color == viewer => Some(piece),
                    Some(piece) if visible[sq] => Some(piece),
                    _ => None,
                };
                match shown {
                    Some(piece) => {
                        if empties > 0 {
                            out.push_str(&empties.to_string());
                            empties = 0;
                        }
                        out.push(piece.to_fen_char());
                    }
                    None => empties += 1,
                }
            }
            if empties > 0 {
                out.push_str(&empties.to_string());
            }
            if row < 7 {
                out.push('/');
            }
        }
        out
    }
}

/// The redacted piece-placement field for `viewer` derived from the full Fog of
/// War `fen`: `viewer`'s own pieces plus any opponent piece on a square one of
/// their pieces attacks; every unseen opponent piece is blanked out.
///
/// Only the placement (first FEN field) is returned — deliberately, as in the
/// RBC adapter — so no side-to-move, castling, or en-passant metadata that might
/// hint at the hidden board crosses the boundary.
pub(crate) fn redacted_placement_for(fen: &str, viewer: Color) -> String {
    let placement = fen.split_whitespace().next().unwrap_or("");
    let board = Board::parse(placement);
    let visible = board.visible_squares(viewer);
    board.redacted_placement(viewer, &visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

    #[test]
    fn start_position_reveals_no_enemy_pieces() {
        // From the start, neither side attacks past the fourth rank, so a player
        // sees only their own army — the redacted board must contain no piece of
        // the opponent's case.
        let white = redacted_placement_for(START, Color::White);
        assert!(
            !white.chars().any(|c| c.is_ascii_lowercase()),
            "White's fog view leaked a black piece: {white}"
        );
        // White still sees its whole own army (16 pieces).
        assert_eq!(white.chars().filter(|c| c.is_ascii_uppercase()).count(), 16);

        let black = redacted_placement_for(START, Color::Black);
        assert!(
            !black.chars().any(|c| c.is_ascii_uppercase()),
            "Black's fog view leaked a white piece: {black}"
        );
    }

    #[test]
    fn attacked_enemy_piece_is_revealed_but_shielded_one_is_hidden() {
        // White knight on e5 attacks d7 and f7 (black pawns there) but not the
        // rest of Black's army. The two attacked pawns are revealed; every other
        // black piece is blanked.
        let fen = "rnbqkbnr/pppppppp/8/4N3/8/8/PPPP1PPP/R1BQKBNR w KQkq - 0 1";
        let white = redacted_placement_for(fen, Color::White);
        // Exactly the two attacked black pawns survive redaction.
        assert_eq!(
            white.chars().filter(|c| *c == 'p').count(),
            2,
            "expected only the two attacked pawns, got: {white}"
        );
        // No black officer (back-rank piece) is ever revealed from here.
        for hidden in ['r', 'n', 'b', 'q', 'k'] {
            assert!(
                !white.contains(hidden),
                "fog view leaked hidden black '{hidden}': {white}"
            );
        }
    }

    #[test]
    fn slider_vision_stops_at_the_first_blocker() {
        // A white rook on a1 with its own pawn on a2 sees a2 (its own piece) but
        // nothing beyond it — the far black rook on a8 stays hidden.
        let fen = "r6k/8/8/8/8/8/P7/R6K w - - 0 1";
        let white = redacted_placement_for(fen, Color::White);
        assert!(
            !white.contains('r'),
            "rook vision leaked a shielded black rook: {white}"
        );
        // With the shielding pawn gone the rook sees all the way up the file, so
        // the black rook on a8 becomes visible.
        let open = "r6k/8/8/8/8/8/8/R6K w - - 0 1";
        let white_open = redacted_placement_for(open, Color::White);
        assert!(
            white_open.contains('r'),
            "open file should reveal the black rook: {white_open}"
        );
    }
}
