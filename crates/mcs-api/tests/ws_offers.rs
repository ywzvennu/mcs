//! End-to-end integration tests for the live draw-offer and rematch flows over
//! the game WebSocket (#84).
//!
//! These bind the real [`axum::Router`] to an ephemeral TCP port and connect
//! genuine [`tokio_tungstenite`] clients, so the whole path — token auth, the
//! upgrade, the opening snapshot, board-action draws, and the table
//! side-channel rematch frames — is exercised exactly as a browser would drive
//! it. The backing store is an in-memory SQLite database with the standard-chess
//! variant registered.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tower::ServiceExt;

use tokio_tungstenite::tungstenite::Message as ClientWsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::VariantOptions;
use mcs_domain::{Game, GameId, TimeControl, User};
use mcs_game::GameActor;
use mcs_storage::{ActionLogRepo, GameRepo, SqlxStorage};
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// The concrete storage is kept alongside [`AppState`] so the game actor can be
/// handed an `Arc<dyn GameRepo>` over the same in-memory database the API reads.
struct TestApp {
    state: AppState,
    storage: Arc<SqlxStorage>,
}

/// Builds an [`AppState`] backed by a fresh in-memory SQLite database.
async fn test_app() -> TestApp {
    let storage = Arc::new(
        SqlxStorage::connect("sqlite::memory:")
            .await
            .expect("connect + migrate in-memory sqlite"),
    );

    let mut registry = mcs_core::VariantRegistry::new();
    register(&mut registry);

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
        state: AppState::new(storage.clone(), Arc::new(registry), session, siwe),
        storage,
    }
}

/// Persists a fresh user with the given address and returns it.
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
/// record, spawns its actor, registers the handle, and returns the game id.
async fn spawn_game(app: &TestApp, white: &User, black: &User) -> GameId {
    let mut registry = mcs_core::VariantRegistry::new();
    register(&mut registry);
    let session = registry
        .new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
        .expect("standard variant registered");

    // Unlimited time control keeps the test deterministic (no clock to flag).
    let game = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        white.id,
        black.id,
        TimeControl::Unlimited,
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
    let handle = GameActor::spawn(
        game_id,
        session,
        repo,
        action_log,
        hook,
        TimeControl::Unlimited,
    );
    app.state.game_hub().insert(game_id, handle);
    game_id
}

/// Binds the router to an ephemeral port and serves it on a background task.
async fn serve(state: AppState) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let app = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

