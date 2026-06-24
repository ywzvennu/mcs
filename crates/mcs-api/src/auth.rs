//! Sign-In with Ethereum (SIWE) HTTP endpoints and session issuance.
//!
//! Two endpoints implement the wallet login handshake:
//!
//! | Method & path      | Purpose |
//! |--------------------|---------|
//! | `GET /auth/nonce`  | Issue a single-use SIWE challenge for an address. |
//! | `POST /auth/verify`| Verify the signed challenge and mint a session JWT. |
//!
//! The flow is: the client requests a nonce for its address, the wallet signs
//! the returned challenge message, and the client posts `{ message, signature }`
//! back. The server verifies the signature, atomically consumes the nonce to
//! defeat replay, upserts the user, and returns a bearer token to be presented
//! on subsequent requests (see [`AuthUser`](crate::AuthUser)).

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use mcs_auth::{generate_nonce, issue_session, verify_siwe};
use mcs_domain::{EvmAddress, UserId};

use crate::error::{ApiError, ApiResult};
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

/// Builds the `/auth` sub-router (nonce challenge + signature verification).
///
/// The returned router is generic over [`AppState`] and is merged into the
/// top-level router by [`crate::router`].
pub fn auth_router() -> Router<AppState> {
    Router::new()
        .route("/auth/nonce", get(nonce))
        .route("/auth/verify", post(verify))
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

    let message = build_siwe_message(
        &cfg.domain,
        &address,
        &cfg.uri,
        cfg.chain_id,
        &nonce,
        issued_at,
        &cfg.statement,
        expiration,
    )?;

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
    let nonce = nonce_from_message(&body.message)?;
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
    let user = state.storage().users().upsert_by_address(&address).await?;
    let token = issue_session(state.session_config(), user.id)?;

    Ok(Json(VerifyResponse {
        token,
        user_id: user.id,
        address: user.address,
    }))
}

/// Parses the `Nonce` field out of a SIWE message.
///
/// We parse with the same `siwe` crate that `verify_siwe` uses, so the nonce we
/// consume is exactly the one the wallet signed — there is no way to consume a
/// different nonce than the one bound into the verified signature.
fn nonce_from_message(message: &str) -> ApiResult<String> {
    let parsed: siwe::Message = message
        .parse()
        .map_err(|_| ApiError::Unauthorized("authentication failed".to_owned()))?;
    Ok(parsed.nonce)
}

/// Renders the canonical EIP-4361 message string for the wallet to sign.
///
/// The output is byte-for-byte what [`verify_siwe`] will later parse, so it is
/// sent to the client verbatim. Field validation (a malformed `domain` or
/// `uri`) surfaces as **422 Unprocessable Entity**, since those come from the
/// server's [`SiweConfig`](crate::SiweConfig) rather than the caller — a 422
/// makes a misconfiguration visible without masquerading as a client error.
#[allow(clippy::too_many_arguments)]
fn build_siwe_message(
    domain: &str,
    address: &EvmAddress,
    uri: &str,
    chain_id: u64,
    nonce: &str,
    issued_at: OffsetDateTime,
    statement: &str,
    expiration: OffsetDateTime,
) -> ApiResult<String> {
    let message = siwe::Message {
        domain: domain
            .parse()
            .map_err(|_| ApiError::UnprocessableEntity("invalid SIWE domain".to_owned()))?,
        address: address_bytes(address)?,
        statement: Some(statement.to_owned()),
        uri: uri
            .parse()
            .map_err(|_| ApiError::UnprocessableEntity("invalid SIWE uri".to_owned()))?,
        version: siwe::Version::V1,
        chain_id,
        nonce: nonce.to_owned(),
        issued_at: issued_at.into(),
        expiration_time: Some(expiration.into()),
        not_before: None,
        request_id: None,
        resources: vec![],
    };
    Ok(message.to_string())
}

/// Converts a validated [`EvmAddress`] into its raw 20-byte form.
///
/// [`EvmAddress`] guarantees a lowercase, `0x`-prefixed, 40-hex-char string, so
/// this cannot realistically fail; the fallible signature only guards the
/// invariant rather than panicking.
fn address_bytes(address: &EvmAddress) -> ApiResult<[u8; 20]> {
    let hex = address
        .as_str()
        .strip_prefix("0x")
        .ok_or_else(|| ApiError::UnprocessableEntity("invalid address".to_owned()))?;
    let mut out = [0u8; 20];
    hex::decode_to_slice(hex, &mut out)
        .map_err(|_| ApiError::UnprocessableEntity("invalid address".to_owned()))?;
    Ok(out)
}
