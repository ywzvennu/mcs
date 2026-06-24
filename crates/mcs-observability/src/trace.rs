//! HTTP request-tracing [`tower_http::trace::TraceLayer`].

use http::Request;
use tower_http::{
    classify::{ServerErrorsAsFailures, SharedClassifier},
    trace::{
        DefaultOnBodyChunk, DefaultOnEos, DefaultOnFailure, DefaultOnRequest, DefaultOnResponse,
        MakeSpan, TraceLayer,
    },
};
use tracing::{Level, Span};

/// A [`MakeSpan`] implementation that records the HTTP method, URI path, and
/// the `x-request-id` header (if present) as span fields.
///
/// This is the `MakeSpan` type used inside [`HttpTraceLayer`].  Consumers
/// generally do not need to reference it directly.
#[derive(Debug, Clone, Copy, Default)]
pub struct McsSpan;

impl<B> MakeSpan<B> for McsSpan {
    fn make_span(&mut self, request: &Request<B>) -> Span {
        let request_id = request
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");

        tracing::span!(
            Level::INFO,
            "http_request",
            method     = %request.method(),
            path       = %request.uri().path(),
            request_id = %request_id,
        )
    }
}

/// The concrete type returned by [`http_trace_layer`].
///
/// This type alias spells out all seven generic parameters of
/// [`TraceLayer`] so that downstream callers can name the type when needed
/// (e.g. when storing it in a struct field).
pub type HttpTraceLayer = TraceLayer<
    SharedClassifier<ServerErrorsAsFailures>,
    McsSpan,
    DefaultOnRequest,
    DefaultOnResponse,
    DefaultOnBodyChunk,
    DefaultOnEos,
    DefaultOnFailure,
>;

/// Returns a [`TraceLayer`] pre-configured for MCS HTTP services.
///
/// The layer:
///
/// - Creates a new `http_request` span for every request, recording the HTTP
///   **method**, **path**, and **request ID** (from `x-request-id` if
///   present).
/// - Logs the response **status code** and **latency** at `INFO` level when
///   the response is complete.
/// - Logs failures at `ERROR` level.
///
/// Pair this with [`crate::request_id_layers`] so that `x-request-id` is
/// always present when the span is created.
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
pub fn http_trace_layer() -> HttpTraceLayer {
    TraceLayer::new_for_http()
        .make_span_with(McsSpan)
        .on_response(
            DefaultOnResponse::new()
                .level(Level::INFO)
                .latency_unit(tower_http::LatencyUnit::Millis),
        )
        .on_failure(DefaultOnFailure::new().level(Level::ERROR))
}
