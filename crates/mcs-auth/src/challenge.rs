//! SIWE challenge construction and nonce generation.

use rand::distributions::Alphanumeric;
use rand::Rng;
use time::OffsetDateTime;

use mcs_domain::EvmAddress;

use crate::error::AuthError;

/// Length, in characters, of a generated nonce.
///
/// EIP-4361 requires a nonce of at least 8 alphanumeric characters. We use a
/// comfortably larger value to make the nonce unpredictable: 24 alphanumeric
/// characters is roughly 142 bits of entropy, well beyond what is needed to
/// resist guessing within a challenge's validity window.
const NONCE_LEN: usize = 24;

/// Generates a cryptographically random, EIP-4361-valid nonce.
///
/// The returned string is [`NONCE_LEN`] ASCII alphanumeric characters
/// (`A-Z`, `a-z`, `0-9`), satisfying the EIP-4361 requirement of at least 8
/// alphanumeric characters. Randomness comes from [`rand::thread_rng`], which
/// is seeded from the operating-system entropy source.
///
/// The nonce is the primary defence against **replay**: the caller MUST persist
/// each generated nonce and reject any [`verify_siwe`](crate::verify_siwe)
/// result whose nonce has already been consumed. This function only *produces*
/// nonces; single-use enforcement is the caller's responsibility (see the
/// crate-level threat model).
///
/// # Examples
///
/// ```
/// let nonce = mcs_auth::generate_nonce();
/// assert!(nonce.len() >= 8);
/// assert!(nonce.chars().all(|c| c.is_ascii_alphanumeric()));
/// ```
#[must_use]
pub fn generate_nonce() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(NONCE_LEN)
        .map(char::from)
        .collect()
}

/// The parameters needed to build a Sign-In with Ethereum (EIP-4361) challenge.
///
/// Construct this on the server, then call [`ChallengeParams::message`] to
/// obtain the canonical EIP-4361 string to send to the wallet for signing. The
/// produced string can be fed verbatim back into
/// [`verify_siwe`](crate::verify_siwe) alongside the wallet's signature.
///
/// # Fields
///
/// All fields map directly onto EIP-4361 message fields. `statement` and
/// `expiration` are optional; the rest are required.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ChallengeParams {
    /// The RFC 3986 authority requesting the sign-in, e.g. `"chess.example"`
    /// or `"localhost:8080"`. This binds the signature to a specific origin.
    pub domain: String,
    /// The address the user claims to control. The recovered signer must match
    /// this for verification to succeed.
    pub address: EvmAddress,
    /// An RFC 3986 URI for the resource being signed into, e.g.
    /// `"https://chess.example/login"`.
    pub uri: String,
    /// The EIP-155 chain ID the session is bound to (`1` for Ethereum
    /// mainnet).
    pub chain_id: u64,
    /// The single-use, unpredictable nonce — typically from
    /// [`generate_nonce`].
    pub nonce: String,
    /// When the challenge was issued. Used as the EIP-4361 `Issued At` field.
    pub issued_at: OffsetDateTime,
    /// An optional human-readable statement shown in the wallet, e.g.
    /// `"Sign in to MCS."`. Must not contain a newline.
    pub statement: Option<String>,
    /// An optional expiry. When set, a wallet signature is only accepted by
    /// [`verify_siwe`](crate::verify_siwe) before this instant, bounding the
    /// replay window.
    pub expiration: Option<OffsetDateTime>,
}

