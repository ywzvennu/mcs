//! Persistence of settled x402 payments for **idempotency** (#108).
//!
//! A paid action (today: game creation via `POST /seeks`) must charge **at most
//! once** even if the client retries the *same* `X-PAYMENT` header — e.g. after a
//! dropped connection, a proxy retry, or a double click. The x402 verify+settle
//! step is not naturally idempotent: replaying it would re-broadcast (or, with a
//! facilitator, re-attempt) settlement and could double-charge the payer.
//!
//! To make the flow idempotent we derive a **stable idempotency key** from the
//! payment payload (see [`idempotency_key`]), and persist a [`PaymentRecord`]
//! under that key the first time a payment settles. On a later request carrying
//! the *same* payload the middleware finds the existing record and skips
//! verification and settlement entirely, reusing the prior settlement.
//!
//! The store is intentionally tiny and backend-agnostic: it carries no `sqlx`
//! (or any other driver) dependency so the core payments crate stays free of a
//! database. The server's storage crate implements [`PaymentStore`] for its
//! sqlx-backed handle; tests use an in-memory map.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{PaymentPayload, Settlement};

/// The canonical x402 "exact" scheme identifier (EIP-3009
/// `transferWithAuthorization`). Its inner payload carries an authorization with
/// a single-use on-chain `nonce`, which we reuse as the idempotency key.
const EXACT_SCHEME: &str = "exact";

/// A persisted record of a settled payment, keyed by its [`idempotency_key`].
///
/// One row exists per *distinct* payment. Its presence is the signal that the
/// payment has already been verified and settled, so a retry of the same
/// `X-PAYMENT` must be served from this record rather than charged again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRecord {
    /// The stable, unique key derived from the payment payload (see
    /// [`idempotency_key`]). This is the store's primary/unique key: a second
    /// `record` under the same key is a conflict, signalling "already recorded".
    pub idempotency_key: String,
    /// The on-chain address of the payer, from the settlement.
    pub payer: String,
    /// The charged amount, in the asset's smallest unit (the requirement's
    /// `max_amount_required`). Stored as a string to preserve arbitrary
    /// precision exactly as it appears on the wire.
    pub amount: String,
    /// The payment-token contract address (e.g. USDC), from the requirement.
    pub asset: String,
    /// The network the payment settled on (e.g. `base-sepolia`).
    pub network: String,
    /// The settlement transaction / authorization hash, when the verifier
    /// returned one. `None` for verifiers that do not surface a hash.
    pub transaction: Option<String>,
    /// The protected resource path the payment unlocked (e.g. `/seeks`).
    pub resource: String,
    /// When the record was first written (RFC 3339 / settlement time).
    pub created_at: time::OffsetDateTime,
}

impl PaymentRecord {
    /// Reconstructs the [`Settlement`] this record represents, so a replayed
    /// (already-paid) request can proceed with the original settlement without
    /// re-verifying.
    #[must_use]
    pub fn settlement(&self) -> Settlement {
        Settlement {
            payer: self.payer.clone(),
            transaction: self.transaction.clone(),
        }
    }
}

/// Error returned by [`PaymentStore::record`].
///
/// `#[non_exhaustive]` so a backend can grow new failure modes without a
/// breaking change; the middleware matches on [`Conflict`](Self::Conflict)
/// explicitly and treats everything else as a hard error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PaymentStoreError {
    /// A record already exists for this `idempotency_key`.
    ///
    /// This is **not** a failure of the operation's intent: it means the payment
    /// was already recorded (often by a concurrent in-flight duplicate that won
    /// the race). The caller should fall back to the existing record via
    /// [`PaymentStore::find`] and treat the request as already-paid.
    #[error("payment already recorded for idempotency key")]
    Conflict,

    /// A backend / driver error occurred while reading or writing the store.
    #[error("payment store backend error: {0}")]
    Backend(String),
}

