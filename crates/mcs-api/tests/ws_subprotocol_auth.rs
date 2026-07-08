//! Integration tests for the `Sec-WebSocket-Protocol` bearer-token handshake.
//!
//! These tests exercise the new browser-compatible authentication path (#103):
//! the client offers `["mcs.v1", "mcs.token.<jwt>"]` as subprotocols, the server
//! extracts the JWT, validates it, and echoes back only `mcs.v1` — never the
//! secret token. The legacy `?token=` query-parameter path is also verified to
//! remain functional (with a deprecation notice in rustdoc). Missing or invalid
//! tokens via both paths are confirmed to be rejected.
//!
//! The backing store is an in-memory SQLite database; the standard-chess variant
//! supplies a real game session for the upgrade tests.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::Message as ClientWsMessage;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::VariantOptions;
use mcs_domain::{Game, TimeControl, User};
use mcs_game::GameActor;
use mcs_storage::{ActionLogRepo, GameRepo, SqlxStorage};
use mcs_variant_mcr::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Shared test wiring (mirrors ws_game.rs)
// ---------------------------------------------------------------------------

struct TestApp {
    state: AppState,
    storage: Arc<SqlxStorage>,
}

async fn test_app() -> TestApp {
    let storage = SqlxStorage::connect("sqlite::memory:")
        .await
        .expect("connect + migrate in-memory sqlite");
    let storage = Arc::new(storage);

    let mut registry = mcs_core::VariantRegistry::new();
    register(&mut registry);
    let variants = Arc::new(registry);

    let session = SessionConfig::new(
        b"test-secret-key-that-is-definitely-32-bytes!!".to_vec(),
        time::Duration::hours(1),
        "mcs-test".to_owned(),
    );
    let siwe = SiweConfig::new(
        "localhost".to_owned(),
        "https://localhost".to_owned(),
        1,
        "Sign in to MCS.".to_owned(),
        time::Duration::minutes(10),
    );
    TestApp {
        state: AppState::new(storage.clone(), variants, session, siwe),
        storage,
    }
}

async fn create_user(app: &TestApp, address: &str) -> User {
    let user = User::new(
        address.parse().expect("valid evm address"),
        None,
        OffsetDateTime::now_utc(),
    );
    app.state
        .storage()
        .users()
        .create(&user)
        .await
        .expect("create user");
    user
}

async fn spawn_game(app: &TestApp, white: &User, black: &User) -> mcs_domain::GameId {
    let mut registry = mcs_core::VariantRegistry::new();
    register(&mut registry);
    let session = registry
        .new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
        .expect("standard variant registered");

    let time_control = TimeControl::RealTime {
        initial: Duration::from_secs(300),
        increment: Duration::from_secs(2),
    };
    let game = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        white.id,
        black.id,
        time_control.clone(),
        true,
        OffsetDateTime::now_utc(),
    );
    let game_id = game.id;
    app.state
        .storage()
        .games()
        .create(&game)
        .await
        .expect("persist game record");

    let repo: Arc<dyn GameRepo> = app.storage.clone();
    let action_log: Arc<dyn ActionLogRepo> = app.storage.clone();
    let hook = app.state.completion_hook().clone();
    let handle = GameActor::spawn(game_id, session, repo, action_log, hook, time_control);
    app.state.game_hub().insert(game_id, handle);

    game_id
}

async fn serve(state: AppState) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let app = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

/// Reads frames from the socket until a JSON text message arrives.
async fn next_json<S>(socket: &mut S) -> serde_json::Value
where
    S: StreamExt<Item = Result<ClientWsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("frame within timeout")
            .expect("stream not ended")
            .expect("frame is ok");
        if let ClientWsMessage::Text(text) = message {
            return serde_json::from_str(&text).expect("frame is JSON");
        }
    }
}

// ---------------------------------------------------------------------------
// Subprotocol-auth happy path
// ---------------------------------------------------------------------------

