//! x402 protocol wire types.
//!
//! Field names use camelCase to match the published x402 specification and
//! facilitate zero-copy interop with existing facilitator implementations.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};

use crate::error::PaymentError;

/// Payment terms advertised to the client in a `402` response.
///
/// A server may list multiple `PaymentRequirements` entries in
/// [`PaymentRequiredResponse::accepts`] to support different schemes, networks,
/// or assets. The client picks one, fulfils it, and retries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequirements {
    /// Payment scheme identifier. The canonical value is `"exact"`, meaning the
    /// client must transfer exactly the specified amount.
    pub scheme: String,

    /// Target network (e.g. `"base"`, `"base-sepolia"`, `"ethereum"`).
    pub network: String,

    /// Maximum token amount the server will accept, expressed as the smallest
    /// unit of the asset (e.g. `"1000000"` for 1 USDC with 6 decimals).
    pub max_amount_required: String,

    /// The URL path (or full URL) of the protected resource.
    pub resource: String,

    /// Human-readable description shown to the user before payment.
    pub description: String,

    /// MIME type of the resource that will be returned after payment.
    pub mime_type: String,

    /// On-chain address that must receive the payment.
    pub pay_to: String,

    /// Maximum number of seconds the signed authorization may remain pending
    /// before the server considers it expired.
    pub max_timeout_seconds: u64,

    /// Contract address of the accepted payment token (e.g. USDC).
    pub asset: String,

    /// Scheme-specific extra data (e.g. EIP-3009 contract call details).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Body sent with every `402 Payment Required` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequiredResponse {
    /// x402 protocol version. Currently `1`.
    pub x402_version: u32,

    /// One or more sets of payment terms the server accepts.
    pub accepts: Vec<PaymentRequirements>,

    /// Optional human-readable error message (e.g. `"Payment expired"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Decoded content of the `X-PAYMENT` request header.
///
/// The header value is `base64(JSON(PaymentPayload))`. Use
/// [`PaymentPayload::from_header`] / [`PaymentPayload::to_header`] for
/// encoding/decoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentPayload {
    /// x402 protocol version. Must match the server's expected version.
    pub x402_version: u32,

    /// Payment scheme used (must match one of the advertised schemes).
    pub scheme: String,

    /// Network on which the payment was made (must match).
    pub network: String,

    /// Scheme-specific payload. For `"exact"` / EIP-3009 this contains the
    /// `transferWithAuthorization` call arguments.
    pub payload: serde_json::Value,
}

impl PaymentPayload {
    /// Decode a [`PaymentPayload`] from a raw `X-PAYMENT` header value
    /// (`base64(JSON(...))`).
    ///
    /// # Errors
    ///
    /// Returns [`PaymentError::MalformedPayment`] if the value is not valid
    /// base64 or the decoded bytes are not valid UTF-8 JSON.
    pub fn from_header(value: &str) -> Result<Self, PaymentError> {
        let bytes = BASE64
            .decode(value.trim())
            .map_err(|e| PaymentError::MalformedPayment(format!("base64 decode: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| PaymentError::MalformedPayment(format!("json decode: {e}")))
    }

    /// Encode this payload into a base64 string suitable for the `X-PAYMENT`
    /// header.
    ///
    /// # Errors
    ///
    /// Returns [`PaymentError::MalformedPayment`] if serialization fails
    /// (should never happen in practice for well-formed types).
    pub fn to_header(&self) -> Result<String, PaymentError> {
        let json = serde_json::to_vec(self)
            .map_err(|e| PaymentError::MalformedPayment(format!("json encode: {e}")))?;
        Ok(BASE64.encode(json))
    }
}

/// Result of a successful payment verification.
///
/// Inserted into the axum request [`Extensions`](axum::extract::Extension)
/// so that inner handlers can read the payer address and transaction hash.
#[derive(Debug, Clone)]
pub struct Settlement {
    /// On-chain address of the payer (checksummed if applicable).
    pub payer: String,
    /// Transaction / authorization hash, if available.
    pub transaction: Option<String>,
}

/// Body of the `X-PAYMENT-RESPONSE` header appended to a successful response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettlementResponse {
    /// `true` if the payment was verified and settled.
    pub success: bool,
    /// Transaction hash, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction: Option<String>,
    /// Network on which settlement occurred.
    pub network: String,
    /// On-chain address of the payer, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips_header_encoding() {
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "base-sepolia".into(),
            payload: serde_json::json!({ "authorization": "0xdeadbeef" }),
        };
        let header = payload.to_header().unwrap();
        let decoded = PaymentPayload::from_header(&header).unwrap();
        assert_eq!(decoded.scheme, payload.scheme);
        assert_eq!(decoded.network, payload.network);
        assert_eq!(decoded.x402_version, payload.x402_version);
    }

    #[test]
    fn from_header_rejects_invalid_base64() {
        let err = PaymentPayload::from_header("not-valid-base64!!!").unwrap_err();
        assert!(matches!(err, PaymentError::MalformedPayment(_)));
    }

    #[test]
    fn from_header_rejects_invalid_json() {
        let garbage = BASE64.encode(b"this is not json");
        let err = PaymentPayload::from_header(&garbage).unwrap_err();
        assert!(matches!(err, PaymentError::MalformedPayment(_)));
    }
}