/// Persistence boundary for settled-payment records, enabling idempotent paid
/// actions.
///
/// Implementations must be [`Send`] + [`Sync`] so the store can be shared across
/// async tasks behind an `Arc`. The trait is object-safe: hold it as
/// `Arc<dyn PaymentStore>`.
#[async_trait]
pub trait PaymentStore: Send + Sync {
    /// Looks up the record for `idempotency_key`, returning `None` when no
    /// payment has been recorded under it yet.
    ///
    /// # Errors
    ///
    /// Returns [`PaymentStoreError::Backend`] on a driver-level failure.
    async fn find(&self, idempotency_key: &str)
        -> Result<Option<PaymentRecord>, PaymentStoreError>;

    /// Inserts `record`, which must be **unique** on its `idempotency_key`.
    ///
    /// A uniqueness violation is reported as [`PaymentStoreError::Conflict`] —
    /// the signal that the payment was already recorded (typically by a
    /// concurrent duplicate). Callers handle that by re-reading via [`find`] and
    /// proceeding with the existing record rather than charging again.
    ///
    /// [`find`]: PaymentStore::find
    ///
    /// # Errors
    ///
    /// - [`PaymentStoreError::Conflict`] if a record already exists for the key.
    /// - [`PaymentStoreError::Backend`] on a driver-level failure.
    async fn record(&self, record: &PaymentRecord) -> Result<(), PaymentStoreError>;
}

/// Derives a **stable idempotency key** from a decoded `X-PAYMENT` payload.
///
/// ## Choice of key
///
/// - **Exact / EIP-3009 scheme** (`scheme == "exact"`): the inner authorization
///   carries a `nonce` that is **single-use on-chain** — the token contract
///   rejects a second `transferWithAuthorization` with the same `(from, nonce)`.
///   That nonce is therefore the natural idempotency key: two requests with the
///   same authorization *are* the same payment, and a third party cannot forge a
///   collision without also forging a valid signature over that nonce. We read
///   it from `payload.authorization.nonce`, falling back to a top-level
///   `payload.nonce` for payload shapes that hoist it.
///
/// - **Any other scheme** (or an exact payload missing a nonce): we fall back to
///   a SHA-256 hash over the *canonical* JSON of the whole payload (scheme,
///   network, version, and the inner `payload`, with object keys sorted). Two
///   byte-identical payloads hash equal; any difference yields a different key.
///   This is a content hash, so it is stable and collision-resistant but, unlike
///   the on-chain nonce, does not by itself guarantee single-use at the chain
///   level — it only deduplicates *identical* retries, which is exactly the
///   idempotency property we need here.
///
/// The returned key is an opaque, printable ASCII string safe to use as a
/// database primary key.
#[must_use]
pub fn idempotency_key(payload: &PaymentPayload) -> String {
    if payload.scheme == EXACT_SCHEME {
        if let Some(nonce) = extract_nonce(&payload.payload) {
            return format!("exact:{}:{}", payload.network, nonce);
        }
    }
    // Fallback: a content hash over the canonical payload.
    format!("hash:{}", canonical_payload_hash(payload))
}