/// Reads frames until a JSON text message arrives, returning it.
async fn next_json(socket: &mut Socket) -> Value {
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

/// Connects a client and consumes the opening snapshot, returning the socket.
async fn connect(addr: std::net::SocketAddr, game_id: GameId, token: &str) -> Socket {
    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let (mut socket, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws handshake succeeds");
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    socket
}

/// Sends a [`ClientMessage`] JSON value over the socket.
async fn send(socket: &mut Socket, message: Value) {
    socket
        .send(ClientWsMessage::Text(message.to_string()))
        .await
        .expect("send frame");
}

/// Submits a typed standard action (e.g. `offer_draw`) on the socket.
fn submit(action_type: &str) -> Value {
    json!({ "type": "submit", "action": { "type": action_type } })
}

// ---------------------------------------------------------------------------
// Draw offers (verify the existing board-action path reaches both players)
// ---------------------------------------------------------------------------

/// A draw is just a board action: White submits `offer_draw`, the event reaches
/// **both** sockets, Black submits `accept_draw`, and the game finishes drawn for
/// both — all over the ordinary `Update` stream, no special message needed.
#[tokio::test]
async fn draw_offer_reaches_both_players_and_accept_draws_the_game() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let white_token = issue_session(app.state.session_config(), white.id).expect("white token");
    let black_token = issue_session(app.state.session_config(), black.id).expect("black token");
    let addr = serve(app.state).await;

    let mut white_socket = connect(addr, game_id, &white_token).await;
    let mut black_socket = connect(addr, game_id, &black_token).await;

    // 1. White offers a draw.
    send(&mut white_socket, submit("offer_draw")).await;

    // 2. The offer reaches BOTH players as a draw_offered event.
    let white_update = next_json(&mut white_socket).await;
    let black_update = next_json(&mut black_socket).await;
    for update in [&white_update, &black_update] {
        assert_eq!(update["type"], "update", "got {update}");
        let events = update["event"]["events"].as_array().expect("events");
        let offered = events
            .iter()
            .any(|e| e["type"] == "draw_offered" && e["by"] == "white");
        assert!(
            offered,
            "both players see draw_offered by white; got {update}"
        );
    }

    // 3. Black accepts; the game finishes drawn for BOTH.
    send(&mut black_socket, submit("accept_draw")).await;

    let white_final = next_json(&mut white_socket).await;
    let black_final = next_json(&mut black_socket).await;
    for update in [&white_final, &black_final] {
        assert_eq!(update["type"], "update", "got {update}");
        // The game-ended status is a finished draw: `{ "finished": { outcome } }`.
        let outcome = &update["event"]["status"]["finished"];
        assert!(outcome.is_object(), "status is finished; got {update}");
        assert_eq!(
            outcome["winner"],
            Value::Null,
            "a draw has no winner; got {outcome}"
        );
    }
}

// ---------------------------------------------------------------------------
// Rematch over the table side-channel
// ---------------------------------------------------------------------------

/// Drives the game to a finished state by having White resign over the socket,
/// consuming the resulting update on both sockets.
async fn finish_by_resignation(white_socket: &mut Socket, black_socket: &mut Socket) {
    send(white_socket, submit("resign")).await;
    let white_update = next_json(white_socket).await;
    let black_update = next_json(black_socket).await;
    for update in [&white_update, &black_update] {
        assert_eq!(update["type"], "update");
        assert!(
            update["event"]["status"]["finished"].is_object(),
            "the game finished on resignation; got {update}"
        );
    }
}

/// Full happy path: A and B finish a game, A offers a rematch, B receives the
/// offer, B accepts, both receive `rematch_accepted` with a `game_id`, and the
/// new game is real, playable, and has the colours swapped.
#[tokio::test]
async fn rematch_offer_accept_creates_swapped_game_for_both() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let white_token = issue_session(app.state.session_config(), white.id).expect("white token");
    let black_token = issue_session(app.state.session_config(), black.id).expect("black token");
    let state = app.state.clone();
    let addr = serve(app.state).await;

    let mut white_socket = connect(addr, game_id, &white_token).await;
    let mut black_socket = connect(addr, game_id, &black_token).await;

    // Finish the game (White resigns).
    finish_by_resignation(&mut white_socket, &mut black_socket).await;

    // White offers a rematch; both players receive rematch_offered { by: white }.
    send(&mut white_socket, json!({ "type": "rematch_offer" })).await;
    let white_offered = next_json(&mut white_socket).await;
    let black_offered = next_json(&mut black_socket).await;
    for frame in [&white_offered, &black_offered] {
        assert_eq!(frame["type"], "rematch_offered", "got {frame}");
        assert_eq!(frame["by"], "white");
    }

    // Black accepts; both receive rematch_accepted { game_id }.
    send(&mut black_socket, json!({ "type": "rematch_accept" })).await;
    let white_accepted = next_json(&mut white_socket).await;
    let black_accepted = next_json(&mut black_socket).await;
    let new_game_id = white_accepted["game_id"]
        .as_str()
        .expect("game_id present")
        .to_owned();
    for frame in [&white_accepted, &black_accepted] {
        assert_eq!(frame["type"], "rematch_accepted", "got {frame}");
        assert_eq!(frame["game_id"], new_game_id);
    }

    // The new game is a real, playable game with colours swapped: B is White, A
    // is Black. Verify through the REST `GET /games/{id}` endpoint.
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri(format!("/games/{new_game_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "new game retrievable");
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let fetched: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(fetched["lifecycle"], "active");
    assert_eq!(
        fetched["white"].as_str().unwrap(),
        black.id.to_string(),
        "Black becomes White in the rematch (colours swapped)"
    );
    assert_eq!(
        fetched["black"].as_str().unwrap(),
        white.id.to_string(),
        "White becomes Black in the rematch (colours swapped)"
    );
}

/// A spectator may not offer a rematch: an offer comes back as an `error`.
#[tokio::test]
async fn spectator_rematch_offer_is_rejected() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let bob = create_user(&app, "0x3333333333333333333333333333333333333333").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let white_token = issue_session(app.state.session_config(), white.id).expect("white token");
    let black_token = issue_session(app.state.session_config(), black.id).expect("black token");
    let bob_token = issue_session(app.state.session_config(), bob.id).expect("bob token");
    let addr = serve(app.state).await;

    let mut white_socket = connect(addr, game_id, &white_token).await;
    let mut black_socket = connect(addr, game_id, &black_token).await;
    let mut spectator = connect(addr, game_id, &bob_token).await;

    finish_by_resignation(&mut white_socket, &mut black_socket).await;
    // The spectator also observes the resignation update; drain it first.
    let resign_seen = next_json(&mut spectator).await;
    assert_eq!(resign_seen["type"], "update", "got {resign_seen}");

    // The spectator tries to offer a rematch.
    send(&mut spectator, json!({ "type": "rematch_offer" })).await;
    let reply = next_json(&mut spectator).await;
    assert_eq!(reply["type"], "error", "got {reply}");
    assert!(
        reply["message"].as_str().unwrap().contains("spectator"),
        "got {reply}"
    );
}

/// Offering a rematch while the game is still ongoing is rejected with an error.
#[tokio::test]
async fn rematch_offer_on_unfinished_game_is_rejected() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let white_token = issue_session(app.state.session_config(), white.id).expect("white token");
    let addr = serve(app.state).await;

    let mut white_socket = connect(addr, game_id, &white_token).await;

    // The game is still ongoing; an offer is rejected.
    send(&mut white_socket, json!({ "type": "rematch_offer" })).await;
    let reply = next_json(&mut white_socket).await;
    assert_eq!(reply["type"], "error", "got {reply}");
    assert!(
        reply["message"].as_str().unwrap().contains("finished"),
        "got {reply}"
    );
}

