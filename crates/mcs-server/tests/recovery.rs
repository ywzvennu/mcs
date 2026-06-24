//! Server-side recovery-wiring smoke test (#58).
//!
//! Proves that startup recovery is wired into the composition root: building the
//! [`AppState`](mcs_api::AppState) and running [`recover_games`] against an empty
//! in-memory store recovers zero games (and therefore logs a count of `0`)
//! without binding a socket. The full restart-recovery behaviour — replaying a
//! played-out game's durable log back into a live actor — is covered by the
//! `mcs-game` integration test; here we only assert the server boots with the
//! recovery step in place.

use std::sync::Arc;

use mcs_server::{config::Config, recover_games};
use mcs_storage::SqlxStorage;

/// Builds an `AppState` against a fresh in-memory database, exactly as the
/// server's `build_app` does before serving.
async fn empty_state() -> mcs_api::AppState {
    let cfg = Config {
        database_url: "sqlite::memory:".to_owned(),
        ..Config::default()
    };
    let storage = Arc::new(
        SqlxStorage::connect(&cfg.database_url)
            .await
            .expect("connect in-memory sqlite"),
    );
    mcs_server::build_state(&cfg, storage, b"test-secret-bytes-not-for-prod".to_vec())
        .expect("build state")
}

#[tokio::test]
async fn recovery_on_an_empty_store_recovers_zero_games() {
    let state = empty_state().await;

    // No games were ever persisted, so recovery is a no-op: it returns 0 and the
    // hub stays empty.
    let recovered = recover_games(&state)
        .await
        .expect("recovery query succeeds on an empty store");

    assert_eq!(recovered, 0, "an empty store recovers no games");
    assert!(
        state.game_hub().is_empty(),
        "the live-game hub is empty after recovering an empty store",
    );
}
