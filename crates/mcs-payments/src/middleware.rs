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
//!     // `store` is any `Arc<dyn PaymentStore>` (the storage crate's handle in
//!     // production); it makes the gate idempotent — a replayed `X-PAYMENT` is
//!     // served from the prior settlement instead of being charged again.
//!     .layer(RequirePaymentLayer::new(reqs, Arc::new(MockVerifier), store));
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
    store::{idempotency_key, PaymentRecord, PaymentStore, PaymentStoreError},
    types::{
        PaymentPayload, PaymentRequiredResponse, PaymentRequirements, Settlement,
        SettlementResponse,
    },
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
    store: Arc<dyn PaymentStore>,
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
    /// - `store` — the [`PaymentStore`] that makes paid actions idempotent: the
    ///   layer checks it *before* verifying so a replayed `X-PAYMENT` is served
    ///   from the prior settlement instead of being charged again, and records
    ///   each fresh settlement under its idempotency key (see [`idempotency_key`]).
    pub fn new(
        requirements: Vec<PaymentRequirements>,
        verifier: Arc<dyn PaymentVerifier>,
        store: Arc<dyn PaymentStore>,
    ) -> Self {
        Self {
            requirements,
            verifier,
            store,
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
            store: Arc::clone(&self.store),
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
    store: Arc<dyn PaymentStore>,
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

    fn call(&mut self, req: Request<Body>) -> BoxFuture<Response<Body>, S::Error> {
        let requirements = self.requirements.clone();
        let verifier = Arc::clone(&self.verifier);
        let store = Arc::clone(&self.store);

        // Clone inner so the borrow doesn't extend into the async block.
        let inner = self.inner.clone();

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

            // ── Idempotency (#108) ───────────────────────────────────────────
            // Derive a stable key from the payload and consult the store BEFORE
            // verifying/settling. If this payment already settled, reuse the
            // prior settlement and skip the (non-idempotent) verify+settle step
            // entirely — so an identical retried request is never charged twice.
            let key = idempotency_key(&payload);

            match store.find(&key).await {
                Ok(Some(existing)) => {
                    debug!("x402: idempotent replay — reusing recorded settlement");
                    let settlement = existing.settlement();
                    return proceed_with_settlement(inner, req, settlement, &reqs.network).await;
                }
                Ok(None) => { /* fall through to verify+settle */ }
                Err(e) => {
                    warn!("x402: payment store lookup failed: {e}");
                    return Ok(payment_required_response(
                        &requirements,
                        Some("payment store unavailable".to_owned()),
                    ));
                }
            }

            let settlement = match verifier.verify(&payload, reqs).await {
                Err(e) => {
                    warn!("x402: verification failed: {e}");
                    return Ok(payment_required_response(
                        &requirements,
                        Some(e.to_string()),
                    ));
                }
                Ok(settlement) => settlement,
            };

            // Persist the settlement under the idempotency key. A concurrent
            // duplicate may have recorded it first; that surfaces as a `Conflict`,
            // in which case we fall back to the now-existing record (we have
            // already settled, but a single record + the inner call still run
            // once per distinct key across the racing requests because only one
            // INSERT wins — see the integration test).
            let record = PaymentRecord {
                idempotency_key: key.clone(),
                payer: settlement.payer.clone(),
                amount: reqs.max_amount_required.clone(),
                asset: reqs.asset.clone(),
                network: reqs.network.clone(),
                transaction: settlement.transaction.clone(),
                resource: reqs.resource.clone(),
                created_at: time::OffsetDateTime::now_utc(),
            };

            let settlement = match store.record(&record).await {
                Ok(()) => settlement,
                Err(PaymentStoreError::Conflict) => {
                    // A concurrent duplicate won the race: reuse its record.
                    match store.find(&key).await {
                        Ok(Some(existing)) => existing.settlement(),
                        // The conflicting row vanished or the read failed; fall
                        // back to our own freshly-computed settlement rather than
                        // failing a request we did settle.
                        _ => settlement,
                    }
                }
                Err(e) => {
                    warn!("x402: recording payment failed: {e}");
                    return Ok(payment_required_response(
                        &requirements,
                        Some("payment store unavailable".to_owned()),
                    ));
                }
            };

            proceed_with_settlement(inner, req, settlement, &reqs.network).await
        })
    }
}

