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
//! let app = Router::new()
//!     .route("/premium", get(handler))
//!     .layer(RequirePaymentLayer::new(vec![reqs], verifier));
//! ```

pub mod error;
pub mod middleware;
pub mod types;
pub mod verifier;

pub use error::PaymentError;
pub use middleware::{RequirePaymentLayer, X_PAYMENT, X_PAYMENT_RESPONSE};
pub use types::{
    PaymentPayload, PaymentRequiredResponse, PaymentRequirements, Settlement, SettlementResponse,
};
pub use verifier::{MockVerifier, PaymentVerifier};
