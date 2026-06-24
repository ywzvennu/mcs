//! Sign-In with Ethereum (EIP-4361 / EIP-191) signature verification.

use time::OffsetDateTime;

use mcs_domain::EvmAddress;
use siwe::{Message, VerificationError};

use crate::error::AuthError;

/// Verifies an EIP-4361 "Sign-In with Ethereum" message and its signature.
///
/// This function performs the full server-side verification of a wallet
/// login:
///
/// 1. Parses `message` as an EIP-4361 message.
/// 2. Checks the message's time bounds (`not_before` / `expiration_time`)
///    against the current UTC time.
/// 3. Recovers the signer's address from the 65-byte EIP-191 personal-sign
///    `signature` over `message`.
/// 4. Confirms the recovered signer equals the `address` field claimed inside
///    the message.
///
/// On success it returns the **recovered** [`EvmAddress`] — the cryptographically
/// authenticated identity, never unverified client input.
///
/// # Replay prevention (caller responsibility)
///
/// This function is stateless and therefore **cannot** detect that the same
/// `(message, signature)` pair has been submitted before. The message's nonce
/// must be enforced as single-use by the caller: record the nonce when the
/// challenge is issued (see [`generate_nonce`](crate::generate_nonce)) and,
/// after this function returns `Ok`, atomically reject the login if the nonce
/// has already been consumed. In the MCS architecture this is wired through
/// `SessionRepo` in `mcs-storage`. See the crate-level threat model.
///
/// # Arguments
///
/// - `message`: the exact EIP-4361 string that was presented to the wallet
///   (typically produced by
///   [`ChallengeParams::message`](crate::ChallengeParams::message)).
/// - `signature`: the wallet's EIP-191 signature. It must be exactly 65 bytes
///   (`r` ‖ `s` ‖ `v`).
///
/// # Errors
///
/// - [`AuthError::InvalidMessage`] — `message` is not a valid EIP-4361 string.
/// - [`AuthError::Expired`] — the message is not yet valid or has expired.
/// - [`AuthError::SignatureVerification`] — the signature is the wrong length
///   or did not cryptographically verify.
/// - [`AuthError::AddressMismatch`] — the signature verified but the recovered
///   signer differs from the address claimed in the message.
///
/// # Examples
///
/// ```no_run
/// # fn demo(message: &str, signature: &[u8]) -> Result<(), mcs_auth::AuthError> {
/// let signer = mcs_auth::verify_siwe(message, signature)?;
/// // `signer` is now the authenticated wallet address.
/// # let _ = signer;
/// # Ok(())
/// # }
/// ```
pub fn verify_siwe(message: &str, signature: &[u8]) -> Result<EvmAddress, AuthError> {
    let parsed: Message = message.parse().map_err(|_| AuthError::InvalidMessage)?;

    // Reject messages outside their validity window before touching the
    // signature, so an expired challenge cannot be replayed even with a valid
    // signature.
    if !parsed.valid_at(&OffsetDateTime::now_utc()) {
        return Err(AuthError::Expired);
    }

    // `verify_eip191` requires exactly 65 bytes; reject other lengths up front
    // with a clear signature error rather than relying on a panic.
    let sig: [u8; 65] = signature
        .try_into()
        .map_err(|_| AuthError::SignatureVerification)?;

    parsed.verify_eip191(&sig).map_err(map_verification_error)?;

    // `verify_eip191` already guarantees the recovered signer equals
    // `parsed.address`; convert that authenticated address into the domain type.
    address_from_bytes(&parsed.address)
}

/// Maps a [`siwe::VerificationError`] to the crate's [`AuthError`].
fn map_verification_error(err: VerificationError) -> AuthError {
    match err {
        // The recovered key did not match the claimed address.
        VerificationError::Signer => AuthError::AddressMismatch,
        // Bad signature length.
        VerificationError::SignatureLength => AuthError::SignatureVerification,
        // Malformed signature / failed ECDSA recovery.
        VerificationError::Crypto(_) => AuthError::SignatureVerification,
        // Time was already checked above, but handle defensively.
        VerificationError::Time => AuthError::Expired,
        // Remaining variants (serialization, domain/nonce mismatch which we do
        // not ask `verify` to check) collapse to a generic signature failure.
        other => AuthError::Other(other.to_string()),
    }
}

/// Extracts the `Nonce` field from an EIP-4361 message string.
///
/// The nonce returned is exactly the value the wallet signed when it produced
/// its EIP-191 signature over `message`. Callers should use this after
/// [`verify_siwe`] to obtain the nonce that was cryptographically authenticated,
/// and then atomically consume it in storage to defeat replay attacks.
///
/// # Errors
///
/// Returns [`AuthError::InvalidMessage`] if `message` is not a valid EIP-4361
/// string.
///
/// # Examples
///
/// ```no_run
/// # fn demo(message: &str) -> Result<(), mcs_auth::AuthError> {
/// let nonce = mcs_auth::nonce_from_message(message)?;
/// // consume `nonce` in your session store to prevent replay
/// # let _ = nonce;
/// # Ok(())
/// # }
/// ```
pub fn nonce_from_message(message: &str) -> Result<String, AuthError> {
    let parsed: Message = message.parse().map_err(|_| AuthError::InvalidMessage)?;
    Ok(parsed.nonce)
}

