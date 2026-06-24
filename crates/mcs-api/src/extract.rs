//! Request extractors for authenticated routes.
//!
//! [`AuthUser`] is the building block for protecting endpoints: any handler
//! that takes an `AuthUser` argument is automatically gated behind a valid
//! session token. The REST game endpoints (#14) and the WebSocket upgrade
//! (#15) will rely on this exact extractor to identify the caller.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use time::OffsetDateTime;

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
/// 3. Check the revocation denylist (#101): a token whose `jti` has been
///    revoked (via `POST /auth/logout`) is rejected with **401**, even though
///    its signature and expiry are still valid.
/// 4. On success, expose the token subject as a [`UserId`].
///
/// # Address claim
///
/// Session JWTs carry only the [`UserId`] as their subject (see
/// [`Claims`](mcs_auth::Claims)). The wallet address is resolved from storage
/// so downstream handlers receive a fully-populated [`AuthUser`] without an
/// extra lookup; the lookup uses the verified `user_id`, so the address is
/// always the one bound to the authenticated account.
///
/// # Token identity
///
/// The validated token's `jti` and `exp` are carried through on the extracted
/// value so a handler such as `POST /auth/logout` can revoke *this* token
/// without re-decoding it. They are the verified claims, not client input.
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// The authenticated user's stable identifier (the JWT `sub` claim).
    pub user_id: UserId,
    /// The Ethereum address bound to the authenticated account.
    pub address: EvmAddress,
    /// The presented token's unique id (`jti`), used to revoke it on logout.
    pub jti: String,
    /// The presented token's expiry, recorded alongside a revocation entry so
    /// the denylist self-trims once the token would expire anyway.
    pub token_expires_at: OffsetDateTime,
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

        // Revocation check (#101): a logged-out token is denylisted by its `jti`.
        // The signature/expiry above still pass for such a token, so this is the
        // gate that enforces logout. A denylisted token fails with the same
        // generic 401 as any other auth failure, so we never reveal *why*.
        if state
            .storage()
            .revoked_tokens()
            .is_revoked(&claims.jti)
            .await
            .map_err(|_| ApiError::Unauthorized("authentication failed".to_owned()))?
        {
            return Err(ApiError::Unauthorized("authentication failed".to_owned()));
        }

        // The token's `exp` is a Unix-second claim; recover it as an
        // `OffsetDateTime` so a logout handler can stamp the revocation entry's
        // trim point. A malformed value is treated as an auth failure.
        let token_expires_at = OffsetDateTime::from_unix_timestamp(claims.exp)
            .map_err(|_| ApiError::Unauthorized("authentication failed".to_owned()))?;

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
            jti: claims.jti,
            token_expires_at,
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