/// Connecting with the JWT in `Sec-WebSocket-Protocol` succeeds.
///
/// The server echoes `mcs.v1` — and only `mcs.v1` — in the upgrade response.
/// The opening snapshot arrives and is well-formed, proving the connection was
/// authenticated and the game session was established.
#[tokio::test]
async fn subprotocol_token_authenticates_and_server_echoes_mcs_v1() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let token = issue_session(app.state.session_config(), white.id)
        .expect("mint token")
        .token;
    let addr = serve(app.state).await;

    // Build an HTTP upgrade request that offers both subprotocols.
    let url = format!("ws://{addr}/ws/game/{game_id}");
    let mut request = url.into_client_request().expect("valid request");
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        format!("mcs.v1, mcs.token.{token}")
            .parse()
            .expect("valid header value"),
    );

    let (mut socket, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws handshake should succeed");

    // The server must echo exactly `mcs.v1`, not the full token subprotocol.
    let negotiated = response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .expect("Sec-WebSocket-Protocol header present in response")
        .to_str()
        .expect("header is valid UTF-8");
    assert_eq!(
        negotiated, "mcs.v1",
        "server must echo mcs.v1 only; got {negotiated:?}"
    );
    assert!(
        !negotiated.contains("mcs.token"),
        "server must NOT echo the token subprotocol; got {negotiated:?}"
    );

    // The opening frame is a well-formed snapshot from White's perspective.
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["your_color"], "white");
    assert_eq!(snapshot["protocol_version"], 3);
}

// ---------------------------------------------------------------------------
// Legacy query-param path (backward compatibility)
// ---------------------------------------------------------------------------

/// The deprecated `?token=` query-parameter path still authenticates.
///
/// Non-browser clients that use this path continue to work; the handshake
/// succeeds and the opening snapshot is delivered.
#[tokio::test]
async fn legacy_query_token_still_authenticates() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let token = issue_session(app.state.session_config(), white.id)
        .expect("mint token")
        .token;
    let addr = serve(app.state).await;

    // No Sec-WebSocket-Protocol header: authenticate via the query string only.
    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let (mut socket, _response) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws handshake should succeed with legacy query-param token");

    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["your_color"], "white");
}

// ---------------------------------------------------------------------------
// Rejection cases
// ---------------------------------------------------------------------------

/// No token in either channel → 401, handshake fails.
#[tokio::test]
async fn no_token_is_rejected() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;
    let addr = serve(app.state).await;

    // Neither a subprotocol token nor a `?token=` param.
    let url = format!("ws://{addr}/ws/game/{game_id}");
    let result = tokio_tungstenite::connect_async(url).await;
    assert!(result.is_err(), "handshake with no token must be rejected");
}

/// An invalid JWT in the `mcs.token.<…>` subprotocol → 401, handshake fails.
#[tokio::test]
async fn invalid_subprotocol_token_is_rejected() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;
    let addr = serve(app.state).await;

    let url = format!("ws://{addr}/ws/game/{game_id}");
    let mut request = url.into_client_request().expect("valid request");
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        "mcs.v1, mcs.token.not.a.valid.jwt"
            .parse()
            .expect("valid header value"),
    );

    let result = tokio_tungstenite::connect_async(request).await;
    assert!(
        result.is_err(),
        "handshake with an invalid subprotocol token must be rejected"
    );
}

/// An invalid JWT in `?token=` → 401, handshake fails.
#[tokio::test]
async fn invalid_query_token_is_rejected() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;
    let addr = serve(app.state).await;

    let url = format!("ws://{addr}/ws/game/{game_id}?token=not.a.valid.jwt");
    let result = tokio_tungstenite::connect_async(url).await;
    assert!(
        result.is_err(),
        "handshake with an invalid query-param token must be rejected"
    );
}

/// Subprotocol token takes precedence over a bad query-param token.
///
/// When both channels are present and the subprotocol token is valid, the
/// connection succeeds even if `?token=` is garbage — the invalid fallback is
/// never reached.
#[tokio::test]
async fn subprotocol_token_takes_precedence_over_query_param() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let token = issue_session(app.state.session_config(), white.id)
        .expect("mint token")
        .token;
    let addr = serve(app.state).await;

    // Valid subprotocol token + a deliberately broken query-param token.
    let url = format!("ws://{addr}/ws/game/{game_id}?token=garbage-ignored");
    let mut request = url.into_client_request().expect("valid request");
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        format!("mcs.v1, mcs.token.{token}")
            .parse()
            .expect("valid header value"),
    );

    let (mut socket, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("subprotocol token should win over bad query-param");

    // The server must still echo mcs.v1.
    let negotiated = response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .expect("Sec-WebSocket-Protocol in response")
        .to_str()
        .expect("valid UTF-8");
    assert_eq!(negotiated, "mcs.v1");

    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["your_color"], "white");
}