/// The offerer cannot accept their own offer: it comes back as an error, no game
/// is created, and the offer stays pending so the opponent can still accept.
#[tokio::test]
async fn offerer_cannot_accept_their_own_rematch_offer() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let white_token = issue_session(app.state.session_config(), white.id).expect("white token");
    let black_token = issue_session(app.state.session_config(), black.id).expect("black token");
    let addr = serve(app.state).await;

    let mut white_socket = connect(addr, game_id, &white_token).await;
    let mut black_socket = connect(addr, game_id, &black_token).await;

    finish_by_resignation(&mut white_socket, &mut black_socket).await;

    // White offers; both see the offer.
    send(&mut white_socket, json!({ "type": "rematch_offer" })).await;
    let _ = next_json(&mut white_socket).await; // rematch_offered to White
    let _ = next_json(&mut black_socket).await; // rematch_offered to Black

    // White tries to accept their own offer → error, and no rematch_accepted.
    send(&mut white_socket, json!({ "type": "rematch_accept" })).await;
    let reply = next_json(&mut white_socket).await;
    assert_eq!(reply["type"], "error", "got {reply}");
    assert!(
        reply["message"].as_str().unwrap().contains("your own"),
        "got {reply}"
    );

    // The offer is still live: Black can accept it, which now creates the game.
    send(&mut black_socket, json!({ "type": "rematch_accept" })).await;
    let accepted = next_json(&mut black_socket).await;
    assert_eq!(accepted["type"], "rematch_accepted", "got {accepted}");
    assert!(accepted["game_id"].is_string());
}

/// Declining a pending offer clears it and notifies both players.
#[tokio::test]
async fn rematch_decline_clears_the_offer_for_both() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let white_token = issue_session(app.state.session_config(), white.id).expect("white token");
    let black_token = issue_session(app.state.session_config(), black.id).expect("black token");
    let addr = serve(app.state).await;

    let mut white_socket = connect(addr, game_id, &white_token).await;
    let mut black_socket = connect(addr, game_id, &black_token).await;

    finish_by_resignation(&mut white_socket, &mut black_socket).await;

    // White offers, Black declines; both see rematch_declined { by: black }.
    send(&mut white_socket, json!({ "type": "rematch_offer" })).await;
    let _ = next_json(&mut white_socket).await;
    let _ = next_json(&mut black_socket).await;

    send(&mut black_socket, json!({ "type": "rematch_decline" })).await;
    let white_declined = next_json(&mut white_socket).await;
    let black_declined = next_json(&mut black_socket).await;
    for frame in [&white_declined, &black_declined] {
        assert_eq!(frame["type"], "rematch_declined", "got {frame}");
        assert_eq!(frame["by"], "black");
    }
}
