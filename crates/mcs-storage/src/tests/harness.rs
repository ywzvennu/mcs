//! Backend-parameterised test harness for the [`SqlxStorage`] integration suite.
//!
//! The same test bodies run against two backends:
//!
//! * **SQLite** (the default) — every test connects to a private
//!   `"sqlite::memory:"` database. In-memory SQLite gives each pool its own
//!   database that is torn down when the pool drops, so tests are already fully
//!   isolated from one another with no extra bookkeeping.
//!
//! * **Postgres** (the `postgres` feature, exercised in CI against a real
//!   service) — every test gets its own **uniquely-named schema** on the shared
//!   database. The pool is configured so every connection runs
//!   `SET search_path TO "<schema>"` on checkout, so the migrations create the
//!   tables inside that private schema and every query is scoped to it. The
//!   schema name embeds a fresh UUID, so concurrent tests can never collide even
//!   on a shared server. The schema is dropped when the harness value is
//!   dropped; per-test uniqueness means correctness never depends on that
//!   teardown succeeding.
//!
//! Tests obtain a backend by calling [`connect_test_storage`] and then use the
//! returned [`TestStorage`] exactly like a [`SqlxStorage`] — it derefs to the
//! inner storage, so `storage.users()`, `storage.games()`, … all work
//! unchanged.

use std::ops::Deref;

use crate::SqlxStorage;

/// Reads the test database URL, defaulting to private in-memory SQLite.
///
/// CI sets `MCS_TEST_DATABASE_URL` (e.g. `postgres://…@localhost/mcs_test`) for
/// the Postgres job; locally and on the default SQLite job the variable is
/// unset and the suite runs against `"sqlite::memory:"`.
fn test_database_url() -> String {
    std::env::var("MCS_TEST_DATABASE_URL").unwrap_or_else(|_| "sqlite::memory:".to_owned())
}

/// A connected [`SqlxStorage`] for a single test, plus any per-test isolation
/// state (a private Postgres schema) that is cleaned up on drop.
///
/// Derefs to the inner [`SqlxStorage`], so test bodies call `storage.users()`,
/// `storage.games()`, … directly.
// The active sqlx backend is selected exactly as in `sqlx_store.rs`: `sqlite`
// wins when present (so the all-features build is a SQLite build), and Postgres
// is the backend only when `postgres` is on *and* `sqlite` is off. The harness
// gates mirror that, so the schema machinery is compiled only for a genuine
// Postgres backend.
pub(crate) struct TestStorage {
    storage: SqlxStorage,
    /// The private Postgres schema to drop on teardown, if any. `None` for the
    /// SQLite backend (the in-memory database is discarded with the pool).
    #[cfg(all(feature = "postgres", not(feature = "sqlite")))]
    schema: Option<String>,
}

impl Deref for TestStorage {
    type Target = SqlxStorage;

    fn deref(&self) -> &Self::Target {
        &self.storage
    }
}

/// Connects a fresh, isolated [`TestStorage`] for one test, with migrations
/// applied.
///
/// On SQLite this is a private in-memory database; on Postgres it is a private,
/// uniquely-named schema on the shared server. Either way the returned handle is
/// hermetic: no test observes another test's rows.
#[cfg(feature = "sqlite")]
pub(crate) async fn connect_test_storage() -> TestStorage {
    let storage = SqlxStorage::connect(&test_database_url())
        .await
        .expect("connect + migrate test database");
    TestStorage { storage }
}

/// Connects a fresh, isolated [`TestStorage`] for one test, with migrations
/// applied. See the [`sqlite`](connect_test_storage) variant for the SQLite
/// path; this one provisions a private Postgres schema.
#[cfg(all(feature = "postgres", not(feature = "sqlite")))]
pub(crate) async fn connect_test_storage() -> TestStorage {
    use sqlx::Executor;

    let url = test_database_url();
    assert!(
        url.starts_with("postgres"),
        "the Postgres backend needs MCS_TEST_DATABASE_URL set to a postgres:// URL \
         (got {url:?}); the CI postgres job provides one",
    );

    // One private schema per test. The UUID guarantees uniqueness across
    // concurrent tests sharing the server; the `t_` prefix keeps it a valid
    // unquoted-friendly identifier and easy to spot.
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());

    // Create the schema up front using a throwaway connection on the default
    // search_path.
    let admin = sqlx::PgPool::connect(&url)
        .await
        .expect("connect admin pool to create test schema");
    admin
        .execute(format!("CREATE SCHEMA \"{schema}\"").as_str())
        .await
        .expect("create per-test schema");
    admin.close().await;

    // Build the working pool: every connection pins its search_path to the
    // private schema, so migrations create tables there and all queries are
    // scoped to it without touching any other test's data.
    let pin = schema.clone();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .after_connect(move |conn, _meta| {
            let pin = pin.clone();
            Box::pin(async move {
                conn.execute(format!("SET search_path TO \"{pin}\"").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("connect scoped pool for test schema");

    let storage = SqlxStorage::from_pool(pool)
        .await
        .expect("run migrations in test schema");

    TestStorage {
        storage,
        schema: Some(schema),
    }
}

#[cfg(all(feature = "postgres", not(feature = "sqlite")))]
impl Drop for TestStorage {
    fn drop(&mut self) {
        let Some(schema) = self.schema.take() else {
            return;
        };
        // Best-effort teardown: drop the private schema and everything in it.
        // Uniqueness already guarantees isolation, so a failed drop only leaves
        // an inert schema behind on an ephemeral CI server; it never affects
        // another test's correctness.
        //
        // We do the drop *synchronously* on a fresh, dedicated thread that owns
        // its own single-thread runtime. Each `#[tokio::test]` runs on its own
        // current-thread runtime that is torn down the instant the test
        // function returns, so a task spawned onto it would be cancelled before
        // it could connect; running the drop on a separate thread and joining it
        // keeps the cleanup reliable without nesting runtimes.
        let url = test_database_url();
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                use sqlx::Executor;
                if let Ok(pool) = sqlx::PgPool::connect(&url).await {
                    let _ = pool
                        .execute(format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE").as_str())
                        .await;
                    pool.close().await;
                }
            });
        })
        .join();
    }
}
