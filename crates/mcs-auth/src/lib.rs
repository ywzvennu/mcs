//! # mcs-auth
//!
//! Storage-agnostic authentication primitives for the Modular Chess Server.
//!
//! Login uses **Sign-In with Ethereum** ([EIP-4361]): the user signs a
//! structured challenge message with their EVM wallet, the server recovers the
//! signing address from the [EIP-191] personal-signature and — if it matches
//! the address embedded in the message — issues a stateless **HS256 JWT**
//! session token. The session token is later presented on each request and
//! verified locally without a round-trip to the wallet.
//!
//! This crate is deliberately **pure** and **IO-free**: no database, no
//! network, no async. It contains only the cryptographic and parsing logic.
//! Anything stateful (nonce storage, user lookup, session revocation) belongs
//! in the integration layer (`mcs-storage`, `mcs-api`).
//!
//! ## Login flow
//!
//! 1. **Challenge.** The server calls [`generate_nonce`] and builds a
//!    [`ChallengeParams`], then sends [`ChallengeParams::message`] to the
//!    client. The caller MUST persist the nonce (e.g. via `SessionRepo` in
//!    `mcs-storage`) so it can be enforced as single-use in step 3.
//! 2. **Sign.** The wallet displays the message and the user signs it,
//!    returning a 65-byte EIP-191 signature.
//! 3. **Verify.** The server calls [`verify_siwe`] with the *exact* message
//!    string and the signature. On success it returns the authenticated
//!    [`EvmAddress`]. The caller MUST then atomically consume the nonce
//!    (reject if already used) — see the [replay](#threat-model) note below.
//! 4. **Session.** The server maps the address to a [`UserId`] and calls
//!    [`issue_session`] to mint a JWT. Subsequent requests are authenticated
//!    with [`verify_session`].
//!
//! ## Threat model
//!
//! This crate guards against the following attacks. Where mitigation requires
//! external state, the responsibility is called out explicitly.
//!
//! - **Address spoofing.** [`verify_siwe`] recovers the signer from the
//!   signature and compares it against the `address` claimed inside the signed
//!   message. A signature produced by a *different* key fails with
//!   [`AuthError::AddressMismatch`] (or [`AuthError::SignatureVerification`]).
//!   The returned address is the *recovered* one, never trusted client input.
//! - **Replay.** A captured `(message, signature)` pair is replayable forever
//!   unless the nonce is single-use. This crate **generates** unpredictable
//!   nonces but **cannot** enforce single-use because it holds no state — the
//!   caller MUST record each issued nonce and reject a verification whose nonce
//!   has already been consumed. The challenge also carries `issued_at` /
//!   `expiration_time`, both validated here, which bound the replay window.
//! - **Expiry.** [`verify_siwe`] rejects a message that is not yet valid
//!   (`not_before` in the future) or already expired (`expiration_time` in the
//!   past) with [`AuthError::Expired`]. JWT sessions carry an `exp` claim and
//!   [`verify_session`] rejects expired tokens.
//! - **Token forgery / tampering.** Session tokens are HS256-signed with a
//!   server secret. A token signed with the wrong secret, altered, or
//!   malformed fails with [`AuthError::InvalidToken`]. The `iss` claim is
//!   validated against the configured issuer to prevent cross-service token
//!   reuse.
//! - **Revocation / logout.** A stateless JWT is otherwise valid until its
//!   `exp`, so logging out cannot "un-issue" it here. Each token therefore
//!   carries a unique `jti` ([`Claims::jti`], surfaced by [`issue_session`] via
//!   [`IssuedSession`]). The integration layer (`mcs-storage`, `mcs-api`)
//!   persists revoked `jti`s in a small **denylist** and checks it on every
//!   authenticated request after [`verify_session`] succeeds. The denylist is
//!   self-trimming: an entry need only live until the token's `exp`, after
//!   which the token is rejected on expiry regardless.
//!
//! ## Cryptography
//!
//! SIWE message parsing and EIP-191 signature recovery are provided by the
//! [`siwe`] crate (default features only — the async EIP-1271 contract-wallet
//! path is intentionally excluded to keep this crate synchronous and IO-free).
//! Signature recovery uses `secp256k1` ECDSA via `k256` underneath. JWTs use
//! [`jsonwebtoken`] with HMAC-SHA256.
//!
//! [EIP-4361]: https://eips.ethereum.org/EIPS/eip-4361
//! [EIP-191]: https://eips.ethereum.org/EIPS/eip-191
//! [`EvmAddress`]: mcs_domain::EvmAddress
//! [`UserId`]: mcs_domain::UserId
#![doc(html_root_url = "https://docs.rs/mcs-auth")]

mod challenge;
mod error;
mod session;
mod siwe_verify;

pub use challenge::{generate_nonce, ChallengeParams};
pub use error::AuthError;
pub use session::{issue_session, verify_session, Claims, IssuedSession, SessionConfig};
pub use siwe_verify::{nonce_from_message, verify_siwe};
