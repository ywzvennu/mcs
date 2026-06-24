//! Move-history and PGN export endpoints.
//!
//! These two read-only routes expose the append-only action log recorded by
//! every game actor, making the full move history available over HTTP:
//!
//! | Method & path             | Purpose |
//! |---------------------------|---------|
//! | `GET /games/{id}/moves`   | Full action log as JSON, ordered by ply. |
//! | `GET /games/{id}/pgn`     | PGN text for board-style variants. |
//!
//! # Authentication
//!
//! Both endpoints are public (no authentication required), consistent with the
//! existing `GET /games/{id}` endpoint they extend.
//!
//! # PGN export and non-board variants
//!
//! PGN is defined for board-style chess variants whose actions are plain moves.
//! For variants whose turns include non-move actions (e.g. RBC `sense` actions),
//! `GET /games/{id}/pgn` returns **409 Conflict** with a clear message rather
//! than crashing or producing a corrupt file. The check is applied per-game by
//! inspecting whether *all* recorded actions are of `"type": "move"` — so a
//! standard game that happens to include a resign or draw offer will also
//! return 409 (those actions carry no UCI string and have no PGN representation).
//! Callers that need the full event stream for any variant can always use
//! `GET /games/{id}/moves`.

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use mcs_core::Color;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use mcs_core::Action;
use mcs_domain::{Game, GameId};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Response DTOs
// ---------------------------------------------------------------------------

/// A single recorded action from a game's append-only log.
///
/// One entry corresponds to one half-move (ply). The `action` field is the
/// raw, type-erased JSON payload recorded by the variant — it is forwarded
/// verbatim so the endpoint works for every variant, including RBC
/// (which includes `sense` actions alongside `move` actions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveEntry {
    /// Zero-based half-move index within the game.
    pub ply: u32,
    /// The colour of the player who took this action.
    pub player: Color,
    /// The type-erased action payload, as defined by the variant.
    pub action: Action,
    /// White's remaining clock in milliseconds when the action was recorded;
    /// `None` for untimed games.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_white_ms: Option<u64>,
    /// Black's remaining clock in milliseconds when the action was recorded;
    /// `None` for untimed games.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_black_ms: Option<u64>,
    /// When the action was recorded (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Response body for `GET /games/{id}/moves`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovesResponse {
    /// The game's stable identifier.
    pub game_id: GameId,
    /// Every recorded action for the game, ordered by ply ascending.
    pub moves: Vec<MoveEntry>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Builds the history sub-router: the two read-only history endpoints.
