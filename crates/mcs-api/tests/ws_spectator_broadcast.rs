//! Cross-node spectator broadcast over the event bus (#109) — **no Redis**.
//!
//! This drives the real [`axum::Router`] with a genuine [`tokio_tungstenite`]
//! client, pinning a fixed two-node membership so *this* node is **not** the
//! owner of the game. A player on a non-owner node would be redirected, but a
//! spectator is served locally: it bootstraps from a read-only durable snapshot
//! and then streams every spectator frame the game's actor publishes on the
//! shared [`LocalEventBus`].
//!
//! Because the actor and the spectator share the *same* in-process bus
//! (`state.event_bus()`), this exercises the whole publish → subscribe → stream
//! path with no Redis: it stands in for "the owner node publishes, a watcher on
//! another node receives", which the env-gated Redis test then confirms across
//! two real bus instances.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as ClientWsMessage;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_cluster::{ClusterError, NodeInfo, NodeRegistry};
use mcs_core::{Action, Color, VariantOptions, VariantRegistry};
use mcs_domain::{Game, GameId, TimeControl, User};
use mcs_game::{GameActor, GameHandle};
use mcs_storage::{ActionLogRepo, GameRepo, SqlxStorage};
use mcs_variant_standard::wire::StandardAction;
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// A fixed-membership registry (no backend, no Redis).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FixedRegistry {
    nodes: Vec<NodeInfo>,
}

#[async_trait]
impl NodeRegistry for FixedRegistry {
    async fn register(&self) -> Result<(), ClusterError> {
        Ok(())
    }
    async fn heartbeat(&self) -> Result<(), ClusterError> {
        Ok(())
    }
    async fn leave(&self) -> Result<(), ClusterError> {
        Ok(())
    }
    async fn live_nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        Ok(self.nodes.clone())
    }
}

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

struct TestApp {
    state: AppState,
    storage: Arc<SqlxStorage>,
}

