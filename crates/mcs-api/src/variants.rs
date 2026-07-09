//! `GET /variants` — discovery endpoint for registered game variants.
//!
//! Returns a JSON list of every variant currently registered in the
//! [`VariantRegistry`](mcs_core::VariantRegistry).  Clients use this to
//! populate a variant selector before posting a seek.
//!
//! | Method & path    | Auth | Purpose |
//! |------------------|------|---------|
//! | `GET /variants`  | no   | List every registered variant with its id and display name. |

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// A single variant entry in the `GET /variants` response.
///
/// The `id` is the stable, machine-facing key used in seek requests; the
/// `display_name` is a human-readable label suitable for UI elements. The
/// remaining fields are render-oriented metadata sourced from the variant's
/// factory ([`mcs_core::VariantMetadata`]) so a client can draw the board without
/// knowing the underlying rules engine.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VariantDto {
    /// The stable, machine-facing identifier (e.g. `"standard"`, `"chess960"`).
    pub id: String,
    /// A human-readable label for the variant (e.g. `"Standard Chess"`).
    pub display_name: String,
    /// The number of files (board width), in squares — `8` for ordinary chess,
    /// `9` for Shogi/Xiangqi, and so on.
    pub board_width: u32,
    /// The number of ranks (board height), in squares.
    pub board_height: u32,
    /// Whether the variant has a persistent hand and piece drops (Crazyhouse and
    /// the shogi family); a client renders a pocket / drop control when set.
    pub has_hand: bool,
    /// A coarse family / piece-set hint, when the variant exposes one. Currently
    /// a piece-set hint like `"chess"`, `"xiangqi"`, `"shogi"`, `"makruk"`, …
    /// (`null` for variants with no family, e.g. RBC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// The starting position in the variant's FEN dialect, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_fen: Option<String>,
}

/// Response body for `GET /variants`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VariantListResponse {
    /// All registered variants, sorted by id for a stable response order.
    pub variants: Vec<VariantDto>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Builds the variants sub-router: `GET /variants`.
pub fn variants_router() -> Router<AppState> {
    Router::new().route("/variants", get(list_variants))
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `GET /variants` — list every registered game variant.
///
/// Reads directly from [`AppState::variants`] so the list always reflects
/// the set of variants registered at server startup.  Variants are sorted
/// by `id` to produce a deterministic, client-friendly ordering.
async fn list_variants(State(state): State<AppState>) -> Json<VariantListResponse> {
    let registry = state.variants();
    let mut ids = registry.ids();
    // Sort for a stable, client-friendly order.
    ids.sort_unstable();

    let variants = ids
        .into_iter()
        .filter_map(|id| {
            registry.get(id).map(|factory| {
                let meta = factory.metadata();
                VariantDto {
                    id: factory.id().to_owned(),
                    display_name: factory.display_name().to_owned(),
                    board_width: meta.board_width,
                    board_height: meta.board_height,
                    has_hand: meta.has_hand,
                    family: meta.family,
                    start_fen: meta.start_fen,
                }
            })
        })
        .collect();

    Json(VariantListResponse { variants })
}

// ---------------------------------------------------------------------------
// OpenAPI documentation marker
// ---------------------------------------------------------------------------

/// `GET /variants` — list every registered game variant.
#[utoipa::path(
    get,
    path = "/variants",
    tag = "variants",
    responses(
        (status = 200, description = "All registered variants, sorted by id.", body = VariantListResponse),
    ),
)]
#[allow(dead_code)]
pub(crate) fn list_variants_doc() {}
