//! Request extractors for authenticated routes.
//!
//! [`AuthUser`] is the building block for protecting endpoints: any handler
//! that takes an `AuthUser` argument is automatically gated behind a valid
//! session token. The REST game endpoints (#14) and the WebSocket upgrade
//! (#15) will rely on this exact extractor to identify the caller.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;

use mcs_auth::verify_session;
use mcs_domain::{EvmAddress, UserId};

use crate::error::ApiError;
use crate::state::AppState;

/// The authenticated caller behind a request, extracted from the bearer token.
///
/// Add `user: AuthUser` to a handler's arguments to require authentication:
/// axum runs this extractor before the handler body, and a missing, malformed,
/// or expired token short-circuits with **401 Unauthorized** (via
/// [`ApiError::Unauthorized`]) before the handler ever runs.
///
/// # Extraction
///
/// 1. Read the `Authorization` header; it must be present and of the form
///    `Bearer <jwt>`.
/// 2. Verify the JWT with [`verify_session`] against the server's
///    [`SessionConfig`](mcs_auth::SessionConfig). This checks the HS256
///    signature, the `exp` (expiry) claim, and the `iss` (issuer) claim.
/// 3. On success, expose the token subject as a [`UserId`].
///
/// # Address claim
///
/// Session JWTs carry only the [`UserId`] as their subject (see
/// [`Claims`](mcs_auth::Claims)). The wallet address is resolved from storage
/// so downstream handlers receive a fully-populated [`AuthUser`] without an
/// extra lookup; the lookup uses the verified `user_id`, so the address is
/// always the one bound to the authenticated account.
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// The authenticated user's stable identifier (the JWT `sub` claim).
    pub user_id: UserId,
    /// The Ethereum address bound to the authenticated account.
    pub address: EvmAddress,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(parts)?;

        // Verify the signature, expiry, and issuer. Any failure maps to 401 via
        // the `From<AuthError>` impl, with a single generic message so we never
        // disclose which check failed.
        let claims = verify_session(state.session_config(), token)?;
        let user_id = claims.sub;

        // Resolve the address bound to this verified user. A token whose subject
        // no longer exists is treated as an authentication failure rather than a
        // 404, so we do not leak account existence to a stale-token holder.
        let user = state
            .storage()
            .users()
            .get(user_id)
            .await
            .map_err(|_| ApiError::Unauthorized("authentication failed".to_owned()))?;

        // Stamp this user as active on every authenticated request so that
        // presence queries stay current without a dedicated heartbeat endpoint.
        state.presence().mark_seen(user_id);

        Ok(AuthUser {
            user_id,
            address: user.address,
        })
    }
}

/// Extracts the raw JWT from an `Authorization: Bearer <jwt>` header.
///
/// Returns [`ApiError::Unauthorized`] if the header is absent, not valid
/// UTF-8, missing the `Bearer ` scheme, or carries an empty token.
fn bearer_token(parts: &Parts) -> Result<&str, ApiError> {
    let header = parts
        .headers
        .get(AUTHORIZATION)
        .ok_or_else(|| ApiError::Unauthorized("missing authorization header".to_owned()))?;

    let value = header
        .to_str()
        .map_err(|_| ApiError::Unauthorized("malformed authorization header".to_owned()))?;

    // The scheme is case-insensitive per RFC 7235; accept any casing of "bearer".
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ApiError::Unauthorized("expected a bearer token".to_owned()))?;

    Ok(token)
}
