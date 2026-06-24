//! Conversions between `rbc-rs` types and the variant's string wire formats.
//!
//! `rbc-rs` works in terms of [`Square`], [`Move`], [`Piece`], and so on; the
//! wire layer speaks algebraic squares, UCI moves, and FEN piece characters.
//! This module is the single place those two worlds are translated, so the
//! session code in [`crate::game`] stays focused on game logic.

use mcs_core::{Color, GameError};
use rbc_rs::{Move, Piece, PieceKind, Square};

/// Maps an `rbc-rs` colour onto the core [`Color`].
pub(crate) fn from_rbc_color(color: rbc_rs::Color) -> Color {
    match color {
        rbc_rs::Color::White => Color::White,
        rbc_rs::Color::Black => Color::Black,
    }
}

/// Maps a core [`Color`] onto the `rbc-rs` colour.
pub(crate) fn to_rbc_color(color: Color) -> rbc_rs::Color {
    match color {
        Color::White => rbc_rs::Color::White,
        Color::Black => rbc_rs::Color::Black,
    }
}

/// The FEN piece character for a piece, e.g. `'P'` for a white pawn or `'n'`
/// for a black knight (white is upper-case, black lower-case).
pub(crate) fn piece_to_fen_char(piece: Piece) -> char {
    let base = match piece.kind {
        PieceKind::King => 'k',
        PieceKind::Queen => 'q',
        PieceKind::Rook => 'r',
        PieceKind::Bishop => 'b',
        PieceKind::Knight => 'n',
        PieceKind::Pawn => 'p',
    };
    match piece.color {
        rbc_rs::Color::White => base.to_ascii_uppercase(),
        rbc_rs::Color::Black => base,
    }
}

/// Parses a single algebraic square such as `"e4"` into an `rbc-rs` [`Square`].
///
/// # Errors
///
/// Returns [`GameError::IllegalAction`] if the string is not a two-character
/// `file` + `rank` coordinate inside the board.
pub(crate) fn square_from_algebraic(text: &str) -> Result<Square, GameError> {
    let bytes = text.as_bytes();
    if bytes.len() != 2 {
        return Err(GameError::IllegalAction);
    }
    let file = bytes[0];
    let rank = bytes[1];
    if !(b'a'..=b'h').contains(&file) || !(b'1'..=b'8').contains(&rank) {
        return Err(GameError::IllegalAction);
    }
    Square::from_coords(file - b'a', rank - b'1').ok_or(GameError::IllegalAction)
}

/// Renders a square as its algebraic coordinate, e.g. `"e4"`.
pub(crate) fn square_to_algebraic(square: Square) -> String {
    square.to_algebraic()
}

/// Maps a promotion suffix character (`'q'`, `'r'`, `'b'`, `'n'`) onto a
/// [`PieceKind`].
fn promotion_from_char(c: char) -> Result<PieceKind, GameError> {
    match c.to_ascii_lowercase() {
        'q' => Ok(PieceKind::Queen),
        'r' => Ok(PieceKind::Rook),
        'b' => Ok(PieceKind::Bishop),
        'n' => Ok(PieceKind::Knight),
        _ => Err(GameError::InvalidActionPayload(format!(
            "invalid promotion piece '{c}'"
        ))),
    }
}

/// The UCI promotion suffix for a promotion piece kind, if it is a legal
/// promotion target.
fn promotion_to_char(kind: PieceKind) -> Option<char> {
    match kind {
        PieceKind::Queen => Some('q'),
        PieceKind::Rook => Some('r'),
        PieceKind::Bishop => Some('b'),
        PieceKind::Knight => Some('n'),
        PieceKind::King | PieceKind::Pawn => None,
    }
}

/// Parses a UCI long-algebraic move string into an `rbc-rs` [`Move`].
///
/// Accepts the 4-character form (`"e2e4"`) and the 5-character promotion form
/// (`"e7e8q"`). `rbc-rs` exposes no UCI parser, so this is implemented here.
///
/// # Errors
///
/// Returns [`GameError::InvalidActionPayload`] if the string is not a valid UCI
/// move (wrong length, off-board square, or bad promotion suffix).
pub(crate) fn move_from_uci(uci: &str) -> Result<Move, GameError> {
    if uci.len() != 4 && uci.len() != 5 {
        return Err(GameError::InvalidActionPayload(format!(
            "invalid UCI move '{uci}': expected 4 or 5 characters"
        )));
    }
    let from = square_from_algebraic(&uci[0..2])
        .map_err(|_| GameError::InvalidActionPayload(format!("invalid UCI origin in '{uci}'")))?;
    let to = square_from_algebraic(&uci[2..4]).map_err(|_| {
        GameError::InvalidActionPayload(format!("invalid UCI destination in '{uci}'"))
    })?;
    let promotion = match uci.chars().nth(4) {
        Some(c) => Some(promotion_from_char(c)?),
        None => None,
    };
    Ok(Move {
        from,
        to,
        promotion,
    })
}