///
/// Merge this into the top-level router alongside the existing game routes.
pub fn history_router() -> Router<AppState> {
    Router::new()
        .route("/games/{id}/moves", get(get_moves))
        .route("/games/{id}/pgn", get(get_pgn))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /games/{id}/moves` — full action log for a game, ordered by ply.
///
/// Returns every action recorded for the game in ply order. The `action`
/// field in each entry is the raw variant payload, so this endpoint works
/// for every registered variant, including RBC (which interleaves `sense`
/// and `move` actions).
///
/// # Errors
///
/// - **404 Not Found** if no game with the given id exists.
async fn get_moves(
    State(state): State<AppState>,
    Path(id): Path<GameId>,
) -> ApiResult<Json<MovesResponse>> {
    // Validate that the game exists before touching the action log.
    // The action log returns an empty vec for unknown games, so we must check
    // explicitly to provide the correct 404 instead of returning empty moves.
    let _game: Game = state.storage().games().get(id).await?;

    let recorded = state.action_log().list(id).await?;

    let moves = recorded
        .into_iter()
        .map(|ra| MoveEntry {
            ply: ra.ply,
            player: ra.player,
            action: ra.action,
            clock_white_ms: ra.clock_white_ms,
            clock_black_ms: ra.clock_black_ms,
            created_at: ra.created_at,
        })
        .collect();

    Ok(Json(MovesResponse { game_id: id, moves }))
}

/// `GET /games/{id}/pgn` — PGN export for board-style variants.
///
/// Produces a standard PGN document for games whose action log consists
/// entirely of `"type": "move"` actions — the necessary condition for a
/// meaningful PGN file. Supported variants include `standard` (and other
/// board-only variants that only record move actions).
///
/// ## PGN tags emitted
///
/// | Tag       | Value |
/// |-----------|-------|
/// | `Event`   | `"MCS game"` |
/// | `Site`    | `"mcs"` |
/// | `Date`    | Creation date, `YYYY.MM.DD` |
/// | `White`   | White player's user id |
/// | `Black`   | Black player's user id |
/// | `Result`  | `"1-0"`, `"0-1"`, `"1/2-1/2"`, or `"*"` |
/// | `Variant` | The variant id (e.g. `"standard"`) |
///
/// ## Movetext
///
/// Moves are emitted as move numbers with UCI strings (e.g. `1. e2e4 e7e5 2. …`),
/// followed by the result token. UCI is chosen over SAN because it requires no
/// chess engine and works correctly for every legal move, including promotions.
///
/// ## Non-board variants
///
/// If any recorded action is not of `"type": "move"` (e.g. an RBC `sense`),
/// this endpoint returns **409 Conflict** with a descriptive error message.
/// Use `GET /games/{id}/moves` to retrieve the full action log for any variant.
///
/// # Errors
///
/// - **404 Not Found** if no game with the given id exists.
/// - **409 Conflict** if any recorded action is not a plain `move` action.
async fn get_pgn(
    State(state): State<AppState>,
    Path(id): Path<GameId>,
) -> ApiResult<impl IntoResponse> {
    let game: Game = state.storage().games().get(id).await?;
    let recorded = state.action_log().list(id).await?;

    // Extract UCI strings, rejecting the game if any action is not a plain move.
    // This keeps PGN export correct and well-defined without a chess engine.
    let mut uci_moves: Vec<String> = Vec::with_capacity(recorded.len());
    for ra in &recorded {
        let action_val = ra.action.as_value();
        match action_val.get("type").and_then(|t| t.as_str()) {
            Some("move") => {
                let uci = action_val
                    .get("uci")
                    .and_then(|u| u.as_str())
                    .ok_or_else(|| {
                        ApiError::Internal(format!(
                            "move action at ply {} is missing 'uci' field",
                            ra.ply
                        ))
                    })?
                    .to_owned();
                uci_moves.push(uci);
            }
            Some(other) => {
                return Err(ApiError::Conflict(format!(
                    "PGN export is unavailable for variant '{}': action at ply {} has type '{}', \
                     which has no PGN representation. Use GET /games/{}/moves to retrieve the \
                     full action log.",
                    game.variant_id, ra.ply, other, id
                )));
            }
            None => {
                return Err(ApiError::Internal(format!(
                    "action at ply {} is missing 'type' field",
                    ra.ply
                )));
            }
        }
    }

    let pgn = build_pgn(&game, &uci_moves);

    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], pgn))
}

// ---------------------------------------------------------------------------
// PGN construction
// ---------------------------------------------------------------------------

/// Renders the PGN result token from the game's outcome.
///
/// Returns `"1-0"` (White wins), `"0-1"` (Black wins), `"1/2-1/2"` (draw),
/// or `"*"` (unfinished / unknown).
fn pgn_result(game: &Game) -> &'static str {
    use mcs_core::Color;
    match &game.outcome {
        None => "*",
        Some(outcome) => match outcome.winner {
            Some(Color::White) => "1-0",
            Some(Color::Black) => "0-1",
            None => "1/2-1/2",
        },
    }
}

/// Builds a complete PGN string from a game record and its ordered UCI moves.
///
/// The movetext uses full-move numbering: White's move and (on the same
/// line) Black's reply, separated by a space and terminated by the result
/// token.
fn build_pgn(game: &Game, uci_moves: &[String]) -> String {
    let date = game.created_at;
    let date_str = format!(
        "{:04}.{:02}.{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    );
    let result = pgn_result(game);

    let mut pgn = String::new();

    // Seven-tag roster
    pgn.push_str("[Event \"MCS game\"]\n");
    pgn.push_str("[Site \"mcs\"]\n");
    pgn.push_str(&format!("[Date \"{date_str}\"]\n"));
    pgn.push_str(&format!("[White \"{}\"]\n", game.white));
    pgn.push_str(&format!("[Black \"{}\"]\n", game.black));
    pgn.push_str(&format!("[Result \"{result}\"]\n"));
    pgn.push_str(&format!("[Variant \"{}\"]\n", game.variant_id));
    pgn.push('\n');

    // Movetext
    let mut movetext = String::new();
    for (i, uci) in uci_moves.iter().enumerate() {
        if i % 2 == 0 {
            // White's move — emit a move number
            let move_number = (i / 2) + 1;
            if !movetext.is_empty() {
                movetext.push(' ');
            }
            movetext.push_str(&format!("{move_number}. {uci}"));
        } else {
            // Black's reply — same move number, no prefix needed
            movetext.push(' ');
            movetext.push_str(uci);
        }
    }
    if !movetext.is_empty() {
        movetext.push(' ');
    }
    movetext.push_str(result);

    pgn.push_str(&movetext);
    pgn.push('\n');

    pgn
}
