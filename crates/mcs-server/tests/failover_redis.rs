//! Automated multi-node failover test over a real Redis (#110) — **never run in
//! the default suite**.
//!
//! This stands up **two nodes inside one process**, sharing the SAME Redis (a
//! [`RedisNodeRegistry`] + [`RedisEventBus`] under a unique key prefix) and the
//! SAME durable database (a single shared [`SqlxStorage`] over a temp SQLite
//! file), each with its own [`AppState`] and distinct node id. It then proves
//! the failover contract end to end:
//!
//! 1. Both nodes register; the game's rendezvous (HRW) owner is computed from
//!    the live two-node set. A game is created and a few moves are played on its
//!    **owner** node's live actor, persisting the action log.
//! 2. The owner node **leaves** (and drops its in-memory handle) — simulating a
//!    crash/eviction. Membership now reports a single survivor, which becomes the
//!    new HRW owner of the same game id.
//! 3. The survivor serves the game by recovering it from the durable action log
//!    via [`AppState::get_or_recover`] — at the correct position, ply, and clock
//!    — and the next move applies and is appended to the shared log.
//!
//! It is `#[ignore]`d and gated on `MCS_TEST_REDIS_URL`, so the default `cargo
//! test` (no Redis) skips it. The CI `redis` job runs it explicitly (see
//! `.github/workflows/ci.yml`), alongside the existing `cluster_redis` test:
//!
//! ```text
//! MCS_TEST_REDIS_URL=redis://127.0.0.1:6379 \
//!   cargo test -p mcs-server --test failover_redis -- --ignored
//! ```

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;

