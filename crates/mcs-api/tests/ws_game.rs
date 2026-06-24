//! End-to-end integration tests for the live-game WebSocket endpoint.
//!
//! These bind the real [`axum::Router`] to an ephemeral TCP port and connect a
//! genuine [`tokio_tungstenite`] client, so the whole path — query-param token
//! auth, the upgrade, the opening snapshot, a submitted move, and the resulting
//! broadcast update — is exercised exactly as a browser would drive it.
//!
//! The backing store is an in-memory SQLite database; the standard-chess variant
//! supplies a real [`GameSession`](mcs_core::GameSession) whose actor is spawned
//! and registered in the [`GameHub`](mcs_api::GameHub).

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as ClientWsMessage;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::VariantOptions;
use mcs_domain::{Game, TimeControl, User};
use mcs_game::GameActor;
use mcs_storage::{GameRepo, Repositories, SqlxStorage};
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

/// The concrete storage is kept alongside the [`AppState`] so the game actor can
/// be handed an `Arc<dyn GameRepo>` over the very same in-memory database the
/// API reads through `Arc<dyn Repositories>`.
struct TestApp {
    state: AppState,
    storage: Arc<SqlxStorage>,
}

/// Builds an [`AppState`] backed by a fresh in-memory SQLite database.
async fn test_app() -> TestApp {
    let storage = SqlxStorage::connect("sqlite::memory:")
        .await
        .expect("connect + migrate in-memory sqlite");
    let storage = Arc::new(storage);
    let repositories: Arc<dyn Repositories> = storage.clone();

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
        state: AppState::new(repositories, session, siwe),
        storage,
    }
}

/// Persists a fresh user with a random address and returns it.
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

/// Creates a standard-chess game between `white` and `black`, persists the
/// record, spawns its actor, and registers the handle in the hub. Returns the
/// game id.
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
        white.id,
        black.id,
        time_control.clone(),
        OffsetDateTime::now_utc(),
    );
    let game_id = game.id;
    app.state
        .storage()
        .games()
        .create(&game)
        .await
        .expect("persist game record");

    // The actor needs a bare `GameRepo`; hand it the same SqlxStorage the API
    // reads through, so both see one in-memory database.
    let repo: Arc<dyn GameRepo> = app.storage.clone();
    let handle = GameActor::spawn(game_id, session, repo, time_control);
    app.state.game_hub().insert(game_id, handle);

    game_id
}

/// Binds the router to an ephemeral port and serves it on a background task.
/// Returns the bound socket address.
async fn serve(state: AppState) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let app = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

/// Reads frames from the socket until a JSON text message arrives, returning it.
async fn next_json<S>(socket: &mut S) -> Value
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
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_then_move_advances_the_board() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let token = issue_session(app.state.session_config(), white.id).expect("mint token");
    let addr = serve(app.state).await;

    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let (mut socket, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws handshake succeeds");

    // 1. The opening frame is a Snapshot from White's perspective.
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["your_color"], "white");
    assert_eq!(snapshot["protocol_version"], 1);
    let start_fen = snapshot["view"]["fen"].as_str().expect("fen present");
    assert!(
        start_fen.starts_with("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w"),
        "snapshot is the initial position; got {start_fen}"
    );

    // 2. Submit a legal opening move (1. e4) as White.
    let submit = json!({ "type": "submit", "action": { "type": "move", "uci": "e2e4" } });
    socket
        .send(ClientWsMessage::Text(submit.to_string()))
        .await
        .expect("send submit");

    // 3. The move comes back as an Update carrying the move event, and the board
    //    has advanced (it is now Black to move).
    let update = next_json(&mut socket).await;
    assert_eq!(update["type"], "update", "got {update}");
    let new_fen = update["view"]["fen"].as_str().expect("fen present");
    assert!(
        new_fen.contains(" b "),
        "after 1. e4 it is Black to move; got {new_fen}"
    );
    assert_ne!(new_fen, start_fen, "the board must have advanced");
    let events = update["event"]["events"].as_array().expect("events array");
    assert!(!events.is_empty(), "the update carries the move event");
}

// ---------------------------------------------------------------------------
// Spectator path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spectator_submit_is_rejected_without_closing() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    // A third user who is not a player in the game.
    let bob = create_user(&app, "0x3333333333333333333333333333333333333333").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let token = issue_session(app.state.session_config(), bob.id).expect("mint token");
    let addr = serve(app.state).await;

    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let (mut socket, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws handshake succeeds");

    // The spectator's snapshot carries a null color.
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert!(snapshot["your_color"].is_null(), "spectator has no color");

    // A spectator's submit is rejected with an Error frame; the socket stays open.
    let submit = json!({ "type": "submit", "action": { "type": "move", "uci": "e2e4" } });
    socket
        .send(ClientWsMessage::Text(submit.to_string()))
        .await
        .expect("send submit");

    let reply = next_json(&mut socket).await;
    assert_eq!(reply["type"], "error", "got {reply}");
    assert!(reply["message"].as_str().unwrap().contains("spectator"));
}

// ---------------------------------------------------------------------------
// Negative auth cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_token_is_rejected() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;
    let addr = serve(app.state).await;

    // No `?token=` at all: the handshake must fail (axum rejects the query
    // extraction before the upgrade).
    let url = format!("ws://{addr}/ws/game/{game_id}");
    let result = tokio_tungstenite::connect_async(url).await;
    assert!(
        result.is_err(),
        "handshake without a token must be rejected"
    );
}

#[tokio::test]
async fn invalid_token_is_rejected() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;
    let addr = serve(app.state).await;

    let url = format!("ws://{addr}/ws/game/{game_id}?token=not.a.valid.jwt");
    let result = tokio_tungstenite::connect_async(url).await;
    assert!(
        result.is_err(),
        "handshake with an invalid token must be rejected"
    );
}
