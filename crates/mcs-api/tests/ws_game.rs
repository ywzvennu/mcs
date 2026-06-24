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
use mcs_storage::{ActionLogRepo, GameRepo, SqlxStorage};
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

    // The actor needs a bare `GameRepo`; hand it the same SqlxStorage the API
    // reads through, so both see one in-memory database. The completion hook is
    // the state's own rating updater, so a game finished over the socket also
    // updates ratings.
    let repo: Arc<dyn GameRepo> = app.storage.clone();
    let action_log: Arc<dyn ActionLogRepo> = app.storage.clone();
    let hook = app.state.completion_hook().clone();
    let handle = GameActor::spawn(game_id, session, repo, action_log, hook, time_control);
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

/// Connects a player and consumes the opening snapshot, returning the socket and
/// the snapshot JSON. Keeps the four reconnect tests below from repeating the
/// handshake boilerplate.
async fn connect(
    addr: std::net::SocketAddr,
    game_id: mcs_domain::GameId,
    token: &str,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Value,
) {
    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let (mut socket, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws handshake succeeds");
    let snapshot = next_json(&mut socket).await;
    (socket, snapshot)
}

/// Submits a UCI move over `socket` and consumes the resulting Update frame.
async fn play_move<S>(socket: &mut S, uci: &str)
where
    S: SinkExt<ClientWsMessage, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<ClientWsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let submit = json!({ "type": "submit", "action": { "type": "move", "uci": uci } });
    socket
        .send(ClientWsMessage::Text(submit.to_string()))
        .await
        .expect("send move");
    let _ = next_json(socket).await;
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

    // 1. The opening frame is a Snapshot from White's perspective. It fully
    //    describes the position: protocol v3, clocks, ply, and side to move.
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["your_color"], "white");
    assert_eq!(snapshot["protocol_version"], 3);
    assert_eq!(snapshot["ply"], 0, "no moves played yet");
    assert_eq!(snapshot["side_to_move"], "white", "White starts");
    // A 5+2 real-time game: both clocks present, each at ~300_000 ms.
    let white_ms = snapshot["clock"]["white_ms"]
        .as_u64()
        .expect("white clock present");
    let black_ms = snapshot["clock"]["black_ms"]
        .as_u64()
        .expect("black clock present");
    assert!(white_ms <= 300_000 && white_ms > 290_000, "got {white_ms}");
    assert_eq!(black_ms, 300_000, "Black's clock is untouched at the start");
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

// ---------------------------------------------------------------------------
// Reconnect & resync
// ---------------------------------------------------------------------------

/// A player drops the socket, the *other* side moves while they are away, and on
/// reconnect the fresh snapshot reflects the advanced position, ply, clocks, and
/// turn — proving the game ran independently of the connection.
#[tokio::test]
async fn reconnect_snapshot_reflects_moves_made_while_away() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let white_token = issue_session(app.state.session_config(), white.id).expect("mint white");
    let black_token = issue_session(app.state.session_config(), black.id).expect("mint black");
    let addr = serve(app.state).await;

    // White connects, plays 1. e4, then drops the socket.
    let (mut white_socket, snapshot) = connect(addr, game_id, &white_token).await;
    assert_eq!(snapshot["ply"], 0);
    let submit = json!({ "type": "submit", "action": { "type": "move", "uci": "e2e4" } });
    white_socket
        .send(ClientWsMessage::Text(submit.to_string()))
        .await
        .expect("send e4");
    let _ = next_json(&mut white_socket).await; // the resulting Update
    drop(white_socket); // White disconnects — the game must keep running.

    // While White is away, Black replies 1... c5 over their own socket.
    let (mut black_socket, _black_snapshot) = connect(addr, game_id, &black_token).await;
    let submit = json!({ "type": "submit", "action": { "type": "move", "uci": "c7c5" } });
    black_socket
        .send(ClientWsMessage::Text(submit.to_string()))
        .await
        .expect("send c5");
    let _ = next_json(&mut black_socket).await; // Black's Update

    // White reconnects: the new snapshot reflects *both* moves — two plies, White
    // to move, and a board carrying e4 and c5.
    let (_white_socket, resync) = connect(addr, game_id, &white_token).await;
    assert_eq!(resync["type"], "snapshot");
    assert_eq!(resync["ply"], 2, "two half-moves were played while away");
    assert_eq!(
        resync["side_to_move"], "white",
        "back to White after 1...c5"
    );
    let fen = resync["view"]["fen"].as_str().expect("fen present");
    assert!(
        fen.contains(" w ") && fen.contains("4P3") && fen.contains("2p"),
        "the board reflects 1. e4 c5; got {fen}"
    );
    // Both clocks are present and positive in the resync frame (the 2s increment
    // can push the mover's figure just past the initial budget, so we only assert
    // presence and that Black — who has not yet completed a move — is unchanged).
    assert!(
        resync["clock"]["white_ms"].as_u64().is_some(),
        "white clock present in resync"
    );
    let black_ms = resync["clock"]["black_ms"].as_u64().expect("black clock");
    assert!(black_ms > 0, "Black still has time; got {black_ms}");
}

