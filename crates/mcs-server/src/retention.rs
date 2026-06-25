//! Periodic retention / GC background task (#107).
//!
//! The retention task runs a single sweep at each configured interval,
//! deleting ephemeral rows whose useful lifetime has passed:
//!
//! - **auth nonces** — expired nonces can never be consumed again; their only
//!   purpose was to prevent replay during the sign-in window.
//! - **revoked tokens** — once a token's own `exp` has passed, JWT verification
//!   rejects it regardless of the denylist; the entry is dead weight.
//! - **stale seeks** — open seeks that have lingered longer than the configured
//!   max age without being matched or cancelled.
//! - **resolved challenges** — declined or canceled challenges older than the
//!   configured max age (accepted challenges are kept for history).
//!
//! The task stops cleanly when the cancellation token it holds is cancelled,
//! which happens during the server's graceful shutdown sequence.

use std::sync::Arc;
use std::time::Duration;

use mcs_storage::{ChallengeRepo, RevokedTokenRepo, SeekRepo, SessionRepo};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::config::RetentionSettings;

/// Counts returned from a single retention sweep.
#[derive(Debug, Default, Clone, Copy)]
pub struct SweepCounts {
    /// Expired auth nonces removed.
    pub nonces: u64,
    /// Expired revoked-token denylist entries removed.
    pub revoked_tokens: u64,
    /// Stale open seeks removed.
    pub seeks: u64,
    /// Old resolved (declined/canceled) challenges removed.
    pub challenges: u64,
}

/// Runs one retention sweep against the supplied repository handles.
///
/// This is extracted from the loop so tests can call it directly without
/// waiting for the interval tick.
///
/// `now` is the reference timestamp for expiry calculations, so callers can
/// inject a deterministic value in tests.
pub async fn run_sweep(
    sessions: &dyn SessionRepo,
    revoked_tokens: &dyn RevokedTokenRepo,
    seeks: &dyn SeekRepo,
    challenges: &dyn ChallengeRepo,
    settings: &RetentionSettings,
    now: OffsetDateTime,
) -> SweepCounts {
    let mut counts = SweepCounts::default();

    // Nonces and revoked tokens are always swept: their max age is the token's
    // own TTL, already baked into the `expires_at` column.
    match sessions.purge_expired_nonces(now).await {
        Ok(n) => counts.nonces = n,
        Err(e) => tracing::warn!(error = %e, "retention: failed to purge expired nonces"),
    }

    match revoked_tokens.purge_expired(now).await {
        Ok(n) => counts.revoked_tokens = n,
        Err(e) => tracing::warn!(
            error = %e,
            "retention: failed to purge expired revoked tokens"
        ),
    }

    // Seek sweep: configurable max age; `0` disables.
    if settings.seek_max_age_secs > 0 {
        let seek_cutoff = now
            - time::Duration::seconds(
                i64::try_from(settings.seek_max_age_secs).unwrap_or(i64::MAX),
            );
        match seeks.purge_stale(seek_cutoff).await {
            Ok(n) => counts.seeks = n,
            Err(e) => tracing::warn!(error = %e, "retention: failed to purge stale seeks"),
        }
    }

    // Challenge sweep: configurable max age; `0` disables.
    if settings.challenge_max_age_secs > 0 {
        let challenge_cutoff = now
            - time::Duration::seconds(
                i64::try_from(settings.challenge_max_age_secs).unwrap_or(i64::MAX),
            );
        match challenges.purge_resolved(challenge_cutoff).await {
            Ok(n) => counts.challenges = n,
            Err(e) => {
                tracing::warn!(error = %e, "retention: failed to purge resolved challenges")
            }
        }
    }

    counts
}

/// Spawns the periodic retention task, returning a [`CancellationToken`] that
/// stops it.
///
/// When [`RetentionSettings::enabled`] is `false` the task is not spawned and
/// the returned token is a no-op placeholder.
///
/// The task wakes every [`interval_secs`](RetentionSettings::interval_secs) and
/// calls [`run_sweep`], logging the counts at `debug` level (with a `info`
/// summary when anything was actually removed).
pub fn spawn_retention_task(
    sessions: Arc<dyn SessionRepo>,
    revoked_tokens: Arc<dyn RevokedTokenRepo>,
    seeks: Arc<dyn SeekRepo>,
    challenges: Arc<dyn ChallengeRepo>,
    settings: RetentionSettings,
    token: CancellationToken,
) {
    if !settings.enabled {
        tracing::info!("retention task disabled (retention.enabled = false)");
        return;
    }

    let interval = Duration::from_secs(settings.interval_secs.max(1));

    tracing::info!(
        interval_secs = settings.interval_secs,
        seek_max_age_secs = settings.seek_max_age_secs,
        challenge_max_age_secs = settings.challenge_max_age_secs,
        "retention task starting",
    );

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip the immediate first tick so the task does not sweep on startup
        // before the server has fully come up.
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let now = OffsetDateTime::now_utc();
                    let counts = run_sweep(
                        sessions.as_ref(),
                        revoked_tokens.as_ref(),
                        seeks.as_ref(),
                        challenges.as_ref(),
                        &settings,
                        now,
                    )
                    .await;

                    let total = counts.nonces
                        + counts.revoked_tokens
                        + counts.seeks
                        + counts.challenges;

                    if total > 0 {
                        tracing::info!(
                            nonces = counts.nonces,
                            revoked_tokens = counts.revoked_tokens,
                            seeks = counts.seeks,
                            challenges = counts.challenges,
                            "retention sweep removed rows",
                        );
                    } else {
                        tracing::debug!("retention sweep: nothing to remove");
                    }
                }
                () = token.cancelled() => {
                    tracing::debug!("retention task stopping (cancellation received)");
                    break;
                }
            }
        }
    });
}
