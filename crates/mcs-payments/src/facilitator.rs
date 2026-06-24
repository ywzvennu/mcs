//! A production [`PaymentVerifier`] backed by an x402 *facilitator* service.
//!
//! A facilitator is a trusted off-chain service that an x402 server delegates
//! to: it inspects a signed payment authorization, decides whether it is valid,
//! and — when asked — broadcasts the settling transaction on-chain. This frees
//! the resource server from holding keys or talking to a node directly.
//!
//! ## Protocol
//!
//! Every facilitator exposes two JSON HTTP endpoints. Both take the same body —
//! the x402 protocol version, the client's [`PaymentPayload`], and the
//! [`PaymentRequirements`] the server advertised — so the facilitator can
//! re-derive everything it needs:
//!
//! - `POST {base}/verify` → `{ isValid, invalidReason?, payer? }`. A cheap,
//!   read-only check that the authorization is well-formed, unexpired, and
//!   funded. No state changes.
//! - `POST {base}/settle` → `{ success, errorReason?, transaction?, network?,
//!   payer? }`. Broadcasts the transaction and returns its hash.
//!
//! [`FacilitatorVerifier::verify`] runs `verify` then `settle`, mapping the
//! result onto a [`Settlement`]. The caller — the axum `402` middleware in
//! [`crate::middleware`] — takes that [`Settlement`], encodes it as the
//! `X-PAYMENT-RESPONSE` header, and attaches it to the unlocked response so the
//! client can observe the settling transaction.
//!
//! This module is compiled only under the `facilitator` cargo feature, which
//! pulls in [`reqwest`]; the default build has no HTTP client.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    error::PaymentError,
    types::{PaymentPayload, PaymentRequirements, Settlement},
    verifier::PaymentVerifier,
};

/// The x402 protocol version this client speaks. Sent as `x402Version` in every
/// facilitator request body.
const X402_VERSION: u32 = 1;

/// Request body shared by the `/verify` and `/settle` facilitator endpoints.
///
/// Borrows its payload and requirements so no clone is needed to serialize a
/// request. Field names are camelCase to match the x402 wire format.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FacilitatorRequest<'a> {
    /// The x402 protocol version (`1`).
    x402_version: u32,
    /// The client's decoded `X-PAYMENT` payload.
    payment_payload: &'a PaymentPayload,
    /// The terms the server advertised in its `402` response.
    payment_requirements: &'a PaymentRequirements,
}

/// Response body of `POST {base}/verify`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyResponse {
    /// Whether the authorization is valid and may proceed to settlement.
    is_valid: bool,
    /// A human-readable reason present when `is_valid` is `false`.
    #[serde(default)]
    invalid_reason: Option<String>,
    /// The payer's on-chain address, if the facilitator could derive it.
    #[serde(default)]
    payer: Option<String>,
}

/// Response body of `POST {base}/settle`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettleResponse {
    /// Whether the settling transaction was accepted by the network.
    success: bool,
    /// A human-readable reason present when `success` is `false`.
    #[serde(default)]
    error_reason: Option<String>,
    /// The settling transaction hash, when available.
    #[serde(default)]
    transaction: Option<String>,
    /// The network settlement occurred on (echoed back; currently unused).
    #[serde(default)]
    #[allow(dead_code)]
    network: Option<String>,
    /// The payer's on-chain address, if the facilitator could derive it.
    #[serde(default)]
    payer: Option<String>,
}

/// A [`PaymentVerifier`] that delegates to a remote x402 facilitator.
///
/// Construct one with [`FacilitatorVerifier::new`] (anonymous) or
/// [`FacilitatorVerifier::with_api_key`] (authenticated with a bearer token).
/// The verifier is cheap to clone and shares a connection-pooled
/// [`reqwest::Client`]; wrap it in an `Arc` and reuse it for the process's
/// lifetime.
///
/// # Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use mcs_payments::FacilitatorVerifier;
///
/// // Point at any standards-compliant x402 facilitator.
/// let verifier = Arc::new(FacilitatorVerifier::with_api_key(
///     "https://facilitator.example.com",
///     "secret-token",
/// ));
/// ```
#[derive(Debug, Clone)]
pub struct FacilitatorVerifier {
    /// Facilitator base URL, with any trailing slash stripped so that
    /// `{base}/verify` is always well-formed.
    base_url: String,
    /// Pooled HTTP client reused across requests.
    client: reqwest::Client,
    /// Optional bearer token sent as `Authorization: Bearer {token}`.
    api_key: Option<String>,
}