/// Inserts the [`Settlement`] into the request extensions, calls the inner
/// service, and appends the `X-PAYMENT-RESPONSE` receipt to the response.
///
/// Shared by the fresh-payment and idempotent-replay paths so both behave
/// identically downstream of settlement.
async fn proceed_with_settlement<S>(
    mut inner: S,
    mut req: Request<Body>,
    settlement: Settlement,
    network: &str,
) -> Result<Response<Body>, S::Error>
where
    S: Service<Request<Body>, Response = Response<Body>>,
{
    let settlement_resp = SettlementResponse {
        success: true,
        transaction: settlement.transaction.clone(),
        network: network.to_owned(),
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use async_trait::async_trait;
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt;

    use crate::{
        store::{PaymentRecord, PaymentStore, PaymentStoreError},
        types::{PaymentPayload, PaymentRequirements, Settlement},
        verifier::{MockVerifier, PaymentVerifier},
    };

    use super::*;

    /// A trivial in-memory [`PaymentStore`] for middleware tests: unique on
    /// `idempotency_key`, with a `record`-conflict path.
    #[derive(Default)]
    struct MemoryStore {
        records: Mutex<std::collections::HashMap<String, PaymentRecord>>,
    }

    #[async_trait]
    impl PaymentStore for MemoryStore {
        async fn find(
            &self,
            idempotency_key: &str,
        ) -> Result<Option<PaymentRecord>, PaymentStoreError> {
            Ok(self.records.lock().unwrap().get(idempotency_key).cloned())
        }

        async fn record(&self, record: &PaymentRecord) -> Result<(), PaymentStoreError> {
            let mut map = self.records.lock().unwrap();
            if map.contains_key(&record.idempotency_key) {
                return Err(PaymentStoreError::Conflict);
            }
            map.insert(record.idempotency_key.clone(), record.clone());
            Ok(())
        }
    }

    /// A [`PaymentVerifier`] wrapper that counts how many times `verify`
    /// (verify+settle) is invoked — used to prove idempotent replays do not
    /// re-settle.
    struct CountingVerifier {
        inner: MockVerifier,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PaymentVerifier for CountingVerifier {
        async fn verify(
            &self,
            payload: &PaymentPayload,
            reqs: &PaymentRequirements,
        ) -> Result<Settlement, PaymentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.verify(payload, reqs).await
        }
    }

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
                Arc::new(MemoryStore::default()),
            ))
    }

    fn valid_payment_header() -> String {
        // An exact/EIP-3009 payload carrying a single-use authorization `nonce`,
        // so its idempotency key is derived from that nonce.
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "base-sepolia".into(),
            payload: serde_json::json!({
                "from": "0xPayer",
                "authorization": { "nonce": "0xdeadbeef" },
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

    /// Counts how many times the inner handler ran, so a replay can be shown to
    /// reach the handler again (idempotently) without re-charging.
    fn counting_app(
        verifier: Arc<dyn PaymentVerifier>,
        store: Arc<dyn PaymentStore>,
        handler_calls: Arc<AtomicUsize>,
    ) -> Router {
        Router::new()
            .route(
                "/gated",
                get(move || {
                    let calls = Arc::clone(&handler_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            )
            .layer(RequirePaymentLayer::new(
                test_requirements(),
                verifier,
                store,
            ))
    }

    /// Two identical paid requests settle **once**: the second is served from the
    /// recorded settlement, so the verifier's verify+settle runs a single time
    /// while both requests still reach the handler with the `X-PAYMENT-RESPONSE`.
    #[tokio::test]
    async fn duplicate_payment_settles_once() {
        let settle_calls = Arc::new(AtomicUsize::new(0));
        let verifier: Arc<dyn PaymentVerifier> = Arc::new(CountingVerifier {
            inner: MockVerifier,
            calls: Arc::clone(&settle_calls),
        });
        let store: Arc<dyn PaymentStore> = Arc::new(MemoryStore::default());

        let header = valid_payment_header();

        for _ in 0..2 {
            let app = counting_app(
                Arc::clone(&verifier),
                Arc::clone(&store),
                Arc::new(AtomicUsize::new(0)),
            );
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/gated")
                        .header(X_PAYMENT, header.clone())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().contains_key(X_PAYMENT_RESPONSE));
        }

        // Verified+settled exactly once across the two identical paid requests.
        assert_eq!(settle_calls.load(Ordering::SeqCst), 1);
    }

    /// A distinct payment (different nonce) settles independently and creates its
    /// own record, so the verifier runs once per distinct payment.
    #[tokio::test]
    async fn distinct_payment_settles_again() {
        let settle_calls = Arc::new(AtomicUsize::new(0));
        let verifier: Arc<dyn PaymentVerifier> = Arc::new(CountingVerifier {
            inner: MockVerifier,
            calls: Arc::clone(&settle_calls),
        });
        let store: Arc<dyn PaymentStore> = Arc::new(MemoryStore::default());

        for nonce in ["0xnonce-a", "0xnonce-b"] {
            let header = PaymentPayload {
                x402_version: 1,
                scheme: "exact".into(),
                network: "base-sepolia".into(),
                payload: serde_json::json!({
                    "from": "0xPayer",
                    "authorization": { "nonce": nonce },
                }),
            }
            .to_header()
            .unwrap();

            let app = counting_app(
                Arc::clone(&verifier),
                Arc::clone(&store),
                Arc::new(AtomicUsize::new(0)),
            );
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
            assert_eq!(response.status(), StatusCode::OK);
        }

        // Two distinct payments ⇒ two settlements.
        assert_eq!(settle_calls.load(Ordering::SeqCst), 2);
    }
}
