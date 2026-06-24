//! HS256 JWT session issuance and verification.

use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use mcs_domain::UserId;

use crate::error::AuthError;

/// Server-side configuration for issuing and verifying session tokens.
///
/// The same configuration must be used for both [`issue_session`] and
/// [`verify_session`]: the `secret` keys the HMAC and the `issuer` is checked
/// on verification, so a mismatch in either rejects the token.
#[derive(Clone)]
#[non_exhaustive]
pub struct SessionConfig {
    /// The HMAC-SHA256 signing secret. Keep this confidential: anyone holding
    /// it can forge valid sessions. It should be high-entropy (>= 32 bytes).
    pub secret: Vec<u8>,
    /// How long an issued token remains valid, measured from issuance.
    pub ttl: Duration,
    /// The `iss` (issuer) claim written into tokens and required to match on
    /// verification, preventing tokens minted for another service from being
    /// accepted here.
    pub issuer: String,
}

impl SessionConfig {
    /// Creates a new session configuration.
    #[must_use]
    pub fn new(secret: Vec<u8>, ttl: Duration, issuer: String) -> Self {
        Self {
            secret,
            ttl,
            issuer,
        }
    }
}

// A manual `Debug` impl that redacts the secret so it cannot leak into logs.
impl std::fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionConfig")
            .field("secret", &"<redacted>")
            .field("ttl", &self.ttl)
            .field("issuer", &self.issuer)
            .finish()
    }
}

/// The claims carried by a session JWT.
///
/// Times are Unix timestamps in seconds, as required by [RFC 7519].
///
/// [RFC 7519]: https://www.rfc-editor.org/rfc/rfc7519
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: the authenticated user's identifier.
    pub sub: UserId,
    /// Issued-at time (Unix seconds).
    pub iat: i64,
    /// Expiry time (Unix seconds). The token is invalid at or after this
    /// instant.
    pub exp: i64,
    /// Issuer: the service that minted the token.
    pub iss: String,
}

/// Issues a signed HS256 session token for `user_id`.
///
/// The token's `iat` is the current UTC time and its `exp` is `iat + ttl`,
/// using the [`SessionConfig::ttl`]. The `iss` claim is set to
/// [`SessionConfig::issuer`].
///
/// # Errors
///
/// Returns [`AuthError::Other`] if the current time cannot be represented or
/// token encoding fails (both unexpected in practice).
pub fn issue_session(cfg: &SessionConfig, user_id: UserId) -> Result<String, AuthError> {
    let now = OffsetDateTime::now_utc();
    let exp = now + cfg.ttl;

    let claims = Claims {
        sub: user_id,
        iat: now.unix_timestamp(),
        exp: exp.unix_timestamp(),
        iss: cfg.issuer.clone(),
    };

    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(&cfg.secret),
    )
    .map_err(|e| AuthError::Other(format!("failed to encode session token: {e}")))
}

/// Verifies a session token and returns its claims.
///
/// Validation enforces:
/// - HS256 signature against [`SessionConfig::secret`];
/// - the `exp` claim is in the future (expired tokens are rejected);
/// - the `iss` claim equals [`SessionConfig::issuer`].
///
/// # Errors
///
/// - [`AuthError::Expired`] — the token's `exp` has elapsed.
/// - [`AuthError::InvalidToken`] — the token is malformed, has a bad
///   signature, a wrong issuer, or otherwise fails validation.
pub fn verify_session(cfg: &SessionConfig, token: &str) -> Result<Claims, AuthError> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&[cfg.issuer.as_str()]);
    validation.set_required_spec_claims(&["exp", "iss", "sub"]);
    // `exp` is validated by default; make the intent explicit.
    validation.validate_exp = true;

    decode::<Claims>(token, &DecodingKey::from_secret(&cfg.secret), &validation)
        .map(|data| data.claims)
        .map_err(|e| match e.kind() {
            ErrorKind::ExpiredSignature => AuthError::Expired,
            _ => AuthError::InvalidToken,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(ttl: Duration) -> SessionConfig {
        SessionConfig::new(
            b"super-secret-key-at-least-32-bytes-long!!".to_vec(),
            ttl,
            "mcs".to_owned(),
        )
    }

    #[test]
    fn issue_then_verify_round_trips_user_id() {
        let cfg = config(Duration::hours(1));
        let user = UserId::new();

        let token = issue_session(&cfg, user).unwrap();
        let claims = verify_session(&cfg, &token).unwrap();

        assert_eq!(claims.sub, user);
        assert_eq!(claims.iss, "mcs");
        assert!(claims.exp > claims.iat);
    }

    #[test]
    fn expired_token_is_rejected() {
        // Negative TTL => token already expired at issuance.
        let cfg = config(Duration::hours(-1));
        let token = issue_session(&cfg, UserId::new()).unwrap();

        let err = verify_session(&cfg, &token).unwrap_err();
        assert_eq!(err, AuthError::Expired);
    }

    #[test]
    fn token_signed_with_different_secret_is_rejected() {
        let issuing = config(Duration::hours(1));
        let token = issue_session(&issuing, UserId::new()).unwrap();

        let verifying = SessionConfig::new(
            b"a-completely-different-secret-key-value!!".to_vec(),
            Duration::hours(1),
            "mcs".to_owned(),
        );
        let err = verify_session(&verifying, &token).unwrap_err();
        assert_eq!(err, AuthError::InvalidToken);
    }

    #[test]
    fn token_with_wrong_issuer_is_rejected() {
        let issuing = config(Duration::hours(1));
        let token = issue_session(&issuing, UserId::new()).unwrap();

        let verifying = SessionConfig::new(
            issuing.secret.clone(),
            Duration::hours(1),
            "other-service".to_owned(),
        );
        let err = verify_session(&verifying, &token).unwrap_err();
        assert_eq!(err, AuthError::InvalidToken);
    }

    #[test]
    fn malformed_token_is_rejected() {
        let cfg = config(Duration::hours(1));
        let err = verify_session(&cfg, "not.a.jwt").unwrap_err();
        assert_eq!(err, AuthError::InvalidToken);
    }

    #[test]
    fn debug_redacts_secret() {
        let cfg = config(Duration::hours(1));
        let rendered = format!("{cfg:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("super-secret"));
    }
}