/// Builds an [`EvmAddress`] from the recovered 20 raw address bytes.
fn address_from_bytes(bytes: &[u8; 20]) -> Result<EvmAddress, AuthError> {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("0x{hex}")
        .parse()
        .map_err(|_| AuthError::Other("recovered address failed validation".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::challenge::{parse_address_bytes, ChallengeParams};

    use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
    use sha3::{Digest, Keccak256};

    /// A fixed, well-known secp256k1 private key used as a deterministic test
    /// vector. (This is the canonical example key from the secp256k1 / web3
    /// test corpus; it is NOT a real account.)
    const TEST_PRIVATE_KEY: &str =
        "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";

    /// The Ethereum address derived from [`TEST_PRIVATE_KEY`], computed once and
    /// pinned here so the test asserts an externally-checkable vector rather
    /// than re-deriving and trivially agreeing with itself.
    const TEST_ADDRESS: &str = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23";

    fn signing_key() -> SigningKey {
        let bytes = hex::decode(TEST_PRIVATE_KEY).unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    /// Derives the 20-byte Ethereum address from a signing key.
    fn derive_address(sk: &SigningKey) -> [u8; 20] {
        let vk: VerifyingKey = *sk.verifying_key();
        let point = vk.to_encoded_point(false);
        let hash = Keccak256::digest(&point.as_bytes()[1..]);
        let mut out = [0u8; 20];
        out.copy_from_slice(&hash[12..]);
        out
    }

    /// Produces a 65-byte EIP-191 personal-sign signature over `message`,
    /// matching what a wallet would return (recovery id offset by 27).
    fn sign(sk: &SigningKey, message: &str) -> [u8; 65] {
        let parsed: Message = message.parse().unwrap();
        let prehash = parsed.eip191_hash().unwrap();
        let (sig, recid): (Signature, RecoveryId) = sk.sign_prehash_recoverable(&prehash).unwrap();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = recid.to_byte() + 27;
        out
    }

    fn challenge_for(address: &EvmAddress, expiration: Option<OffsetDateTime>) -> String {
        ChallengeParams {
            domain: "localhost".to_owned(),
            address: address.clone(),
            uri: "https://localhost".to_owned(),
            chain_id: 1,
            nonce: "abcdef1234567890Z".to_owned(),
            issued_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            statement: Some("Sign in to MCS.".to_owned()),
            expiration,
        }
        .message()
        .unwrap()
    }

    #[test]
    fn derived_address_matches_known_vector() {
        let sk = signing_key();
        let addr = derive_address(&sk);
        let expected = parse_address_bytes(&TEST_ADDRESS.parse().unwrap()).unwrap();
        assert_eq!(
            addr, expected,
            "derived address must match the pinned vector"
        );
    }

    #[test]
    fn valid_signature_recovers_expected_address() {
        let sk = signing_key();
        let address: EvmAddress = TEST_ADDRESS.parse().unwrap();
        let message = challenge_for(&address, None);
        let signature = sign(&sk, &message);

        let recovered = verify_siwe(&message, &signature).unwrap();
        assert_eq!(recovered, address);
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let sk = signing_key();
        let address: EvmAddress = TEST_ADDRESS.parse().unwrap();
        let message = challenge_for(&address, None);
        let mut signature = sign(&sk, &message);
        // Flip a bit in the `s` component: still 65 bytes, but the recovered
        // signer will differ from (or fail to match) the claimed address.
        signature[40] ^= 0x01;

        let err = verify_siwe(&message, &signature).unwrap_err();
        assert!(
            matches!(
                err,
                AuthError::AddressMismatch | AuthError::SignatureVerification
            ),
            "tampered signature must fail, got {err:?}"
        );
    }

    #[test]
    fn wrong_address_in_message_is_rejected() {
        let sk = signing_key();
        // Sign a message that *claims* a different address than the signer's.
        let other: EvmAddress = "0x000000000000000000000000000000000000dead"
            .parse()
            .unwrap();
        let message = challenge_for(&other, None);
        let signature = sign(&sk, &message);

        let err = verify_siwe(&message, &signature).unwrap_err();
        assert_eq!(err, AuthError::AddressMismatch);
    }

    #[test]
    fn expired_message_is_rejected() {
        let sk = signing_key();
        let address: EvmAddress = TEST_ADDRESS.parse().unwrap();
        // Expiration in the distant past.
        let past = OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap();
        let message = challenge_for(&address, Some(past));
        let signature = sign(&sk, &message);

        let err = verify_siwe(&message, &signature).unwrap_err();
        assert_eq!(err, AuthError::Expired);
    }

    #[test]
    fn wrong_length_signature_is_rejected() {
        let address: EvmAddress = TEST_ADDRESS.parse().unwrap();
        let message = challenge_for(&address, None);
        let err = verify_siwe(&message, &[0u8; 10]).unwrap_err();
        assert_eq!(err, AuthError::SignatureVerification);
    }

    #[test]
    fn malformed_message_is_rejected() {
        let err = verify_siwe("this is not a SIWE message", &[0u8; 65]).unwrap_err();
        assert_eq!(err, AuthError::InvalidMessage);
    }
}
