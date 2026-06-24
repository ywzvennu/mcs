//! [`PaymentError`] — typed errors for the x402 payment flow.

use thiserror::Error;

/// Errors that can arise during x402 payment verification.
///
/// Marked `#[non_exhaustive]` so new failure modes (e.g. additional facilitator
/// transport errors) can be added without a breaking change; downstream `match`
/// arms must include a wildcard.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PaymentError {
    /// The `X-PAYMENT` header was absent from the request.
    #[error("missing X-PAYMENT header")]
    MissingPayment,

    /// The `X-PAYMENT` header was present but could not be decoded.
    #[error("malformed X-PAYMENT header: {0}")]
    MalformedPayment(String),

    /// The payload scheme does not match any advertised requirement.
    #[error("payment scheme mismatch: got `{got}`, expected `{expected}`")]
    SchemeMismatch { got: String, expected: String },

    /// The payload network does not match any advertised requirement.
    #[error("payment network mismatch: got `{got}`, expected `{expected}`")]
    NetworkMismatch { got: String, expected: String },

    /// The payload asset does not match the required asset.
    #[error("payment asset mismatch: got `{got}`, expected `{expected}`")]
    AssetMismatch { got: String, expected: String },

    /// The facilitator (or mock) rejected the payment.
    #[error("payment verification failed: {0}")]
    VerificationFailed(String),

    /// The facilitator service could not be reached, returned an unexpected
    /// status, or sent a body that could not be decoded. Distinct from
    /// [`VerificationFailed`](Self::VerificationFailed), which signals that the
    /// facilitator *did* respond but rejected the payment.
    #[error("facilitator error: {0}")]
    Facilitator(String),
}
