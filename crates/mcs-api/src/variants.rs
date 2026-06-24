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

use crate::state::AppState;

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// A single variant entry in the `GET /variants` response.
///
/// The `id` is the stable, machine-facing key used in seek requests; the
/// `display_name` is a human-readable label suitable for UI elements.
#[derive(Debug, Clone, Serialize)]
pub struct VariantDto {
    /// The stable, machine-facing identifier (e.g. `"standard"`, `"chess960"`).
    pub id: String,
    /// A human-readable label for the variant (e.g. `"Standard Chess"`).
    pub display_name: String,
}

/// Response body for `GET /variants`.
#[derive(Debug, Clone, Serialize)]
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
            registry.get(id).map(|factory| VariantDto {
                id: factory.id().to_owned(),
                display_name: factory.display_name().to_owned(),
            })
        })
        .collect();

    Json(VariantListResponse { variants })
}
