//! Persistence of settled x402 payments for idempotency (#108).
//!
//! The payment idempotency contract lives in `mcs-payments` as the
//! [`PaymentStore`] trait over a [`PaymentRecord`]: the payment middleware
//! checks the store *before* verifying so a replayed `X-PAYMENT` is served from
//! the prior settlement, and records each fresh settlement under a stable
//! idempotency key. This crate provides the durable backends:
//!
//! - [`SqlxStorage`](crate::SqlxStorage) implements [`PaymentStore`] against the
//!   `payments` table (PK / unique on `idempotency_key`).
//! - the in-memory test repo implements it over a `HashMap`.
//!
//! Both are reachable through the [`Repositories`](crate::Repositories) aggregate
//! via [`payments`](crate::Repositories::payments), so the API layer injects one
//! handle into the payment layer and settled payments persist.
//!
//! The trait itself is defined in `mcs-payments` (not here) so the payment
//! crate — which owns the middleware that consumes it — does not depend on this
//! storage crate. We re-export it for convenience.

pub use mcs_payments::{PaymentRecord, PaymentStore, PaymentStoreError};