/// Renders an `rbc-rs` [`Move`] as a UCI long-algebraic string.
pub(crate) fn move_to_uci(mv: Move) -> String {
    let mut uci = String::with_capacity(5);
    uci.push_str(&square_to_algebraic(mv.from));
    uci.push_str(&square_to_algebraic(mv.to));
    if let Some(c) = mv.promotion.and_then(promotion_to_char) {
        uci.push(c);
    }
    uci
}

/// Builds a FEN piece-placement field that contains **only** the pieces
/// belonging to `viewer`, querying each square through `piece_at`.
///
/// This is the core of the hidden-information redaction: every square occupied
/// by the opponent is rendered as empty, so the resulting string cannot reveal
/// where the opponent's pieces are. The format is the standard FEN board field
/// (ranks 8 down to 1, files a to h, runs of empties collapsed to a digit).
pub(crate) fn own_pieces_fen(
    viewer: Color,
    mut piece_at: impl FnMut(Square) -> Option<Piece>,
) -> String {
    let viewer = to_rbc_color(viewer);
    let mut fen = String::new();
    for rank in (0u8..8).rev() {
        let mut empty_run = 0u32;
        for file in 0u8..8 {
            let square = Square::from_coords(file, rank).expect("file/rank in 0..8");
            match piece_at(square) {
                Some(piece) if piece.color == viewer => {
                    if empty_run > 0 {
                        fen.push(char::from_digit(empty_run, 10).expect("1..=8 is a digit"));
                        empty_run = 0;
                    }
                    fen.push(piece_to_fen_char(piece));
                }
                // Empty squares and opponent-occupied squares are both rendered
                // blank: the opponent's pieces are deliberately invisible here.
                _ => empty_run += 1,
            }
        }
        if empty_run > 0 {
            fen.push(char::from_digit(empty_run, 10).expect("1..=8 is a digit"));
        }
        if rank > 0 {
            fen.push('/');
        }
    }
    fen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_round_trips_through_algebraic() {
        for text in ["a1", "h8", "e4", "d5"] {
            let square = square_from_algebraic(text).unwrap();
            assert_eq!(square_to_algebraic(square), text);
        }
    }

    #[test]
    fn rejects_malformed_squares() {
        for bad in ["", "e", "e9", "i4", "44", "e44"] {
            assert!(
                square_from_algebraic(bad).is_err(),
                "{bad} should be invalid"
            );
        }
    }

    #[test]
    fn move_round_trips_through_uci() {
        for uci in ["e2e4", "e7e8q", "a7a8n", "b1c3"] {
            let mv = move_from_uci(uci).unwrap();
            assert_eq!(move_to_uci(mv), uci);
        }
    }

    #[test]
    fn rejects_malformed_uci() {
        for bad in ["e2e", "e2e4k", "e2e4qq", "z2e4"] {
            assert!(move_from_uci(bad).is_err(), "{bad} should be invalid");
        }
    }

    #[test]
    fn own_pieces_fen_hides_opponent() {
        // A white pawn on e2 and a black pawn on e7. From white's perspective
        // only the white pawn appears; the black pawn's square is empty.
        let white_pawn = Piece {
            color: rbc_rs::Color::White,
            kind: PieceKind::Pawn,
        };
        let black_pawn = Piece {
            color: rbc_rs::Color::Black,
            kind: PieceKind::Pawn,
        };
        let e2 = square_from_algebraic("e2").unwrap();
        let e7 = square_from_algebraic("e7").unwrap();
        let piece_at = |sq: Square| {
            if sq == e2 {
                Some(white_pawn)
            } else if sq == e7 {
                Some(black_pawn)
            } else {
                None
            }
        };
        let white_view = own_pieces_fen(Color::White, piece_at);
        assert_eq!(white_view, "8/8/8/8/8/8/4P3/8");
        let black_view = own_pieces_fen(Color::Black, piece_at);
        assert_eq!(black_view, "8/4p3/8/8/8/8/8/8");
    }
}
