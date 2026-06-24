//! Sign-In with Ethereum (SIWE) HTTP endpoints and session issuance.
//!
//! Three endpoints implement the wallet login handshake and logout:
//!
//! | Method & path      | Purpose |
//! |--------------------|---------|
//! | `GET /auth/nonce`  | Issue a single-use SIWE challenge for an address. |
//! | `POST /auth/verify`| Verify the signed challenge and mint a session JWT. |
//! | `POST /auth/logout`| Revoke the caller's current session token. |
//!
//! The flow is: the client requests a nonce for its address, the wallet signs
//! the returned challenge message, and the client posts `{ message, signature }`
//! back. The server verifies the signature, atomically consumes the nonce to
//! defeat replay, upserts the user, and returns a bearer token to be presented
//! on subsequent requests (see [`AuthUser`](crate::AuthUser)).
//!
//! ## Session model & revocation (#101)
//!
//! Sessions are **stateless HS256 JWTs**: a token is self-validating (its
//! signature, `exp`, and `iss` claims) and needs no per-request storage
//! round-trip to *prove* it. That also means it cannot simply be "un-issued" —
//! it is valid until `exp`.
//!
//! To support logout, each token carries a unique `jti` (JWT ID). The server
//! keeps a small **persisted denylist** of revoked `jti`s
//! ([`RevokedTokenRepo`](mcs_storage::RevokedTokenRepo)). `POST /auth/logout`
//! adds the caller's current `jti` to it, and the
//! [`AuthUser`](crate::AuthUser) extractor checks the denylist on every
//! authenticated request — so a logged-out token is rejected immediately, on
//! this and every node sharing the store, and the rejection survives a restart.
//! Each entry is stored with the token's `exp` and is purged once that passes,
//! since the token is rejected on expiry anyway; the denylist therefore stays
//! bounded by the number of *unexpired* revoked tokens.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use mcs_auth::{generate_nonce, issue_session, nonce_from_message, verify_siwe, ChallengeParams};
use mcs_domain::{EvmAddress, UserId};

use crate::error::{ApiError, ApiResult};
use crate::extract::AuthUser;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

/// Query parameters for `GET /auth/nonce`.
#[derive(Debug, Deserialize)]
pub struct NonceQuery {
    /// The address the caller intends to authenticate. Validated into an
    /// [`EvmAddress`] during deserialization, so a malformed value is rejected
    /// with **422 Unprocessable Entity** before the handler runs.
    pub address: EvmAddress,
}

/// The structured SIWE challenge fields, echoed back alongside the canonical
/// message so a client can render or re-derive the message if it wishes.
///
/// Times are RFC 3339 strings (e.g. `"2026-06-24T12:00:00Z"`).
#[derive(Debug, Serialize)]
pub struct ChallengeFields {
    /// The RFC 3986 authority requesting the sign-in.
    pub domain: String,
    /// The address being authenticated (lowercase, `0x`-prefixed).
    pub address: EvmAddress,
    /// The RFC 3986 URI of the resource being signed into.
    pub uri: String,
    /// The EIP-155 chain ID the session is bound to.
    pub chain_id: u64,
    /// The single-use nonce embedded in the message.
    pub nonce: String,
    /// When the challenge was issued (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    /// The human-readable statement shown in the wallet.
    pub statement: String,
    /// When the challenge expires (RFC 3339, UTC). After this the nonce is
    /// rejected at verification time.
    #[serde(with = "time::serde::rfc3339")]
    pub expiration: OffsetDateTime,
}

/// Response body for `GET /auth/nonce`.
#[derive(Debug, Serialize)]
pub struct NonceResponse {
    /// The canonical EIP-4361 message string, ready to hand to the wallet for
    /// signing. It must be signed and returned **verbatim**.
    pub message: String,
    /// The structured fields that compose [`Self::message`], for convenience.
    pub challenge: ChallengeFields,
}

/// Request body for `POST /auth/verify`.
#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    /// The exact SIWE message string that was signed (as returned by
    /// `GET /auth/nonce`).
    pub message: String,
    /// The wallet's EIP-191 signature, hex-encoded (with or without a `0x`
    /// prefix). Must decode to exactly 65 bytes.
    pub signature: String,
}

