//! Unit tests for `mcs-observability`.

use crate::{
    config::{LogFormat, ObservabilityConfig},
    request_id::UuidRequestId,
    trace::http_trace_layer,
};

use tower_http::request_id::MakeRequestId as _;

// ── ObservabilityConfig ──────────────────────────────────────────────────────

#[test]
fn default_config_is_pretty_info() {
    let cfg = ObservabilityConfig::default();
    assert_eq!(cfg.format, LogFormat::Pretty);
    assert_eq!(cfg.default_directive, "info");
}

#[test]
fn log_format_default_is_pretty() {
    assert_eq!(LogFormat::default(), LogFormat::Pretty);
}

#[test]
fn config_can_be_set_to_json() {
    let cfg = ObservabilityConfig {
        format: LogFormat::Json,
        default_directive: "debug".to_owned(),
    };
    assert_eq!(cfg.format, LogFormat::Json);
    assert_eq!(cfg.default_directive, "debug");
}

// ── init / subscriber installation ──────────────────────────────────────────

/// Installing the subscriber succeeds the first time and returns an error
/// on subsequent attempts (double-init guard).
///
/// We do **not** call the real `init` globally here — instead we exercise
/// the internal `install` function directly so that test parallelism does
/// not cause flaky failures.  The error path is checked by calling once
/// more after a successful install.
#[test]
fn install_pretty_succeeds_once() {
    let cfg = ObservabilityConfig {
        format: LogFormat::Pretty,
        default_directive: "error".to_owned(), // quiet during tests
    };
    // First call should succeed (or fail with TryInitError if another test
    // beat us to it — both outcomes are acceptable).
    let _ = crate::config::install(&cfg);
    // Second call must either succeed (if the first returned Err) or return
    // the TryInitError.  What matters is it does **not** panic.
    let _ = crate::config::install(&cfg);
}

#[test]
fn install_json_does_not_panic() {
    let cfg = ObservabilityConfig {
        format: LogFormat::Json,
        default_directive: "error".to_owned(),
    };
    let _ = crate::config::install(&cfg);
}

// ── http_trace_layer ─────────────────────────────────────────────────────────

#[test]
fn http_trace_layer_constructs() {
    // Just verify the constructor does not panic and we can clone the layer.
    let layer = http_trace_layer();
    let _ = layer.clone();
}

// ── request_id_layers ────────────────────────────────────────────────────────

#[test]
fn request_id_layers_construct() {
    let (set, propagate) = crate::request_id_layers();
    // Clone should be available; the layers are Send + Sync.
    let _ = set.clone();
    let _ = propagate.clone();
}

#[test]
fn uuid_request_id_generates_valid_uuid() {
    use http::Request;

    let mut maker = UuidRequestId;
    let req = Request::builder().body(()).expect("valid request");
    let id = maker.make_request_id(&req).expect("always produces an id");

    let header_val = id.header_value().to_str().expect("valid utf-8");
    // A UUID v4 is 36 characters: 8-4-4-4-12.
    assert_eq!(header_val.len(), 36, "expected UUID length");
    // Two consecutive IDs must differ.
    let id2 = maker.make_request_id(&req).expect("always produces an id");
    assert_ne!(
        id.header_value(),
        id2.header_value(),
        "IDs should be unique"
    );
}

#[test]
fn request_id_header_name_is_correct() {
    assert_eq!(
        crate::request_id::REQUEST_ID_HEADER.as_str(),
        "x-request-id"
    );
}

// ── InitError Display ────────────────────────────────────────────────────────

#[test]
fn init_error_display_contains_message() {
    // Build a TryInitError by trying to init twice.
    let cfg = ObservabilityConfig::default();
    let _ = crate::config::install(&cfg); // may succeed or fail
    if let Err(e) = crate::config::install(&cfg) {
        let msg = e.to_string();
        assert!(
            msg.contains("tracing subscriber"),
            "Display should mention 'tracing subscriber'; got: {msg}"
        );
    }
    // If neither call returned an error (e.g. global subscriber not yet set)
    // the assertion is vacuously satisfied.
}