async fn test_app() -> TestApp {
    let storage = Arc::new(
        SqlxStorage::connect("sqlite::memory:")
            .await
            .expect("connect + migrate in-memory sqlite"),
    );

    let mut registry = VariantRegistry::new();
    register(&mut registry);

    let session = SessionConfig::new(
        b"test-secret-key-that-is-definitely-32-bytes!!".to_vec(),
        Duration::hours(1),
        "mcs-test".to_owned(),
    );
    let siwe = SiweConfig::new(
        "localhost".to_owned(),
        "https://localhost".to_owned(),
        1,
        "Sign in to MCS.".to_owned(),
        Duration::minutes(10),
    );
    TestApp {
        state: AppState::new(storage.clone(), Arc::new(registry), session, siwe),
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

/// Creates and persists a standard-chess game, then spawns its actor **on the
/// state's shared event bus** (so its spectator frames reach a subscriber of
/// that same bus). Returns the id and the live handle to drive moves directly.
async fn spawn_game_on_bus(app: &TestApp, white: &User, black: &User) -> (GameId, GameHandle) {
    let mut registry = VariantRegistry::new();
    register(&mut registry);
    let session = registry
        .new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
        .expect("standard variant registered");

    let time_control = TimeControl::RealTime {
        initial: StdDuration::from_secs(300),
        increment: StdDuration::from_secs(2),
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
    let handle = GameActor::spawn_with_bus(
        game_id,
        session,
        repo,
        action_log,
        hook,
        time_control,
        app.state.event_bus().clone(),
    );
    app.state.game_hub().insert(game_id, handle.clone());
    (game_id, handle)
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

async fn next_json<S>(socket: &mut S) -> Value
where
    S: StreamExt<Item = Result<ClientWsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = socket
            .next()
            .await
            .expect("a frame")
            .expect("a non-error frame");
        if let ClientWsMessage::Text(text) = message {
            return serde_json::from_str(&text).expect("frame is JSON");
        }
    }
}

fn mv(uci: &str) -> Action {
    Action::from_typed(&StandardAction::Move {
        uci: uci.to_owned(),
    })
    .expect("serializable")
}

/// Two nodes with distinct reachable addresses, and the one that is *not* the
/// rendezvous owner of `game_id` (so a spectator there takes the bus path).
fn non_owner_node_for(game_id: GameId) -> (Vec<NodeInfo>, NodeInfo) {
    let a = NodeInfo::new("node-a", "http://10.0.0.1:8080");
    let b = NodeInfo::new("node-b", "http://10.0.0.2:8080");
    let nodes = vec![a.clone(), b.clone()];
    let owner = mcs_cluster::owner(&game_id.to_string(), &nodes)
        .expect("two-node set has an owner")
        .clone();
    let this = if owner.id == a.id { b } else { a };
    assert_ne!(this.id, owner.id, "this node must be the non-owner");
    (nodes, this)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A spectator connected to a node that does NOT own the game is served locally
/// (no redirect): it gets an opening snapshot and then receives a live frame for
/// each move the owner's actor applies, delivered over the event bus.
#[tokio::test]
async fn spectator_on_a_non_owner_node_streams_moves_over_the_bus() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let bob = create_user(&app, "0x3333333333333333333333333333333333333333").await;

    let (game_id, handle) = spawn_game_on_bus(&app, &white, &black).await;

    // Pin a two-node set where this node is the non-owner, so the spectator path
    // (not a redirect) is exercised.
    let (nodes, this_node) = non_owner_node_for(game_id);
    let registry: Arc<dyn NodeRegistry> = Arc::new(FixedRegistry { nodes });
    let state = app.state.clone().with_cluster(registry, this_node);

    let token = issue_session(state.session_config(), bob.id)
        .expect("mint token")
        .token;
    let addr = serve(state).await;

    // The spectator connects: the handshake upgrades (NO redirect) and the first
    // frame is the bootstrap snapshot with a null colour.
    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let (mut socket, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("spectator handshake upgrades, not redirected");
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert!(snapshot["your_color"].is_null(), "spectator has no colour");
    assert_eq!(snapshot["ply"], 0, "no moves played yet");

    // The owner's actor applies a move (driven directly here, standing in for a
    // player on the owning node). Its spectator frame travels over the bus.
    handle
        .submit_action(Color::White, mv("e2e4"))
        .await
        .expect("white moves");

    // The spectator receives the move as a fresh snapshot at ply 1.
    let frame = next_json(&mut socket).await;
    assert_eq!(frame["type"], "snapshot", "got {frame}");
    assert!(frame["your_color"].is_null());
    assert_eq!(
        frame["ply"], 1,
        "the streamed frame reflects the played move"
    );

    // A second move streams likewise.
    handle
        .submit_action(Color::Black, mv("e7e5"))
        .await
        .expect("black moves");
    let frame = next_json(&mut socket).await;
    assert_eq!(frame["type"], "snapshot");
    assert_eq!(frame["ply"], 2);
}

/// A spectator's submit is rejected without closing the socket, even on the
/// bus-served (non-owner) path.
#[tokio::test]
async fn spectator_submit_is_rejected_on_the_bus_path() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let bob = create_user(&app, "0x3333333333333333333333333333333333333333").await;

    let (game_id, _handle) = spawn_game_on_bus(&app, &white, &black).await;
    let (nodes, this_node) = non_owner_node_for(game_id);
    let registry: Arc<dyn NodeRegistry> = Arc::new(FixedRegistry { nodes });
    let state = app.state.clone().with_cluster(registry, this_node);

    let token = issue_session(state.session_config(), bob.id)
        .expect("mint token")
        .token;
    let addr = serve(state).await;

    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let (mut socket, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("spectator handshake upgrades");
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");

    socket
        .send(ClientWsMessage::Text(
            serde_json::json!({ "type": "submit", "action": { "type": "move", "uci": "e2e4" } })
                .to_string(),
        ))
        .await
        .expect("send submit");

    let reply = next_json(&mut socket).await;
    assert_eq!(reply["type"], "error", "got {reply}");
    assert!(reply["message"].as_str().unwrap().contains("spectator"));
}