impl FacilitatorVerifier {
    /// Creates a verifier targeting `base_url` with no authentication.
    ///
    /// Any trailing slash on `base_url` is trimmed so endpoint paths join
    /// cleanly.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::build(base_url.into(), None)
    }

    /// Creates a verifier that authenticates each request with a bearer
    /// `api_key` (`Authorization: Bearer {api_key}`).
    #[must_use]
    pub fn with_api_key(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::build(base_url.into(), Some(api_key.into()))
    }

    /// Shared constructor: normalizes the base URL and builds the HTTP client.
    fn build(base_url: String, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client: reqwest::Client::new(),
            api_key,
        }
    }

    /// POSTs `body` to `{base_url}{path}` and deserializes the JSON response,
    /// attaching the bearer token when configured.
    ///
    /// Transport failures and non-2xx statuses map to
    /// [`PaymentError::Facilitator`]; a body that fails to deserialize into `T`
    /// does too. The endpoint's *semantic* outcome (invalid / unsuccessful) is
    /// left for the caller to interpret from `T`.
    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &FacilitatorRequest<'_>,
    ) -> Result<T, PaymentError> {
        let url = format!("{}{path}", self.base_url);
        let mut request = self.client.post(&url).json(body);
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }

        let response = request
            .send()
            .await
            .map_err(|e| PaymentError::Facilitator(format!("POST {url}: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            // Surface a snippet of the body to aid debugging without dumping an
            // unbounded payload into the error message.
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(256).collect();
            return Err(PaymentError::Facilitator(format!(
                "POST {url} returned {status}: {snippet}"
            )));
        }

        response
            .json::<T>()
            .await
            .map_err(|e| PaymentError::Facilitator(format!("decoding {url} response: {e}")))
    }
}

