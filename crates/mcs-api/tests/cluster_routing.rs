//! Cluster-aware WebSocket routing tests (#68) — **no Redis required**.
//!
//! These bind the real [`axum::Router`] to an ephemeral TCP port and drive it
//! with a genuine [`tokio_tungstenite`] client, injecting a tiny in-memory
//! [`NodeRegistry`] that reports a fixed two-node live set. With membership
//! fixed, the rendezvous owner of any game id is deterministic, so we can assert
//! both branches of the routing decision exactly:
//!
//! - when **another** node owns the game, the handshake is answered with **421
//!   Misdirected Request** and a body naming the owner — the socket is *not*
//!   upgraded; and
//! - when **this** node owns the game, the handshake upgrades and is served
//!   locally, exactly as single-node.
//!
//! A real socket is required because the redirect/serve decision happens around
//! the WebSocket upgrade, which hyper only performs over a live connection.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use time::{Duration, OffsetDateTime};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_cluster::{ClusterError, NodeInfo, NodeRegistry};
use mcs_core::{VariantOptions, VariantRegistry};
use mcs_domain::{Game, GameId, TimeControl, User};
use mcs_game::GameActor;
use mcs_storage::{ActionLogRepo, GameRepo, SqlxStorage};
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// A fixed-membership registry for tests (no backend, no Redis).
// ---------------------------------------------------------------------------

/// A [`NodeRegistry`] whose `live_nodes` always returns a fixed set.
///
/// This lets a test pin the live membership so the rendezvous owner of a game id
/// is deterministic, without touching Redis or any real coordination backend.
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
    let storage = SqlxStorage::connect("sqlite::memory:")
        .await
        .expect("connect + migrate in-memory sqlite");
    let storage = Arc::new(storage);

    let mut registry = VariantRegistry::new();
    register(&mut registry);
    let variants = Arc::new(registry);

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

/// Creates a standard-chess game, persists it, spawns its actor, and registers
/// the handle in the hub. Returns the game id.
async fn spawn_game(app: &TestApp, white: &User, black: &User) -> GameId {
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
    let handle = GameActor::spawn(game_id, session, repo, action_log, hook, time_control);
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

/// Two nodes, `node-a` and `node-b`, with distinct reachable addresses.
fn two_nodes() -> (NodeInfo, NodeInfo) {
    (
        NodeInfo::new("node-a", "http://10.0.0.1:8080"),
        NodeInfo::new("node-b", "http://10.0.0.2:8080"),
    )
}

// ---------------------------------------------------------------------------
// Routing: another node owns the game -> redirect, no upgrade.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn redirects_when_another_node_owns_the_game() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;
    let token = issue_session(app.state.session_config(), white.id).expect("mint token");

    // Pin a two-node membership, then choose *this* node to be the non-owner of
    // the game so the handler must redirect to the rendezvous owner.
    let (a, b) = two_nodes();
    let nodes = vec![a.clone(), b.clone()];
    let owner = mcs_cluster::owner(&game_id.to_string(), &nodes)
        .expect("two-node set has an owner")
        .clone();
    let this_node = if owner.id == a.id {
        b.clone()
    } else {
        a.clone()
    };
    assert_ne!(this_node.id, owner.id, "this node is the non-owner");

    let registry: Arc<dyn NodeRegistry> = Arc::new(FixedRegistry { nodes });
    let state = app.state.clone().with_cluster(registry, this_node);
    let addr = serve(state).await;

    // The handshake to a non-owner is rejected before the upgrade: tungstenite
    // surfaces the non-101 response as `Error::Http`.
    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let err = tokio_tungstenite::connect_async(url)
        .await
        .expect_err("a game owned elsewhere must not upgrade");

    let response = match err {
        tungstenite::Error::Http(response) => response,
        other => panic!("expected an HTTP rejection, got {other:?}"),
    };
    // 421 Misdirected Request, with a Location header pointing at the owner.
    assert_eq!(
        response.status(),
        tungstenite::http::StatusCode::MISDIRECTED_REQUEST,
        "a game owned elsewhere must be redirected, not upgraded"
    );
    let location = response
        .headers()
        .get("location")
        .expect("redirect carries a Location header")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(location.starts_with(&owner.address), "got {location}");

    // The JSON body names the owner and a reconnect ws_url that keeps the token.
    let body = response.body().as_ref().expect("redirect carries a body");
    let json: serde_json::Value = serde_json::from_slice(body).expect("body is JSON");
    assert_eq!(json["owner"]["id"], owner.id.to_string(), "got {json}");
    assert_eq!(json["owner"]["address"], owner.address);
    let ws_url = json["ws_url"].as_str().expect("ws_url present");
    assert!(ws_url.starts_with(&owner.address), "got {ws_url}");
    assert!(
        ws_url.contains(&format!("/ws/game/{game_id}")),
        "ws_url keeps the game path; got {ws_url}"
    );
    assert!(
        ws_url.contains(&format!("token={token}")),
        "ws_url preserves the token; got {ws_url}"
    );
}

// ---------------------------------------------------------------------------
// Routing: this node owns the game -> serve locally (upgrade succeeds).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn serves_locally_when_this_node_owns_the_game() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;
    let token = issue_session(app.state.session_config(), white.id).expect("mint token");

    // Pin a two-node membership and make *this* node the rendezvous owner, so the
    // handler serves locally and the handshake upgrades cleanly.
    let (a, b) = two_nodes();
    let nodes = vec![a.clone(), b.clone()];
    let owner = mcs_cluster::owner(&game_id.to_string(), &nodes)
        .expect("two-node set has an owner")
        .clone();

    let registry: Arc<dyn NodeRegistry> = Arc::new(FixedRegistry { nodes });
    let state = app.state.clone().with_cluster(registry, owner);
    let addr = serve(state).await;

    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let (_socket, response) = tokio_tungstenite::connect_async(url)
        .await
        .expect("the owning node serves the socket");
    assert_eq!(
        response.status(),
        tungstenite::http::StatusCode::SWITCHING_PROTOCOLS,
        "the owning node must upgrade, not redirect"
    );
}

// ---------------------------------------------------------------------------
// Single-node default: the local registry never redirects.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_node_default_serves_without_redirect() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;
    let token = issue_session(app.state.session_config(), white.id).expect("mint token");

    // No `with_cluster`: the default single-node LocalRegistry owns every game.
    let addr = serve(app.state.clone()).await;

    let url = format!("ws://{addr}/ws/game/{game_id}?token={token}");
    let (_socket, response) = tokio_tungstenite::connect_async(url)
        .await
        .expect("single-node default serves locally");
    assert_eq!(
        response.status(),
        tungstenite::http::StatusCode::SWITCHING_PROTOCOLS,
        "single-node default must serve locally, never redirect"
    );
}
