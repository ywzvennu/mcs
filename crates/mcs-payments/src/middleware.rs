//! Axum [`RequirePaymentLayer`] middleware implementing the x402 gate.
//!
//! Attach via [`Router::layer`](axum::Router::layer):
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use axum::{routing::get, Router};
//! use mcs_payments::{RequirePaymentLayer, MockVerifier, PaymentRequirements};
//!
//! let reqs = vec![PaymentRequirements { /* ... */ }];
//! let app = Router::new()
//!     .route("/paid", get(handler))
//!     .layer(RequirePaymentLayer::new(reqs, Arc::new(MockVerifier)));
//! ```

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    body::Body,
    http::{HeaderName, HeaderValue, Request, Response, StatusCode},
    response::IntoResponse,
};
use tower::Layer;
use tower::Service;
use tracing::{debug, warn};

use crate::{
    error::PaymentError,
    types::{PaymentPayload, PaymentRequiredResponse, PaymentRequirements, SettlementResponse},
    verifier::PaymentVerifier,
};

/// HTTP header name for the payment token sent by the client.
pub const X_PAYMENT: &str = "x-payment";

/// HTTP header name for the settlement receipt returned by the server.
pub const X_PAYMENT_RESPONSE: &str = "x-payment-response";

// ── Layer ────────────────────────────────────────────────────────────────────

/// [`tower::Layer`] that wraps an inner service with x402 payment enforcement.
#[derive(Clone)]
pub struct RequirePaymentLayer {
    requirements: Vec<PaymentRequirements>,
    verifier: Arc<dyn PaymentVerifier>,
}

impl std::fmt::Debug for RequirePaymentLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequirePaymentLayer")
            .field("requirements", &self.requirements)
            .finish_non_exhaustive()
    }
}

impl RequirePaymentLayer {
    /// Create a new layer.
    ///
    /// - `requirements` — list of acceptable payment terms (sent in `402` bodies).
    /// - `verifier` — shared verifier; use [`MockVerifier`](crate::MockVerifier)
    ///   during development and a real facilitator client in production.
    pub fn new(requirements: Vec<PaymentRequirements>, verifier: Arc<dyn PaymentVerifier>) -> Self {
        Self {
            requirements,
            verifier,
        }
    }
}

impl<S> Layer<S> for RequirePaymentLayer {
    type Service = RequirePayment<S>;

    fn layer(&self, inner: S) -> RequirePayment<S> {
        RequirePayment {
            inner,
            requirements: self.requirements.clone(),
            verifier: Arc::clone(&self.verifier),
        }
    }
}

// ── Service ──────────────────────────────────────────────────────────────────

/// Middleware service produced by [`RequirePaymentLayer`].
#[derive(Clone)]
pub struct RequirePayment<S> {
    inner: S,
    requirements: Vec<PaymentRequirements>,
    verifier: Arc<dyn PaymentVerifier>,
}

impl<S: std::fmt::Debug> std::fmt::Debug for RequirePayment<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequirePayment")
            .field("inner", &self.inner)
            .field("requirements", &self.requirements)
            .finish_non_exhaustive()
    }
}

type BoxFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

impl<S> Service<Request<Body>> for RequirePayment<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> BoxFuture<Response<Body>, S::Error> {
        let requirements = self.requirements.clone();
        let verifier = Arc::clone(&self.verifier);

