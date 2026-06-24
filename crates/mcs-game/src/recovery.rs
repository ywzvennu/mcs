//! Rebuilding a live game actor from the durable action log.
//!
//! When the server restarts, every game that was still in progress lives only
//! in storage: a [`Game`] record carrying the live snapshot (ply, clocks, side
//! to move) plus the append-only [`ActionLogRepo`] holding every move played so
//! far. [`recover_game`] turns that durable state back into a running
//! [`GameActor`](crate::GameActor):
//!
//! 1. it instantiates a fresh [`GameSession`] of the game's variant through the
//!    [`VariantRegistry`];
//! 2. it replays every [`RecordedAction`], in `ply` order, back through
//!    [`GameSession::apply`], driving the session to the exact position the log
//!    describes;
//! 3. it spawns a **resumed** actor (see
//!    [`GameActor::spawn_resumed`](crate::GameActor::spawn_resumed)) seeded with
//!    the game's [`ply`](Game::ply) — so the next recorded move continues at the
//!    right index — and each side's persisted remaining clock, so play picks up
//!    where it left off.
//!
//! The server calls this once per unfinished game at startup
//! ([`GameRepo::list_unfinished`](mcs_storage::GameRepo::list_unfinished)),
//! inserting each returned [`GameHandle`](crate::GameHandle) into its live-game
//! hub. A single game that fails to recover is logged and skipped; recovery of
//! one game never aborts the others.

use std::sync::Arc;

use mcs_core::VariantRegistry;
use mcs_domain::Game;
use mcs_storage::{ActionLogRepo, GameRepo};
use thiserror::Error;

use crate::actor::{ClockRemaining, GameActor, GameHandle};
use crate::completion::GameCompletionHook;

/// Why a single in-progress game could not be rebuilt from its durable log.
///
/// Each variant points at a distinct failure during recovery, so the server can
/// log a precise cause for the one game it skips while continuing to recover the
/// rest.
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// The game's variant could not be instantiated from the registry — either
    /// no variant is registered under [`Game::variant_id`], or the variant
    /// rejected the stored [`variant_options`](Game::variant_options).
    ///
    /// This usually means the build no longer ships the variant the game was
    /// created with.
    #[error("cannot create a session for variant {variant_id:?}: {source}")]
    Session {
        /// The variant id that could not be instantiated.
        variant_id: String,
        /// The underlying registry/factory error.
        source: mcs_core::GameError,
    },

    /// Reading the action log for the game failed.
    #[error("cannot read the action log: {0}")]
    Log(#[source] mcs_storage::StorageError),

    /// Replaying a recorded action diverged from the durable log: the action the
    /// log says was legal at this point was rejected by a freshly built session.
    ///
    /// This indicates the persisted history is inconsistent with the current
    /// variant implementation (a corrupt log, or a variant whose rules changed),
    /// so the game cannot be safely resumed.
    #[error(
        "replay diverged at ply {ply}: the recorded action was rejected by the rebuilt session: {source}"
    )]
    Replay {
        /// The `ply` at which replay failed.
        ply: u32,
        /// The rule error the rebuilt session returned for the recorded action.
        source: mcs_core::GameError,
    },
}

/// Rebuilds a live [`GameActor`](crate::GameActor) for one in-progress `game`
/// from its durable action log and returns a handle to it.
///
/// The session is recreated through `registry`, every action in `action_log` is
/// replayed in `ply` order to reach the current position, and a resumed actor
/// is spawned seeded with the game's [`ply`](Game::ply) and persisted clocks
/// (so downtime is not charged to either player — see
/// [`GameActor::spawn_resumed`](crate::GameActor::spawn_resumed)).
///
/// `repo` and `hook` are handed to the spawned actor exactly as a freshly
/// created game would receive them: the actor refreshes the live snapshot and
/// persists the final result through `repo`, and runs `hook` once on game end.
///
/// # Errors
///
/// - [`RecoveryError::Session`] if the variant cannot be instantiated;
/// - [`RecoveryError::Log`] if the action log cannot be read;
/// - [`RecoveryError::Replay`] if a recorded action is rejected on replay,
///   meaning the durable history diverges from the rebuilt session.
pub async fn recover_game(
    game: &Game,
    registry: &VariantRegistry,
    action_log: Arc<dyn ActionLogRepo>,
    repo: Arc<dyn GameRepo>,
    hook: Arc<dyn GameCompletionHook>,
) -> Result<GameHandle, RecoveryError> {
    // 1. Recreate a fresh session of the game's variant, configured exactly as
    //    it was first created.
    let mut session = registry
        .new_game(&game.variant_id, &game.variant_options)
        .map_err(|source| RecoveryError::Session {
            variant_id: game.variant_id.clone(),
            source,
        })?;

    // 2. Replay the durable history, oldest move first, to drive the session to
    //    the current position. The log is the authoritative record; a rejected
    //    replay means the stored history no longer matches the variant's rules.
    let recorded = action_log.list(game.id).await.map_err(RecoveryError::Log)?;
    for action in &recorded {
        session
            .apply(action.player, &action.action)
            .map_err(|source| RecoveryError::Replay {
                ply: action.ply,
                source,
            })?;
    }

    // 3. Spawn a resumed actor that continues recording at the game's ply and
    //    resumes each side's clock from its persisted remaining time. The
    //    persisted snapshot (game.ply) is one past the last recorded ply.
    let clock_remaining = resume_clock(game);
    let handle = GameActor::spawn_resumed(
        game.id,
        session,
        repo,
        action_log,
        hook,
        game.time_control.clone(),
        game.ply,
        clock_remaining,
    );

    Ok(handle)
}

/// Determines each side's remaining time to resume a recovered `game` with.
///
/// For a real-time game that has recorded a live snapshot, this is exactly the
/// persisted [`clock_white_ms`](Game::clock_white_ms) /
/// [`clock_black_ms`](Game::clock_black_ms). A timed game recovered *before its
/// first move* has no snapshot yet (both fields are `None`); in that case each
/// side resumes at the time control's full initial budget rather than zero,
/// which would otherwise flag the side to move the instant it is recovered.
///
/// For untimed games the values are ignored downstream (see
/// [`ClockEngine::from_remaining`](crate::ClockEngine::from_remaining)), so the
/// fallback is harmless.
fn resume_clock(game: &Game) -> ClockRemaining {
    match (game.clock_white_ms, game.clock_black_ms) {
        // A snapshot exists: resume from exactly what was persisted.
        (Some(_), _) | (_, Some(_)) => {
            ClockRemaining::from_millis(game.clock_white_ms, game.clock_black_ms)
        }
        // No snapshot yet (timed game, not yet played): start at full budget.
        (None, None) => match game.time_control {
            mcs_domain::TimeControl::RealTime { initial, .. } => ClockRemaining {
                white: initial,
                black: initial,
            },
            // Correspondence/unlimited ignore these values entirely.
            _ => ClockRemaining::from_millis(None, None),
        },
    }
}