use mcs_api::{AppState, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_cluster::{NodeInfo, NodeRegistry, RedisEventBus, RedisNodeRegistry};
use mcs_core::{Action, Color, VariantOptions, VariantRegistry};
use mcs_domain::{Game, GameId, GameLifecycle, TimeControl, User};
use mcs_storage::{ActionLogRepo, GameRepo, SqlxStorage, UserRepo};
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// A SQLite file under the OS temp dir, removed on drop, used as the **shared
/// durable database** both nodes read and write. A file (not `:memory:`) makes
/// the "shared, durable, action-log-backed recovery" property explicit, and
/// each test gets its own file via a UUID so parallel runs never collide.
struct TempDb {
    path: std::path::PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mcs-failover-{}.db", uuid::Uuid::new_v4()));
        Self { path }
    }

    /// The sqlx connection URL for this file.
    fn url(&self) -> String {
        // `?mode=rwc` creates the file if missing.
        format!("sqlite://{}?mode=rwc", self.path.display())
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        // Best-effort cleanup of the main DB file plus any WAL/SHM siblings.
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}

/// A session/SIWE config pair good enough to build an [`AppState`] in tests.
fn test_configs() -> (SessionConfig, SiweConfig) {
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
    (session, siwe)
}

/// Builds an [`AppState`] for one node: it shares `storage` (the durable DB) and
/// `bus` (the cross-node spectator bus) with its peer, but wires in this node's
/// own cluster identity and membership registry.
fn node_state(
    storage: Arc<SqlxStorage>,
    bus: Arc<RedisEventBus>,
    registry: Arc<dyn NodeRegistry>,
    this_node: NodeInfo,
) -> AppState {
    let mut registry_variants = VariantRegistry::new();
    register(&mut registry_variants);
    let (session, siwe) = test_configs();
    AppState::new(storage, Arc::new(registry_variants), session, siwe)
        .with_event_bus(bus)
        .with_cluster(registry, this_node)
}

/// Persists a fresh user with the given address into the shared store.
async fn create_user(storage: &SqlxStorage, address: &str) -> User {
    let user = User::new(
        address.parse().expect("valid evm address"),
        None,
        OffsetDateTime::now_utc(),
    );
    UserRepo::create(storage, &user).await.expect("create user");
    user
}

/// A UCI move action for the standard variant.
fn uci(mv: &str) -> Action {
    serde_json::from_value(serde_json::json!({ "type": "move", "uci": mv }))
        .expect("valid move action")
}

/// The FEN the handle currently shows from White's view.
async fn fen(handle: &mcs_game::GameHandle) -> String {
    let view = handle.view_for(Color::White).await.expect("view");
    view.as_value()["fen"]
        .as_str()
        .expect("fen present")
        .to_owned()
}

// ---------------------------------------------------------------------------
// The failover scenario
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a live Redis; set MCS_TEST_REDIS_URL to run"]
async fn surviving_node_recovers_game_after_owner_leaves() {
    let Ok(url) = std::env::var("MCS_TEST_REDIS_URL") else {
        eprintln!("MCS_TEST_REDIS_URL unset; skipping");
        return;
    };

    // A unique Redis key prefix per run so concurrent invocations never see each
    // other's membership.
    let prefix = format!("mcs:test:failover:{}:", uuid::Uuid::new_v4());

    // One shared durable database, one shared cross-node event bus.
    let temp_db = TempDb::new();
    let storage = Arc::new(
        SqlxStorage::connect(&temp_db.url())
            .await
            .expect("connect + migrate shared sqlite file"),
    );
    let bus = Arc::new(
        RedisEventBus::connect(&url)
            .await
            .expect("connect event bus")
            .with_prefix(prefix.clone()),
    );

    // Two node identities, each with its own Redis-backed registry handle sharing
    // the one prefix so they see one membership set.
    let node_a = NodeInfo::new("failover-node-a", "http://10.0.0.1:8080");
    let node_b = NodeInfo::new("failover-node-b", "http://10.0.0.2:8080");
    let reg_a: Arc<dyn NodeRegistry> = Arc::new(
        RedisNodeRegistry::connect(&url, node_a.clone(), 30)
            .await
            .expect("connect registry a")
            .with_prefix(prefix.clone()),
    );
    let reg_b: Arc<dyn NodeRegistry> = Arc::new(
        RedisNodeRegistry::connect(&url, node_b.clone(), 30)
            .await
            .expect("connect registry b")
            .with_prefix(prefix.clone()),
    );

    // Both nodes join the cluster.
    reg_a.register().await.expect("register a");
    reg_b.register().await.expect("register b");

    let state_a = node_state(storage.clone(), bus.clone(), reg_a.clone(), node_a.clone());
    let state_b = node_state(storage.clone(), bus.clone(), reg_b.clone(), node_b.clone());

    // Two players in the shared DB, and a durable, already-Active game record.
    let white = create_user(&storage, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&storage, "0x2222222222222222222222222222222222222222").await;

    let time_control = TimeControl::RealTime {
        initial: Duration::from_secs(300),
        increment: Duration::from_secs(2),
    };
    let mut game = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        white.id,
        black.id,
        time_control,
        true,
        OffsetDateTime::now_utc(),
    );
    game.lifecycle = GameLifecycle::Active;
    let game_id: GameId = game.id;
    GameRepo::create(&*storage, &game)
        .await
        .expect("persist game record");

    // Resolve the game's HRW owner from the live two-node set, and pick the
    // surviving (non-owner) node as the failover target.
    let nodes = reg_a.live_nodes().await.expect("live nodes");
    assert_eq!(nodes.len(), 2, "both nodes are live; got {nodes:?}");
    let owner_info = mcs_cluster::owner(&game_id.to_string(), &nodes)
        .expect("the two-node set has an owner")
        .clone();

    let (owner_state, owner_reg, survivor_state, survivor_id) = if owner_info.id == node_a.id {
        (&state_a, &reg_a, &state_b, node_b.id.clone())
    } else {
        (&state_b, &reg_b, &state_a, node_a.id.clone())
    };
    assert_ne!(
        owner_info.id, survivor_id,
        "owner and survivor are different nodes"
    );

    // 1. Play a few moves on the OWNER node's live actor (1. e4 e5 2. Nf3),
    //    recovering it on demand exactly as the WS handler would.
    {
        let handle = owner_state
            .get_or_recover(game_id)
            .await
            .expect("owner recover ok")
            .expect("an Active game yields a handle");
        handle
            .submit_action(Color::White, uci("e2e4"))
            .await
            .expect("1. e4");
        handle
            .submit_action(Color::Black, uci("e7e5"))
            .await
            .expect("1... e5");
        handle
            .submit_action(Color::White, uci("g1f3"))
            .await
            .expect("2. Nf3");
    }

    // The shared log now holds three plies; the durable snapshot advanced.
    let log_before = storage.list(game_id).await.expect("log");
    assert_eq!(log_before.len(), 3, "three plies recorded by the owner");
    let record_before = GameRepo::get(&*storage, game_id).await.expect("record");
    assert_eq!(record_before.ply, 3, "snapshot ply advanced to 3");

    // 2. The owner node dies: it leaves the cluster and drops its in-memory
    //    handle. The durable record and action log remain in the shared DB.
    owner_reg.leave().await.expect("owner leaves the cluster");
    owner_state.game_hub().remove(game_id);

    // Membership now reports only the survivor, which becomes the HRW owner of
    // the same game id.
    let nodes_after = reg_a
        .live_nodes()
        .await
        .expect("live nodes after owner leaves");
    assert_eq!(
        nodes_after.len(),
        1,
        "only the survivor remains live; got {nodes_after:?}"
    );
    assert!(
        mcs_cluster::is_owner(&game_id.to_string(), &survivor_id, &nodes_after),
        "the survivor is now the rendezvous owner of the game"
    );

    // 3. The survivor — which never held this game in memory — serves it by
    //    recovering from the durable action log, at the exact post-2.Nf3
    //    position, and the next move applies and is logged.
    let survivor_handle = survivor_state
        .get_or_recover(game_id)
        .await
        .expect("survivor recover ok")
        .expect("the Active game is revived on the survivor");

    let revived_fen = fen(&survivor_handle).await;
    assert!(
        revived_fen.contains(" b "),
        "after 1. e4 e5 2. Nf3 it is Black to move; got {revived_fen}"
    );
    assert!(
        revived_fen.starts_with("rnbqkbnr/pppp1ppp/8/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R b"),
        "survivor revived to the exact position after 1. e4 e5 2. Nf3; got {revived_fen}"
    );

    // The clock resumed from the persisted remaining time, not from zero.
    let snapshot = survivor_handle.snapshot().await.expect("snapshot");
    assert_eq!(
        snapshot.ply, 3,
        "revived at ply 3, continuing the shared log"
    );
    if let Some(clock) = snapshot.clock {
        assert!(
            clock.white_remaining > Duration::ZERO && clock.black_remaining > Duration::ZERO,
            "both clocks resumed with time remaining; got {clock:?}"
        );
    }

    // The next move continues the game on the survivor and is appended to the
    // shared durable log — proving the failover handed off cleanly.
    survivor_handle
        .submit_action(Color::Black, uci("b8c6"))
        .await
        .expect("2... Nc6 applies on the recovered survivor actor");

    let log_after = storage
        .list(game_id)
        .await
        .expect("log after failover move");
    assert_eq!(
        log_after.len(),
        4,
        "the survivor appended a fourth ply, not a duplicate"
    );
    assert_eq!(log_after[3].ply, 3, "the new move continues at ply 3");
    assert_eq!(log_after[3].player, Color::Black);

    // Clean up the survivor's membership.
    survivor_state
        .cluster()
        .registry()
        .leave()
        .await
        .expect("survivor leaves");
}