impl ChallengeParams {
    /// Renders the canonical EIP-4361 message string for the wallet to sign.
    ///
    /// The output is byte-for-byte the string that
    /// [`verify_siwe`](crate::verify_siwe) will parse and over which the
    /// EIP-191 signature is computed, so the caller should transmit it
    /// unaltered (no trimming or re-encoding).
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::InvalidMessage`] if any field is not valid for an
    /// EIP-4361 message — for example a `domain` that is not a valid RFC 3986
    /// authority, a `uri` that is not a valid URI, or a `statement` containing
    /// a newline.
    pub fn message(&self) -> Result<String, AuthError> {
        let address = parse_address_bytes(&self.address)?;

        let message = siwe::Message {
            domain: self.domain.parse().map_err(|_| AuthError::InvalidMessage)?,
            address,
            statement: self.statement.clone(),
            uri: self.uri.parse().map_err(|_| AuthError::InvalidMessage)?,
            version: siwe::Version::V1,
            chain_id: self.chain_id,
            nonce: self.nonce.clone(),
            issued_at: self.issued_at.into(),
            expiration_time: self.expiration.map(Into::into),
            not_before: None,
            request_id: None,
            resources: vec![],
        };

        Ok(message.to_string())
    }
}

/// Converts a validated [`EvmAddress`] into its raw 20-byte form.
///
/// [`EvmAddress`] guarantees a lowercase `0x`-prefixed 40-hex-char string, so
/// this conversion cannot realistically fail; the fallible signature is kept
/// only to surface an [`AuthError::InvalidMessage`] rather than panicking if
/// that invariant were ever violated.
pub(crate) fn parse_address_bytes(address: &EvmAddress) -> Result<[u8; 20], AuthError> {
    let hex = address
        .as_str()
        .strip_prefix("0x")
        .ok_or(AuthError::InvalidMessage)?;
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = hex.get(i * 2..i * 2 + 2).ok_or(AuthError::InvalidMessage)?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| AuthError::InvalidMessage)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_address() -> EvmAddress {
        "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23"
            .parse()
            .unwrap()
    }

    #[test]
    fn nonce_is_long_enough_and_alphanumeric() {
        for _ in 0..100 {
            let nonce = generate_nonce();
            assert!(nonce.len() >= 8, "nonce must be >= 8 chars");
            assert_eq!(nonce.len(), NONCE_LEN);
            assert!(nonce.chars().all(|c| c.is_ascii_alphanumeric()));
        }
    }

    #[test]
    fn nonces_are_distinct() {
        let a = generate_nonce();
        let b = generate_nonce();
        assert_ne!(a, b, "two nonces should not collide");
    }

    #[test]
    fn message_is_canonical_eip4361() {
        let params = ChallengeParams {
            domain: "localhost".to_owned(),
            address: sample_address(),
            uri: "https://localhost".to_owned(),
            chain_id: 1,
            nonce: "abcdef1234567890Z".to_owned(),
            issued_at: OffsetDateTime::from_unix_timestamp(1_750_000_000).unwrap(),
            statement: Some("Sign in.".to_owned()),
            expiration: None,
        };
        let msg = params.message().unwrap();

        // The challenge must parse straight back as a valid SIWE message and
        // round-trip the structured fields we set.
        let parsed: siwe::Message = msg.parse().unwrap();
        assert_eq!(parsed.nonce, "abcdef1234567890Z");
        assert_eq!(parsed.chain_id, 1);
        assert_eq!(parsed.statement.as_deref(), Some("Sign in."));
        // The checksummed address must encode the same 20 bytes.
        assert_eq!(
            parsed.address,
            parse_address_bytes(&sample_address()).unwrap()
        );
        assert!(msg.contains("Nonce: abcdef1234567890Z"));
    }

    #[test]
    fn invalid_domain_is_rejected() {
        let params = ChallengeParams {
            domain: "not a valid authority".to_owned(),
            address: sample_address(),
            uri: "https://localhost".to_owned(),
            chain_id: 1,
            nonce: generate_nonce(),
            issued_at: OffsetDateTime::from_unix_timestamp(1_750_000_000).unwrap(),
            statement: None,
            expiration: None,
        };
        assert_eq!(params.message().unwrap_err(), AuthError::InvalidMessage);
    }

    #[test]
    fn address_bytes_round_trip() {
        let bytes = parse_address_bytes(&sample_address()).unwrap();
        assert_eq!(
            &format!("0x{}", hex_lower(&bytes)),
            sample_address().as_str()
        );
    }

    fn hex_lower(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
