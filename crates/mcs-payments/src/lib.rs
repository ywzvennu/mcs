//! # mcs-payments
//!
//! x402 ("402 Payment Required") protocol types and axum middleware for MCS.
//!
//! ## Protocol overview
//!
//! 1. Client requests a gated resource.
//! 2. Server replies `402` with a JSON body [`PaymentRequiredResponse`] listing
//!    accepted payment terms in `accepts`.
//! 3. Client pays on-chain (e.g. EIP-3009 `transferWithAuthorization` for USDC)
//!    and retries with an [`X_PAYMENT`] header whose value is a base64-encoded
//!    JSON [`PaymentPayload`].
//! 4. Server decodes the payload, calls a [`PaymentVerifier`] (which in
//!    production contacts a facilitator service), and on success:
//!    - inserts a [`Settlement`] into the axum request extensions, and
//!    - adds an [`X_PAYMENT_RESPONSE`] header to the response.
//!
//! ## Verifiers
//!
//! - [`MockVerifier`] — development only; performs no on-chain checks.
//! - [`FacilitatorVerifier`](facilitator::FacilitatorVerifier) — calls a real
//!   x402 facilitator's `/verify` + `/settle` endpoints. Available under the
//!   `facilitator` cargo feature (which pulls in [`reqwest`]).
//!
//! ## Usage
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use axum::{routing::get, Router};
//! use mcs_payments::{RequirePaymentLayer, MockVerifier, PaymentRequirements};
//!
//! let reqs = PaymentRequirements {
//!     scheme: "exact".into(),
//!     network: "base-sepolia".into(),
//!     max_amount_required: "1000000".into(), // 1 USDC (6 decimals)
//!     resource: "/premium".into(),
//!     description: "Premium chess analysis".into(),
//!     mime_type: "application/json".into(),
//!     pay_to: "0xYourAddress".into(),
//!     max_timeout_seconds: 300,
//!     asset: "0xUSDCAddress".into(),
//!     extra: None,
//! };
//! let verifier = Arc::new(MockVerifier);
//! // `store: Arc<dyn PaymentStore>` makes the gate idempotent (#108): a replayed
//! // `X-PAYMENT` is served from the recorded settlement, never charged twice.
//! let app = Router::new()
//!     .route("/premium", get(handler))
//!     .layer(RequirePaymentLayer::new(vec![reqs], verifier, store));
//! ```

pub mod error;
#[cfg(feature = "facilitator")]
pub mod facilitator;
pub mod middleware;
pub mod store;
pub mod types;
pub mod verifier;

pub use error::PaymentError;
#[cfg(feature = "facilitator")]
pub use facilitator::FacilitatorVerifier;
pub use middleware::{RequirePaymentLayer, X_PAYMENT, X_PAYMENT_RESPONSE};
pub use store::{idempotency_key, PaymentRecord, PaymentStore, PaymentStoreError};
pub use types::{
    PaymentPayload, PaymentRequiredResponse, PaymentRequirements, Settlement, SettlementResponse,
};
pub use verifier::{MockVerifier, PaymentVerifier};
