//! Request-ID propagation helpers.
//!
//! The MCS server uses the `x-request-id` header to correlate log lines for a
//! single HTTP exchange across service boundaries.  This module wraps
//! [`tower_http::request_id`] to expose a ready-to-use pair of Tower layers.
//!
//! ## How it works
//!
//! 1. [`SetRequestIdLayer`](tower_http::request_id::SetRequestIdLayer) inspects
//!    the incoming request.  If an `x-request-id` header is already present its
//!    value is forwarded unchanged; otherwise a new UUID v4 is generated and
//!    inserted.
//! 2. [`PropagateRequestIdLayer`](tower_http::request_id::PropagateRequestIdLayer)
//!    copies the request ID determined in step 1 into the outgoing response
//!    header so callers can correlate their own logs.

use http::HeaderName;
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use uuid::Uuid;

/// The canonical header used to carry request IDs throughout the MCS stack.
pub static REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Generates a UUID v4 request ID for every request that does not already
/// carry one.
#[derive(Debug, Clone, Copy, Default)]
pub struct UuidRequestId;

impl MakeRequestId for UuidRequestId {
    fn make_request_id<B>(&mut self, _request: &http::Request<B>) -> Option<RequestId> {
        let id = Uuid::new_v4().to_string();
        let value = id
            .parse::<http::HeaderValue>()
            .expect("UUID is always a valid header value");
        Some(RequestId::new(value))
    }
}

/// Returns a [`SetRequestIdLayer`] / [`PropagateRequestIdLayer`] pair.
///
/// Insert both layers into your Tower [`ServiceBuilder`](tower::ServiceBuilder)
/// **before** [`crate::http_trace_layer`] so that the request span already
/// contains the ID when it is created.
///
/// # Example
///
/// ```rust,no_run
/// use mcs_observability::{http_trace_layer, request_id_layers};
/// use tower::ServiceBuilder;
///
/// let (set_id, propagate_id) = request_id_layers();
/// let _stack = ServiceBuilder::new()
///     .layer(set_id)
///     .layer(propagate_id)
///     .layer(http_trace_layer());
/// ```
pub fn request_id_layers() -> (SetRequestIdLayer<UuidRequestId>, PropagateRequestIdLayer) {
    let set = SetRequestIdLayer::new(REQUEST_ID_HEADER.clone(), UuidRequestId);
    let propagate = PropagateRequestIdLayer::new(REQUEST_ID_HEADER.clone());
    (set, propagate)
}