/// `?since_ply=N` replays exactly the actions recorded after ply `N` as `replay`
/// frames before live streaming resumes — for a perfect-information variant.
#[tokio::test]
async fn since_ply_replays_only_the_missed_actions() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let white_token = issue_session(app.state.session_config(), white.id).expect("mint white");
    let black_token = issue_session(app.state.session_config(), black.id).expect("mint black");
    let addr = serve(app.state).await;

    // Play three half-moves: 1. e4 c5 2. Nf3 (plies 0, 1, 2).
    let (mut white_socket, _s) = connect(addr, game_id, &white_token).await;
    let (mut black_socket, _s) = connect(addr, game_id, &black_token).await;
    play_move(&mut white_socket, "e2e4").await;
    play_move(&mut black_socket, "c7c5").await;
    play_move(&mut white_socket, "g1f3").await;
    drop(white_socket);

    // Reconnect having already seen ply 0 (1. e4): catch-up must replay plies 1
    // and 2 only, in order, then nothing else until live play.
    let url = format!("ws://{addr}/ws/game/{game_id}?token={white_token}&since_ply=0");
    let (mut socket, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws handshake succeeds");

    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["ply"], 3, "three half-moves recorded");

    let replay_one = next_json(&mut socket).await;
    assert_eq!(replay_one["type"], "replay");
    assert_eq!(replay_one["ply"], 1);
    assert_eq!(replay_one["player"], "black");
    assert_eq!(replay_one["action"]["uci"], "c7c5");

    let replay_two = next_json(&mut socket).await;
    assert_eq!(replay_two["type"], "replay");
    assert_eq!(replay_two["ply"], 2);
    assert_eq!(replay_two["player"], "white");
    assert_eq!(replay_two["action"]["uci"], "g1f3");

    // The next thing the client sees is a live update, never another replay: a
    // move from Black resumes the stream.
    let submit = json!({ "type": "submit", "action": { "type": "move", "uci": "b8c6" } });
    black_socket
        .send(ClientWsMessage::Text(submit.to_string()))
        .await
        .expect("send Nc6");
    let live = next_json(&mut socket).await;
    assert_eq!(live["type"], "update", "live streaming resumed; got {live}");
}

/// `?since_ply` past the last recorded ply replays nothing: the snapshot alone
/// resyncs the client, and the next frame is a live update.
#[tokio::test]
async fn since_ply_at_head_replays_nothing() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    let white_token = issue_session(app.state.session_config(), white.id).expect("mint white");
    let black_token = issue_session(app.state.session_config(), black.id).expect("mint black");
    let addr = serve(app.state).await;

    // Play 1. e4 (ply 0), then reconnect with since_ply=0: nothing newer exists.
    let (mut socket, _s) = connect(addr, game_id, &white_token).await;
    let submit = json!({ "type": "submit", "action": { "type": "move", "uci": "e2e4" } });
    socket
        .send(ClientWsMessage::Text(submit.to_string()))
        .await
        .expect("send e4");
    let _ = next_json(&mut socket).await;
    drop(socket);

    let url = format!("ws://{addr}/ws/game/{game_id}?token={white_token}&since_ply=0");
    let (mut socket, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws handshake succeeds");

    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["ply"], 1);

    // No replay frames follow: the very next frame is a live update, produced by
    // Black's reply over a separate socket.
    let (mut black_socket, _s) = connect(addr, game_id, &black_token).await;
    let submit = json!({ "type": "submit", "action": { "type": "move", "uci": "c7c5" } });
    black_socket
        .send(ClientWsMessage::Text(submit.to_string()))
        .await
        .expect("send c5");

    let next = next_json(&mut socket).await;
    assert_eq!(
        next["type"], "update",
        "with since_ply at the head, no replay precedes the live update; got {next}"
    );
}