        // Clone inner so the borrow doesn't extend into the async block.
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let payment_header = req
                .headers()
                .get(X_PAYMENT)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);

            let header_str = match payment_header {
                None => {
                    debug!("x402: no X-PAYMENT header — returning 402");
                    return Ok(payment_required_response(&requirements, None));
                }
                Some(h) => h,
            };

            let payload = match PaymentPayload::from_header(&header_str) {
                Err(e) => {
                    warn!("x402: malformed X-PAYMENT header: {e}");
                    return Ok(payment_required_response(
                        &requirements,
                        Some(e.to_string()),
                    ));
                }
                Ok(p) => p,
            };

            // Find the first requirement whose scheme / network match the payload.
            let matched_req = requirements
                .iter()
                .find(|r| r.scheme == payload.scheme && r.network == payload.network);

            let reqs = match matched_req {
                None => {
                    let msg = format!(
                        "no requirement matches scheme={} network={}",
                        payload.scheme, payload.network
                    );
                    warn!("x402: {msg}");
                    return Ok(payment_required_response(&requirements, Some(msg)));
                }
                Some(r) => r,
            };

            match verifier.verify(&payload, reqs).await {
                Err(e) => {
                    warn!("x402: verification failed: {e}");
                    Ok(payment_required_response(
                        &requirements,
                        Some(e.to_string()),
                    ))
                }
                Ok(settlement) => {
                    let settlement_resp = SettlementResponse {
                        success: true,
                        transaction: settlement.transaction.clone(),
                        network: reqs.network.clone(),
                        payer: Some(settlement.payer.clone()),
                    };

                    req.extensions_mut().insert(settlement);

                    let mut response = inner.call(req).await?;

                    if let Ok(header_val) = build_settlement_header(&settlement_resp) {
                        response
                            .headers_mut()
                            .insert(HeaderName::from_static(X_PAYMENT_RESPONSE), header_val);
                    }

                    Ok(response)
                }
            }
        })
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn payment_required_response(
    requirements: &[PaymentRequirements],
    error: Option<String>,
) -> Response<Body> {
    let body = PaymentRequiredResponse {
        x402_version: 1,
        accepts: requirements.to_vec(),
        error,
    };
    let json = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(StatusCode::PAYMENT_REQUIRED)
        .header("content-type", "application/json")
        .body(Body::from(json))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn build_settlement_header(sr: &SettlementResponse) -> Result<HeaderValue, PaymentError> {
    let s = serde_json::to_string(sr)
        .map_err(|e| PaymentError::MalformedPayment(format!("settlement encode: {e}")))?;
    HeaderValue::from_str(&s)
        .map_err(|e| PaymentError::MalformedPayment(format!("settlement header: {e}")))
}

// ── Integration tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt;

    use crate::{
        types::{PaymentPayload, PaymentRequirements},
        verifier::MockVerifier,
    };

    use super::*;

    fn test_requirements() -> Vec<PaymentRequirements> {
        vec![PaymentRequirements {
            scheme: "exact".into(),
            network: "base-sepolia".into(),
            max_amount_required: "1000000".into(),
            resource: "/gated".into(),
            description: "Premium feature".into(),
            mime_type: "application/json".into(),
            pay_to: "0xRecipient".into(),
            max_timeout_seconds: 300,
            asset: "0xUSDC".into(),
            extra: None,
        }]
    }

    fn make_app() -> Router {
        Router::new()
            .route("/gated", get(|| async { "ok" }))
            .layer(RequirePaymentLayer::new(
                test_requirements(),
                Arc::new(MockVerifier),
            ))
    }

    fn valid_payment_header() -> String {
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "base-sepolia".into(),
            payload: serde_json::json!({
                "from": "0xPayer",
                "authorization": "0xdeadbeef"
            }),
        };
        payload.to_header().unwrap()
    }

    #[tokio::test]
    async fn unpaid_request_returns_402_with_accepts() {
        let app = make_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/gated")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["x402Version"], 1);
        let accepts = parsed["accepts"].as_array().unwrap();
        assert!(!accepts.is_empty());
        assert_eq!(accepts[0]["scheme"], "exact");
    }

    #[tokio::test]
    async fn valid_payment_returns_200_with_settlement_header() {
        let app = make_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/gated")
                    .header(X_PAYMENT, valid_payment_header())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().contains_key(X_PAYMENT_RESPONSE),
            "expected X-PAYMENT-RESPONSE header"
        );
        let settlement_raw = response.headers()[X_PAYMENT_RESPONSE].to_str().unwrap();
        let settlement: serde_json::Value = serde_json::from_str(settlement_raw).unwrap();
        assert_eq!(settlement["success"], true);
        assert_eq!(settlement["payer"], "0xPayer");
    }

    #[tokio::test]
    async fn malformed_payment_header_returns_402() {
        let app = make_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/gated")
                    .header(X_PAYMENT, "this-is-not-base64!!!")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(parsed["error"].is_string());
    }

    #[tokio::test]
    async fn mismatched_scheme_returns_402() {
        let app = make_app();
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "streaming".into(), // wrong scheme
            network: "base-sepolia".into(),
            payload: serde_json::json!({}),
        };
        let header = payload.to_header().unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/gated")
                    .header(X_PAYMENT, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(parsed["error"].is_string());
    }

    #[tokio::test]
    async fn mismatched_network_returns_402() {
        let app = make_app();
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "ethereum".into(), // wrong network
            payload: serde_json::json!({}),
        };
        let header = payload.to_header().unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/gated")
                    .header(X_PAYMENT, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    }
}