#[async_trait]
impl PaymentVerifier for FacilitatorVerifier {
    /// Verifies and settles `payload` against `reqs` via the facilitator.
    ///
    /// 1. `POST /verify`. If `isValid` is `false`, returns
    ///    [`PaymentError::VerificationFailed`] carrying `invalidReason` and
    ///    never settles.
    /// 2. `POST /settle`. On `success`, returns a [`Settlement`] with the
    ///    payer address (preferring the settle response, falling back to the
    ///    verify response) and the transaction hash. On failure, returns
    ///    [`PaymentError::VerificationFailed`] carrying `errorReason`.
    ///
    /// Any transport error, non-2xx status, or undecodable body surfaces as
    /// [`PaymentError::Facilitator`].
    async fn verify(
        &self,
        payload: &PaymentPayload,
        reqs: &PaymentRequirements,
    ) -> Result<Settlement, PaymentError> {
        let body = FacilitatorRequest {
            x402_version: X402_VERSION,
            payment_payload: payload,
            payment_requirements: reqs,
        };

        let verify: VerifyResponse = self.post("/verify", &body).await?;
        if !verify.is_valid {
            let reason = verify
                .invalid_reason
                .unwrap_or_else(|| "facilitator reported the payment invalid".to_owned());
            return Err(PaymentError::VerificationFailed(reason));
        }

        let settle: SettleResponse = self.post("/settle", &body).await?;
        if !settle.success {
            let reason = settle
                .error_reason
                .unwrap_or_else(|| "facilitator failed to settle the payment".to_owned());
            return Err(PaymentError::VerificationFailed(reason));
        }

        // Prefer the payer the settle step reported; fall back to the verify
        // step. If neither carried one, surface a clear facilitator error rather
        // than fabricating an address.
        let payer = settle.payer.or(verify.payer).ok_or_else(|| {
            PaymentError::Facilitator("facilitator returned no payer address".to_owned())
        })?;

        Ok(Settlement {
            payer,
            transaction: settle.transaction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_payload() -> PaymentPayload {
        PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "base-sepolia".into(),
            payload: json!({ "from": "0xPayer", "authorization": "0xauth" }),
        }
    }

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

    /// The exact JSON body the client must send to both endpoints, used to
    /// assert wire-format conformance.
    fn expected_request_body() -> serde_json::Value {
        json!({
            "x402Version": 1,
            "paymentPayload": {
                "x402Version": 1,
                "scheme": "exact",
                "network": "base-sepolia",
                "payload": { "from": "0xPayer", "authorization": "0xauth" },
            },
            "paymentRequirements": {
                "scheme": "exact",
                "network": "base-sepolia",
                "maxAmountRequired": "1000000",
                "resource": "/premium",
                "description": "Test",
                "mimeType": "application/json",
                "payTo": "0xRecipient",
                "maxTimeoutSeconds": 300,
                "asset": "0xUSDC",
            },
        })
    }

    #[tokio::test]
    async fn verify_then_settle_returns_settlement() {
        let server = MockServer::start().await;

        // /verify asserts the x402 request shape and reports the payment valid.
        Mock::given(method("POST"))
            .and(path("/verify"))
            .and(body_json(expected_request_body()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "isValid": true,
                "payer": "0xPayer",
            })))
            .expect(1)
            .mount(&server)
            .await;

        // /settle likewise asserts the shape and returns a transaction hash.
        Mock::given(method("POST"))
            .and(path("/settle"))
            .and(body_json(expected_request_body()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "transaction": "0xtxhash",
                "network": "base-sepolia",
                "payer": "0xPayer",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let verifier = FacilitatorVerifier::new(server.uri());
        let settlement = verifier
            .verify(&test_payload(), &test_reqs())
            .await
            .expect("verify+settle should succeed");

        assert_eq!(settlement.payer, "0xPayer");
        assert_eq!(settlement.transaction.as_deref(), Some("0xtxhash"));
    }

    #[tokio::test]
    async fn invalid_verify_short_circuits_without_settling() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "isValid": false,
                "invalidReason": "insufficient funds",
            })))
            .expect(1)
            .mount(&server)
            .await;

        // No /settle stub is mounted: if the client called it the request would
        // 404 and surface as a Facilitator error rather than VerificationFailed,
        // so this also proves settle is skipped.
        let verifier = FacilitatorVerifier::new(server.uri());
        let err = verifier
            .verify(&test_payload(), &test_reqs())
            .await
            .unwrap_err();

        match err {
            PaymentError::VerificationFailed(reason) => {
                assert_eq!(reason, "insufficient funds");
            }
            other => panic!("expected VerificationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn settle_failure_maps_to_verification_failed() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "isValid": true })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/settle"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": false,
                "errorReason": "broadcast rejected",
            })))
            .mount(&server)
            .await;

        let verifier = FacilitatorVerifier::new(server.uri());
        let err = verifier
            .verify(&test_payload(), &test_reqs())
            .await
            .unwrap_err();

        match err {
            PaymentError::VerificationFailed(reason) => assert_eq!(reason, "broadcast rejected"),
            other => panic!("expected VerificationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_error_maps_to_facilitator_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
            .mount(&server)
            .await;

        let verifier = FacilitatorVerifier::new(server.uri());
        let err = verifier
            .verify(&test_payload(), &test_reqs())
            .await
            .unwrap_err();

        assert!(
            matches!(err, PaymentError::Facilitator(_)),
            "a 500 must map to a Facilitator error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn unreachable_facilitator_maps_to_facilitator_error() {
        // A port with no listener: the connection attempt fails at transport.
        let verifier = FacilitatorVerifier::new("http://127.0.0.1:1");
        let err = verifier
            .verify(&test_payload(), &test_reqs())
            .await
            .unwrap_err();

        assert!(
            matches!(err, PaymentError::Facilitator(_)),
            "an unreachable host must map to a Facilitator error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn api_key_is_sent_as_bearer_token() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/verify"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "isValid": true,
                "payer": "0xPayer",
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/settle"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "transaction": "0xtxhash",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let verifier = FacilitatorVerifier::with_api_key(server.uri(), "secret-token");
        let settlement = verifier
            .verify(&test_payload(), &test_reqs())
            .await
            .expect("authenticated verify+settle should succeed");
        assert_eq!(settlement.payer, "0xPayer");
    }

    #[tokio::test]
    async fn base_url_trailing_slash_is_trimmed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "isValid": true,
                "payer": "0xPayer",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/settle"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "success": true })))
            .mount(&server)
            .await;

        // A trailing slash on the base URL must not yield `//verify`.
        let verifier = FacilitatorVerifier::new(format!("{}/", server.uri()));
        let settlement = verifier
            .verify(&test_payload(), &test_reqs())
            .await
            .expect("trailing-slash base URL should still route correctly");
        assert_eq!(settlement.payer, "0xPayer");
    }

    /// Live end-to-end test against a real facilitator. Ignored by default and
    /// gated on `MCS_LIVE_FACILITATOR_URL` so CI (which has no facilitator)
    /// never runs it. Run with:
    ///
    /// ```sh
    /// MCS_LIVE_FACILITATOR_URL=https://facilitator.example.com \
    ///   cargo test --features facilitator -- --ignored live_facilitator
    /// ```
    #[tokio::test]
    #[ignore = "requires a live facilitator; set MCS_LIVE_FACILITATOR_URL"]
    async fn live_facilitator_round_trip() {
        let Ok(url) = std::env::var("MCS_LIVE_FACILITATOR_URL") else {
            eprintln!("MCS_LIVE_FACILITATOR_URL unset; skipping");
            return;
        };
        let verifier = match std::env::var("MCS_LIVE_FACILITATOR_API_KEY") {
            Ok(key) => FacilitatorVerifier::with_api_key(url, key),
            Err(_) => FacilitatorVerifier::new(url),
        };
        // We only assert the call completes and returns *some* typed result; a
        // synthetic payload is unlikely to settle, so either outcome is fine.
        let result = verifier.verify(&test_payload(), &test_reqs()).await;
        eprintln!("live facilitator result: {result:?}");
    }
}
