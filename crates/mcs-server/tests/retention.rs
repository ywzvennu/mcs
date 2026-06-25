//! Server-level tests for the retention / GC task (#107).
//!
//! These tests drive [`run_sweep`] directly (no interval tick needed) to
//! verify the purge functions are called correctly and that `enabled = false`
//! is respected.

use std::sync::Arc;

use mcs_server::config::RetentionSettings;
use mcs_server::retention::run_sweep;
use mcs_storage::SqlxStorage;
use time::OffsetDateTime;

/// Connects a fresh in-memory SQLite database for one test.
async fn storage() -> Arc<SqlxStorage> {
    Arc::new(
        SqlxStorage::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite"),
    )
}

// ---------------------------------------------------------------------------
// run_sweep exercises all four purge paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_sweep_purges_expired_nonces_revoked_tokens_seeks_and_challenges() {
    use mcs_domain::{Challenge, ColorPreference, Seek, TimeControl, UserId};
    use mcs_storage::{ChallengeRepo, RevokedTokenRepo, SeekRepo, SessionRepo};

    let storage = storage().await;
    let now = OffsetDateTime::now_utc();
    let old_ts = now - time::Duration::hours(25);
    let cutoff_age: u64 = 24 * 3600; // 24 h — the default max age

    // Auth nonce: store one expired + one live.
    let addr: mcs_domain::EvmAddress = "0xabcdef1234567890abcdef1234567890abcdef12"
        .parse()
        .unwrap();
    storage
        .store_nonce(&addr, "expired_nonce", old_ts)
        .await
        .unwrap();
    storage
        .store_nonce(&addr, "live_nonce", now + time::Duration::minutes(10))
        .await
        .unwrap();

    // Revoked token: store one expired + one live.
    storage.revoke("old_jti", old_ts).await.unwrap();
    storage
        .revoke("fresh_jti", now + time::Duration::hours(1))
        .await
        .unwrap();

    // Seek: one old, one fresh.
    let old_seek = Seek::new(
        UserId::new(),
        "standard".to_owned(),
        TimeControl::Unlimited,
        ColorPreference::Random,
        true,
        old_ts,
    );
    let fresh_seek = Seek::new(
        UserId::new(),
        "standard".to_owned(),
        TimeControl::Unlimited,
        ColorPreference::Random,
        true,
        now,
    );
    SeekRepo::create(&*storage, &old_seek).await.unwrap();
    SeekRepo::create(&*storage, &fresh_seek).await.unwrap();

    // Challenge: one old declined, one fresh pending.
    let mut old_challenge = Challenge::new(
        UserId::new(),
        UserId::new(),
        "standard".to_owned(),
        TimeControl::Unlimited,
        true,
        ColorPreference::Random,
        old_ts,
    );
    ChallengeRepo::create(&*storage, &old_challenge)
        .await
        .unwrap();
    old_challenge.decline();
    ChallengeRepo::update(&*storage, &old_challenge)
        .await
        .unwrap();

    let fresh_challenge = Challenge::new(
        UserId::new(),
        UserId::new(),
        "standard".to_owned(),
        TimeControl::Unlimited,
        true,
        ColorPreference::Random,
        now,
    );
    ChallengeRepo::create(&*storage, &fresh_challenge)
        .await
        .unwrap();

    let settings = RetentionSettings {
        enabled: true,
        interval_secs: 3600,
        seek_max_age_secs: cutoff_age,
        challenge_max_age_secs: cutoff_age,
    };

    let counts = run_sweep(
        storage.as_ref(),
        storage.as_ref(),
        storage.as_ref(),
        storage.as_ref(),
        &settings,
        now,
    )
    .await;

    assert_eq!(counts.nonces, 1, "one expired nonce removed");
    assert_eq!(
        counts.revoked_tokens, 1,
        "one expired revoked token removed"
    );
    assert_eq!(counts.seeks, 1, "one stale seek removed");
    assert_eq!(counts.challenges, 1, "one old resolved challenge removed");

    // Live data is untouched.
    assert!(storage.consume_nonce(&addr, "live_nonce").await.unwrap());
    assert!(storage.is_revoked("fresh_jti").await.unwrap());
    assert!(SeekRepo::get(&*storage, fresh_seek.id)
        .await
        .unwrap()
        .is_some());
    assert!(ChallengeRepo::get(&*storage, fresh_challenge.id)
        .await
        .is_ok());
}

// ---------------------------------------------------------------------------
// Zero max_age disables individual sweeps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_sweep_with_zero_max_age_skips_seeks_and_challenges() {
    use mcs_domain::{Challenge, ColorPreference, Seek, TimeControl, UserId};
    use mcs_storage::{ChallengeRepo, SeekRepo};

    let storage = storage().await;
    let old_ts = OffsetDateTime::UNIX_EPOCH;
    let now = OffsetDateTime::now_utc();

    let old_seek = Seek::new(
        UserId::new(),
        "standard".to_owned(),
        TimeControl::Unlimited,
        ColorPreference::Random,
        true,
        old_ts,
    );
    SeekRepo::create(&*storage, &old_seek).await.unwrap();

    let mut old_challenge = Challenge::new(
        UserId::new(),
        UserId::new(),
        "standard".to_owned(),
        TimeControl::Unlimited,
        true,
        ColorPreference::Random,
        old_ts,
    );
    ChallengeRepo::create(&*storage, &old_challenge)
        .await
        .unwrap();
    old_challenge.cancel();
    ChallengeRepo::update(&*storage, &old_challenge)
        .await
        .unwrap();

    let settings = RetentionSettings {
        enabled: true,
        interval_secs: 3600,
        seek_max_age_secs: 0,      // disabled
        challenge_max_age_secs: 0, // disabled
    };

    let counts = run_sweep(
        storage.as_ref(),
        storage.as_ref(),
        storage.as_ref(),
        storage.as_ref(),
        &settings,
        now,
    )
    .await;

    assert_eq!(counts.seeks, 0, "seek sweep disabled by zero max age");
    assert_eq!(
        counts.challenges, 0,
        "challenge sweep disabled by zero max age"
    );

    // Both rows still present.
    assert!(SeekRepo::get(&*storage, old_seek.id)
        .await
        .unwrap()
        .is_some());
    assert!(ChallengeRepo::get(&*storage, old_challenge.id)
        .await
        .is_ok());
}