/// Response body for `POST /auth/verify`.
#[derive(Debug, Serialize)]
pub struct VerifyResponse {
    /// The HS256 session JWT. Present it as `Authorization: Bearer <token>` on
    /// authenticated requests.
    pub token: String,
    /// The authenticated user's stable identifier.
    pub user_id: UserId,
    /// The authenticated wallet address (the cryptographically recovered one).
    pub address: EvmAddress,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Builds the `/auth` sub-router (nonce challenge + verify + logout).
///
/// The returned router is generic over [`AppState`] and is merged into the
/// top-level router by [`crate::router`].
pub fn auth_router() -> Router<AppState> {
    Router::new()
        .merge(nonce_router())
        .merge(verify_router())
        .merge(logout_router())
}

/// Builds the single-route `GET /auth/nonce` sub-router.
///
/// Isolated so [`crate::router`] can wrap *only* the nonce route in the per-IP
/// rate-limit layer (#100) without also throttling unrelated routes.
pub fn nonce_router() -> Router<AppState> {
    Router::new().route("/auth/nonce", get(nonce))
}

/// Builds the single-route `POST /auth/verify` sub-router.
///
/// Isolated so [`crate::router`] can wrap *only* the verify route in its own
/// per-IP rate-limit layer (#100).
pub fn verify_router() -> Router<AppState> {
    Router::new().route("/auth/verify", post(verify))
}

/// Builds the single-route `POST /auth/logout` sub-router.
///
/// Logout requires authentication (it revokes *the caller's own* token), so it
/// is not behind the per-IP login rate limiter; it is merged directly into the
/// top-level router by [`crate::router`].
pub fn logout_router() -> Router<AppState> {
    Router::new().route("/auth/logout", post(logout))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /auth/nonce` — issue a single-use SIWE challenge for `address`.
///
/// Generates an unpredictable nonce, persists it with an expiry (so it can be
/// enforced as single-use at verification time), and returns the canonical
/// SIWE message together with its structured fields.
async fn nonce(
    State(state): State<AppState>,
    Query(query): Query<NonceQuery>,
) -> ApiResult<Json<NonceResponse>> {
    let cfg = state.siwe_config();
    let address = query.address;

    let nonce = generate_nonce();
    let issued_at = OffsetDateTime::now_utc();
    let expiration = issued_at + cfg.nonce_ttl;

    // Persist the nonce before handing out the challenge so the matching
    // `consume_nonce` at verification time can enforce single use. Use the same
    // expiry that we embed in the message, keeping the two bounds consistent.
    state
        .storage()
        .sessions()
        .store_nonce(&address, &nonce, expiration)
        .await?;

    let message = ChallengeParams::new(
        cfg.domain.clone(),
        address.clone(),
        cfg.uri.clone(),
        cfg.chain_id,
        nonce.clone(),
        issued_at,
        Some(cfg.statement.clone()),
        Some(expiration),
    )
    .message()
    .map_err(|_| ApiError::UnprocessableEntity("invalid SIWE configuration".to_owned()))?;

    Ok(Json(NonceResponse {
        message,
        challenge: ChallengeFields {
            domain: cfg.domain.clone(),
            address,
            uri: cfg.uri.clone(),
            chain_id: cfg.chain_id,
            nonce,
            issued_at,
            statement: cfg.statement.clone(),
            expiration,
        },
    }))
}

/// `POST /auth/verify` — verify a signed challenge and mint a session token.
///
/// Steps: recover the signer from the SIWE message and signature, atomically
/// consume the message's nonce (rejecting replays), upsert the user, and issue
/// a JWT.
async fn verify(
    State(state): State<AppState>,
    Json(body): Json<VerifyRequest>,
) -> ApiResult<Json<VerifyResponse>> {
    // Decode the hex signature (tolerating an optional `0x` prefix). A bad
    // encoding is a client error, not an auth failure.
    let hex = body.signature.strip_prefix("0x").unwrap_or(&body.signature);
    let signature = hex::decode(hex)
        .map_err(|_| ApiError::BadRequest("signature is not valid hex".to_owned()))?;

    // Recover the authenticated address from the signature over the message.
    // A wrong/forged signature yields `AuthError`, mapped to 401.
    let address = verify_siwe(&body.message, &signature)?;

    // Extract the nonce the wallet actually signed and atomically consume it.
    // This is the single-use enforcement that defeats replay: a captured
    // `(message, signature)` pair fails here the second time.
    let nonce = nonce_from_message(&body.message)
        .map_err(|_| ApiError::Unauthorized("authentication failed".to_owned()))?;
    let consumed = state
        .storage()
        .sessions()
        .consume_nonce(&address, &nonce)
        .await?;
    if !consumed {
        return Err(ApiError::Unauthorized(
            "nonce is unknown, expired, or already used".to_owned(),
        ));
    }

    // Get-or-create the user for this address, then mint the session token.
    // The issued token carries a unique `jti`, the key under which logout later
    // revokes it; we only need the encoded token here.
    let user = state.storage().users().upsert_by_address(&address).await?;
    let issued = issue_session(state.session_config(), user.id)?;

    Ok(Json(VerifyResponse {
        token: issued.token,
        user_id: user.id,
        address: user.address,
    }))
}

/// `POST /auth/logout` — revoke the caller's current session token.
///
/// Requires authentication: the [`AuthUser`] extractor validates the bearer
/// token and hands the handler that token's verified `jti` and `exp`. We add
/// the `jti` to the revocation denylist (stamped with the token's `exp` so the
/// entry self-trims), after which the same token is rejected by the extractor
/// on every subsequent request — on this and any node sharing the store, and
/// across restarts.
///
/// Idempotent: logging out with an already-revoked token still succeeds with
/// **204 No Content** (re-revoking the same `jti` is a no-op). A different,
/// non-revoked token is unaffected.
async fn logout(State(state): State<AppState>, user: AuthUser) -> ApiResult<StatusCode> {
    state
        .storage()
        .revoked_tokens()
        .revoke(&user.jti, user.token_expires_at)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