/// Pulls the EIP-3009 authorization `nonce` out of an exact-scheme inner
/// payload, accepting either `{"authorization": {"nonce": ...}}` or a hoisted
/// top-level `{"nonce": ...}`. Returns the nonce as a string when present and
/// non-empty.
fn extract_nonce(inner: &serde_json::Value) -> Option<String> {
    let nonce = inner
        .get("authorization")
        .and_then(|auth| auth.get("nonce"))
        .or_else(|| inner.get("nonce"))?;
    let s = match nonce {
        serde_json::Value::String(s) => s.clone(),
        // A numeric or other scalar nonce is rendered via its JSON form so it
        // round-trips to a stable string.
        other if other.is_number() => other.to_string(),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}

/// SHA-256 over the canonical JSON serialization of the payload.
///
/// `serde_json::to_vec` on a [`PaymentPayload`] is deterministic for a given
/// value: struct fields serialize in declaration order, and the inner
/// `serde_json::Value` is canonicalized first so that semantically-equal objects
/// with differently-ordered keys produce the same bytes.
fn canonical_payload_hash(payload: &PaymentPayload) -> String {
    let canonical = serde_json::json!({
        "x402Version": payload.x402_version,
        "scheme": payload.scheme,
        "network": payload.network,
        "payload": canonicalize(&payload.payload),
    });
    // `serde_json` writes object keys of a `Map` in their stored order; the
    // top-level literal above is fixed, and `canonicalize` sorts every nested
    // object, so the byte stream is stable across equal values.
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    hex_encode(&digest)
}

/// Recursively sorts the keys of every object so equal payloads canonicalize to
/// identical bytes regardless of original key order.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let out: serde_json::Map<String, serde_json::Value> = sorted
                .into_iter()
                .map(|(k, v)| (k.clone(), canonicalize(v)))
                .collect();
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize).collect())
        }
        other => other.clone(),
    }
}

/// Lowercase hex-encodes bytes without pulling in an extra dependency.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_payload(nonce: &str) -> PaymentPayload {
        PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "base-sepolia".into(),
            payload: serde_json::json!({
                "from": "0xPayer",
                "authorization": { "nonce": nonce, "value": "10000" },
            }),
        }
    }

    #[test]
    fn exact_scheme_uses_authorization_nonce() {
        let key = idempotency_key(&exact_payload("0xabc123"));
        assert_eq!(key, "exact:base-sepolia:0xabc123");
    }

    #[test]
    fn exact_scheme_accepts_hoisted_nonce() {
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "base".into(),
            payload: serde_json::json!({ "nonce": "n-42" }),
        };
        assert_eq!(idempotency_key(&payload), "exact:base:n-42");
    }

    #[test]
    fn same_nonce_yields_same_key() {
        let a = idempotency_key(&exact_payload("0xsame"));
        let b = idempotency_key(&exact_payload("0xsame"));
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_nonces_yield_distinct_keys() {
        let a = idempotency_key(&exact_payload("0xone"));
        let b = idempotency_key(&exact_payload("0xtwo"));
        assert_ne!(a, b);
    }

    #[test]
    fn non_exact_scheme_falls_back_to_content_hash() {
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "streaming".into(),
            network: "base".into(),
            payload: serde_json::json!({ "channel": "abc" }),
        };
        let key = idempotency_key(&payload);
        assert!(key.starts_with("hash:"), "got {key}");
        // Stable across calls.
        assert_eq!(key, idempotency_key(&payload));
    }

    #[test]
    fn exact_without_nonce_falls_back_to_hash() {
        let payload = PaymentPayload {
            x402_version: 1,
            scheme: "exact".into(),
            network: "base".into(),
            payload: serde_json::json!({ "from": "0xPayer" }),
        };
        assert!(idempotency_key(&payload).starts_with("hash:"));
    }

    #[test]
    fn content_hash_is_key_order_independent() {
        let a = PaymentPayload {
            x402_version: 1,
            scheme: "streaming".into(),
            network: "base".into(),
            payload: serde_json::json!({ "a": 1, "b": 2 }),
        };
        let b = PaymentPayload {
            x402_version: 1,
            scheme: "streaming".into(),
            network: "base".into(),
            payload: serde_json::json!({ "b": 2, "a": 1 }),
        };
        assert_eq!(idempotency_key(&a), idempotency_key(&b));
    }

    #[test]
    fn record_round_trips_to_settlement() {
        let record = PaymentRecord {
            idempotency_key: "exact:base:0x1".into(),
            payer: "0xPayer".into(),
            amount: "10000".into(),
            asset: "0xUSDC".into(),
            network: "base".into(),
            transaction: Some("0xhash".into()),
            resource: "/seeks".into(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let settlement = record.settlement();
        assert_eq!(settlement.payer, "0xPayer");
        assert_eq!(settlement.transaction.as_deref(), Some("0xhash"));
    }
}
