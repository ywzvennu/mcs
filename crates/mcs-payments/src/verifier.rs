//! [`PaymentVerifier`] trait and [`MockVerifier`] for development.

use async_trait::async_trait;

use crate::{
    error::PaymentError,
    types::{PaymentPayload, PaymentRequirements, Settlement},
};

/// Async trait for verifying and settling a payment.
///
/// In production, implement this trait with an HTTP client that contacts your
/// facilitator service (e.g. `https://facilitator.example.com/verify`).  The
/// facilitator checks the on-chain authorization and—if valid—broadcasts the
/// transaction, returning the payer address and transaction hash.
///
/// # Example production skeleton
///
/// ```rust,ignore
/// struct FacilitatorClient { http: reqwest::Client, base_url: String }
///
/// #[async_trait]
/// impl PaymentVerifier for FacilitatorClient {
///     async fn verify(
///         &self,
///         payload: &PaymentPayload,
///         reqs: &PaymentRequirements,
///     ) -> Result<Settlement, PaymentError> {
///         let resp = self.http
///             .post(format!("{}/verify", self.base_url))
///             .json(&serde_json::json!({ "payload": payload, "requirements": reqs }))
///             .send()
///             .await
///             .map_err(|e| PaymentError::VerificationFailed(e.to_string()))?;
///         // deserialize and return Settlement { payer, transaction }
///         todo!()
///     }
/// }
/// ```
#[async_trait]
pub trait PaymentVerifier: Send + Sync {
    /// Verify `payload` against `reqs` and return a [`Settlement`] on success.
    async fn verify(
        &self,
        payload: &PaymentPayload,
        reqs: &PaymentRequirements,
    ) -> Result<Settlement, PaymentError>;
}

/// Development-only verifier that accepts any well-formed payload whose
/// `scheme`, `network`, and `asset` match the requirements.
///
/// # ⚠️ WARNING — DEV / TEST ONLY
///
/// `MockVerifier` performs **no cryptographic checks whatsoever**. It does not
/// verify signatures, balances, or on-chain state. Any caller who constructs a
/// valid-looking JSON payload will be granted access.
///
/// **Never use `MockVerifier` in production.**  Replace it with a real
/// [`PaymentVerifier`] implementation backed by a trusted facilitator.
#[derive(Debug, Clone, Copy)]
pub struct MockVerifier;

#[async_trait]
impl PaymentVerifier for MockVerifier {
    async fn verify(
        &self,
        payload: &PaymentPayload,
        reqs: &PaymentRequirements,
    ) -> Result<Settlement, PaymentError> {
        if payload.scheme != reqs.scheme {
            return Err(PaymentError::SchemeMismatch {
                got: payload.scheme.clone(),
                expected: reqs.scheme.clone(),
            });
        }
        if payload.network != reqs.network {
            return Err(PaymentError::NetworkMismatch {
                got: payload.network.clone(),
                expected: reqs.network.clone(),
            });
        }

        // Extract `asset` from the inner payload if present and cross-check.
        if let Some(got_asset) = payload.payload.get("asset").and_then(|v| v.as_str()) {
            if got_asset != reqs.asset {
                return Err(PaymentError::AssetMismatch {
                    got: got_asset.to_owned(),
                    expected: reqs.asset.clone(),
                });
            }
        }

        let payer = payload
            .payload
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("0x0000000000000000000000000000000000000000")
            .to_owned();

        let transaction = payload
            .payload
            .get("authorization")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        Ok(Settlement { payer, transaction })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PaymentRequirements;

    fn test_reqs() -> PaymentRequirements {
        PaymentRequirements {
            scheme: "exact".into(),
            network: "base-sepolia".into(),
            max_amount_required: "1000000".into(),
            resource: "/premium".into(),
            description: "Test".into(),
            mime_type: "application/json".into(),
            pay_to: "0xRecipient".into(),
            max_timeout_seconds: 300,
            asset: "0xUSDC".into(),
            extra: None,
        }
    }

    #[tokio::test]
    async fn mock_accepts_matching_payload() {
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "base-sepolia".into(),
            payload: serde_json::json!({ "from": "0xPayer", "authorization": "0xhash" }),
        };
        let settlement = MockVerifier.verify(&payload, &test_reqs()).await.unwrap();
        assert_eq!(settlement.payer, "0xPayer");
        assert_eq!(settlement.transaction.as_deref(), Some("0xhash"));
    }

    #[tokio::test]
    async fn mock_rejects_scheme_mismatch() {
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "streaming".into(),
            network: "base-sepolia".into(),
            payload: serde_json::json!({}),
        };
        let err = MockVerifier
            .verify(&payload, &test_reqs())
            .await
            .unwrap_err();
        assert!(matches!(err, PaymentError::SchemeMismatch { .. }));
    }

    #[tokio::test]
    async fn mock_rejects_network_mismatch() {
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "ethereum".into(),
            payload: serde_json::json!({}),
        };
        let err = MockVerifier
            .verify(&payload, &test_reqs())
            .await
            .unwrap_err();
        assert!(matches!(err, PaymentError::NetworkMismatch { .. }));
    }

    #[tokio::test]
    async fn mock_rejects_asset_mismatch() {
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "base-sepolia".into(),
            payload: serde_json::json!({ "asset": "0xWRONG" }),
        };
        let err = MockVerifier
            .verify(&payload, &test_reqs())
            .await
            .unwrap_err();
        assert!(matches!(err, PaymentError::AssetMismatch { .. }));
    }
}
