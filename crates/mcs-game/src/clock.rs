//! Authoritative, server-side clock engine with flag detection.
//!
//! [`ClockEngine`] is the single source of truth for how much time each player
//! has left in a live game. It is a **pure** state machine: every method that
//! needs the current instant takes an explicit `now: OffsetDateTime` rather than
//! reading the wall clock internally. That makes the time-control mathematics
//! fully deterministic and lets tests drive it with hand-picked timestamps.
//!
//! The engine understands every [`TimeControl`] variant:
//!
//! - [`TimeControl::RealTime`] is the interesting case: each side has a ticking
//!   budget. The side to move spends elapsed wall-clock time; on completing a
//!   move the player has their elapsed deducted and the increment added, then
//!   the opponent's clock starts. A side whose budget reaches zero has
//!   **flagged** and loses on time.
//! - [`TimeControl::Correspondence`] gives the side to move a fixed number of
//!   days per move. Rather than a draining budget this is a per-move *deadline*:
//!   the mover flags only if they let the deadline pass without moving. The
//!   "remaining" reported is the time left until that deadline.
//! - [`TimeControl::Unlimited`] never flags and always reports zero remaining;
//!   it carries no timing state at all.
//!
//! The engine does not decide *when* it is consulted — the game actor is
//! responsible for calling [`ClockEngine::on_move`] after each accepted action
//! and for waking at the flag deadline (see [`ClockEngine::flag_deadline`]) so a
//! player who simply stops moving still loses on time.

use std::time::Duration;

use mcs_core::Color;
use mcs_domain::{Clock, TimeControl};
use time::OffsetDateTime;

/// The number of seconds in a day, used to translate a correspondence
/// `days_per_move` budget into a [`Duration`].
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Per-side ticking state for a [`TimeControl::RealTime`] game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RealTimeState {
    /// Banked time remaining for White, excluding any time elapsing right now.
    white_remaining: Duration,
    /// Banked time remaining for Black, excluding any time elapsing right now.
    black_remaining: Duration,
    /// Time added to the mover's clock after each completed move.
    increment: Duration,
    /// The side whose clock is currently ticking, together with the instant it
    /// began ticking. `None` before [`ClockEngine::start`] and after the game
    /// ends, when no clock is running.
    ticking: Option<(Color, OffsetDateTime)>,
}

/// Per-move deadline state for a [`TimeControl::Correspondence`] game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CorrespondenceState {
    /// How long the side to move has to make each move.
    per_move: Duration,
    /// The side to move and the instant their current move window began.
    /// `None` before the clock starts and after the game ends.
    ticking: Option<(Color, OffsetDateTime)>,
}

/// The internal mode of a [`ClockEngine`], one per [`TimeControl`] variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A draining per-side budget with increment.
    RealTime(RealTimeState),
    /// A per-move deadline measured in days.
    Correspondence(CorrespondenceState),
    /// No timing at all; never flags.
    Unlimited,
}

/// An authoritative server-side clock for one live game.
///
/// Construct one with [`ClockEngine::new`] from the game's [`TimeControl`], call
/// [`start`](ClockEngine::start) once play begins, and call
/// [`on_move`](ClockEngine::on_move) after every accepted move. Query
/// [`remaining`](ClockEngine::remaining) for a live read of either side's clock
/// and [`flagged`](ClockEngine::flagged) to detect a time-out.
///
/// All instant-dependent methods take an explicit `now`, so the engine performs
/// no wall-clock reads of its own and is fully deterministic under test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockEngine {
    mode: Mode,
}

impl ClockEngine {
    /// Builds a clock for `time_control` with both sides at their full budget
    /// and no clock yet ticking. Call [`start`](ClockEngine::start) to begin.
    #[must_use]
    pub fn new(time_control: &TimeControl) -> Self {
        let mode = match *time_control {
            TimeControl::RealTime { initial, increment } => Mode::RealTime(RealTimeState {
                white_remaining: initial,
                black_remaining: initial,
                increment,
                ticking: None,
            }),
            TimeControl::Correspondence { days_per_move } => {
                Mode::Correspondence(CorrespondenceState {
                    per_move: Duration::from_secs(u64::from(days_per_move) * SECONDS_PER_DAY),
                    ticking: None,
                })
            }
            TimeControl::Unlimited => Mode::Unlimited,
        };
        Self { mode }
    }

    /// Returns `true` if this engine tracks a draining real-time budget.
    ///
    /// Correspondence and unlimited games still track a (deadline or absent)
    /// notion of time, but only real-time games have a ticking per-side budget.
    #[must_use]
    pub fn is_real_time(&self) -> bool {
        matches!(self.mode, Mode::RealTime(_))
    }

    /// Begins the clock for the side to move (`to_move`) at instant `now`.
    ///
    /// This records when the first move window opened so that later calls can
    /// measure elapsed time. Calling it more than once simply re-anchors the
    /// running clock to `now` for the current side; for [`TimeControl::Unlimited`]
    /// it is a no-op.
    pub fn start(&mut self, to_move: Color, now: OffsetDateTime) {
        match &mut self.mode {
            Mode::RealTime(state) => state.ticking = Some((to_move, now)),
            Mode::Correspondence(state) => state.ticking = Some((to_move, now)),
            Mode::Unlimited => {}
        }
    }

    /// Records that `mover` completed a move at instant `now` and starts the
    /// opponent's clock.
    ///
    /// For [`TimeControl::RealTime`] this deducts the time that elapsed since
    /// `mover`'s window opened, then adds the increment, then begins the
    /// opponent's window at `now`. A move that overran the budget leaves the
    /// mover at zero remaining (which [`flagged`](ClockEngine::flagged) reports);
    /// the increment is still added but cannot resurrect a flagged clock past
    /// the moment of expiry — flag detection takes precedence.
    ///
    /// For [`TimeControl::Correspondence`] it opens a fresh per-move window for
    /// the opponent. For [`TimeControl::Unlimited`] it does nothing.
    ///
    /// If the clock was never [`start`](ClockEngine::start)ed, or it is not
    /// `mover`'s clock that is ticking, the call still hands the turn to the
    /// opponent but deducts nothing, treating the move as instantaneous.
    pub fn on_move(&mut self, mover: Color, now: OffsetDateTime) {
        match &mut self.mode {
            Mode::RealTime(state) => {
                if let Some((ticking, started)) = state.ticking {
                    if ticking == mover {
                        let elapsed = elapsed_since(started, now);
                        let increment = state.increment;
                        let remaining = state.remaining_mut(mover);
                        *remaining = remaining.saturating_sub(elapsed);
                        // Only credit the increment if the mover did not flag.
                        if !remaining.is_zero() {
                            *remaining = remaining.saturating_add(increment);
                        }
                    }
                }
                state.ticking = Some((mover.opposite(), now));
            }
            Mode::Correspondence(state) => {
                state.ticking = Some((mover.opposite(), now));
            }
            Mode::Unlimited => {}
        }
    }

    /// Returns `player`'s remaining time, live as of `now`.
    ///
    /// For the side whose clock is ticking this subtracts the time elapsed since
    /// their window opened, so a player watching their own clock sees it count
    /// down in real time. For the idle side it returns their banked time. For
    /// [`TimeControl::Correspondence`] it returns the time left until the side
    /// to move's deadline (or zero for the idle side). For
    /// [`TimeControl::Unlimited`] it always returns [`Duration::ZERO`].
    #[must_use]
    pub fn remaining(&self, player: Color, now: OffsetDateTime) -> Duration {
        match &self.mode {
            Mode::RealTime(state) => {
                let banked = state.remaining(player);
                match state.ticking {
                    Some((ticking, started)) if ticking == player => {
                        banked.saturating_sub(elapsed_since(started, now))
                    }
                    _ => banked,
                }
            }
            Mode::Correspondence(state) => match state.ticking {
                Some((ticking, started)) if ticking == player => {
                    state.per_move.saturating_sub(elapsed_since(started, now))
                }
                _ => Duration::ZERO,
            },
            Mode::Unlimited => Duration::ZERO,
        }
    }

    /// Returns the side that has run out of time as of `now`, if any.
    ///
    /// At most one side can be flagged, because only the side to move's clock is
    /// running. Returns `None` for [`TimeControl::Unlimited`], for a clock that
    /// has not started, and whenever the running side still has time left.
    #[must_use]
    pub fn flagged(&self, now: OffsetDateTime) -> Option<Color> {
        let ticking = match &self.mode {
            Mode::RealTime(state) => state.ticking,
            Mode::Correspondence(state) => state.ticking,
            Mode::Unlimited => None,
        }?;
        let (side, _) = ticking;
        if self.remaining(side, now).is_zero() {
            Some(side)
        } else {
            None
        }
    }

    /// Returns the instant at which the side to move will flag, if a clock is
    /// running and bounded.
    ///
    /// The game actor uses this to arm a timer so a player who stops moving
    /// still loses on time. Returns `None` for [`TimeControl::Unlimited`] and
    /// when no clock is ticking; the deadline is always re-validated against the
    /// engine when the timer fires, so a slightly early or late wake is safe.
    #[must_use]
    pub fn flag_deadline(&self) -> Option<OffsetDateTime> {
        match &self.mode {
            Mode::RealTime(state) => state
                .ticking
                .map(|(side, started)| started + state.remaining(side)),
            Mode::Correspondence(state) => {
                state.ticking.map(|(_, started)| started + state.per_move)
            }
            Mode::Unlimited => None,
        }
    }

    /// Captures the engine's current state as a serializable [`Clock`] snapshot
    /// taken at `now`, with both sides' live remaining time and the running
    /// side's window-start timestamp.
    ///
    /// For [`TimeControl::Unlimited`] both remaining times are zero and no turn
    /// timestamp is recorded.
    #[must_use]
    pub fn snapshot(&self, now: OffsetDateTime) -> Clock {
        let turn_started_at = match &self.mode {
            Mode::RealTime(state) => state.ticking.map(|(_, started)| started),
            Mode::Correspondence(state) => state.ticking.map(|(_, started)| started),
            Mode::Unlimited => None,
        };
        Clock::with_times(
            self.remaining(Color::White, now),
            self.remaining(Color::Black, now),
            turn_started_at,
        )
    }
}

impl RealTimeState {
    /// Returns the banked remaining time for `player`.
    fn remaining(&self, player: Color) -> Duration {
        match player {
            Color::White => self.white_remaining,
            Color::Black => self.black_remaining,
        }
    }

    /// Returns a mutable reference to the banked remaining time for `player`.
    fn remaining_mut(&mut self, player: Color) -> &mut Duration {
        match player {
            Color::White => &mut self.white_remaining,
            Color::Black => &mut self.black_remaining,
        }
    }
}

/// Computes the elapsed time from `started` to `now`, clamping any negative or
/// zero span to [`Duration::ZERO`].
///
/// Wall clocks can briefly move backwards (NTP corrections), so a `now` earlier
/// than `started` must never be interpreted as "negative elapsed"; treating it
/// as zero is the safe, monotonic choice for a clock that should only ever drain.
fn elapsed_since(started: OffsetDateTime, now: OffsetDateTime) -> Duration {
    (now - started).try_into().unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed base instant for deterministic arithmetic in the tests below.
    fn base() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn real_time(initial_secs: u64, increment_secs: u64) -> TimeControl {
        TimeControl::RealTime {
            initial: Duration::from_secs(initial_secs),
            increment: Duration::from_secs(increment_secs),
        }
    }

    #[test]
    fn new_real_time_starts_both_sides_at_initial() {
        let engine = ClockEngine::new(&real_time(180, 2));
        assert!(engine.is_real_time());
        // Nothing ticking yet, so both report their full budget regardless of now.
        assert_eq!(
            engine.remaining(Color::White, base()),
            Duration::from_secs(180)
        );
        assert_eq!(
            engine.remaining(Color::Black, base()),
            Duration::from_secs(180)
        );
        assert_eq!(engine.flagged(base()), None);
    }

    #[test]
    fn running_clock_counts_down_live() {
        let mut engine = ClockEngine::new(&real_time(60, 0));
        engine.start(Color::White, base());
        // 10 seconds into White's turn, White shows 50 left, Black untouched.
        let now = base() + Duration::from_secs(10);
        assert_eq!(engine.remaining(Color::White, now), Duration::from_secs(50));
        assert_eq!(engine.remaining(Color::Black, now), Duration::from_secs(60));
    }

    #[test]
    fn on_move_deducts_elapsed_and_adds_increment() {
        let mut engine = ClockEngine::new(&real_time(60, 5));
        engine.start(Color::White, base());

        // White takes 10s then moves: 60 - 10 + 5 = 55 banked.
        let t1 = base() + Duration::from_secs(10);
        engine.on_move(Color::White, t1);
        assert_eq!(engine.remaining(Color::White, t1), Duration::from_secs(55));
        // Now Black is on the move; their clock starts at t1.
        assert_eq!(engine.remaining(Color::Black, t1), Duration::from_secs(60));

        // Black takes 4s then moves: 60 - 4 + 5 = 61 banked.
        let t2 = t1 + Duration::from_secs(4);
        engine.on_move(Color::Black, t2);
        assert_eq!(engine.remaining(Color::Black, t2), Duration::from_secs(61));
        assert_eq!(engine.remaining(Color::White, t2), Duration::from_secs(55));
    }

    #[test]
    fn alternating_moves_accumulate_correctly() {
        let mut engine = ClockEngine::new(&real_time(100, 1));
        engine.start(Color::White, base());

        let mut t = base();
        // White: spends 5, +1 -> 96
        t += Duration::from_secs(5);
        engine.on_move(Color::White, t);
        // Black: spends 7, +1 -> 94
        t += Duration::from_secs(7);
        engine.on_move(Color::Black, t);
        // White: spends 3, +1 -> 94
        t += Duration::from_secs(3);
        engine.on_move(Color::White, t);

        assert_eq!(engine.remaining(Color::White, t), Duration::from_secs(94));
        assert_eq!(engine.remaining(Color::Black, t), Duration::from_secs(94));
    }

    #[test]
    fn flag_detected_at_zero_for_running_side() {
        let mut engine = ClockEngine::new(&real_time(30, 0));
        engine.start(Color::White, base());

        // Up to exactly 30s nobody is flagged...
        let at_limit = base() + Duration::from_secs(30);
        assert_eq!(engine.flagged(at_limit), Some(Color::White));
        // Just before, no flag.
        let before = base() + Duration::from_secs(29);
        assert_eq!(engine.flagged(before), None);
        assert_eq!(
            engine.remaining(Color::White, before),
            Duration::from_secs(1)
        );
        // Well past the limit, still flagged White and clamped at zero.
        let after = base() + Duration::from_secs(45);
        assert_eq!(engine.flagged(after), Some(Color::White));
        assert_eq!(engine.remaining(Color::White, after), Duration::ZERO);
    }

    #[test]
    fn flagging_on_move_does_not_credit_increment() {
        let mut engine = ClockEngine::new(&real_time(30, 5));
        engine.start(Color::White, base());

        // White overran by moving at 40s; remaining clamps to zero, no +5.
        let t = base() + Duration::from_secs(40);
        engine.on_move(Color::White, t);
        assert_eq!(engine.remaining(Color::White, t), Duration::ZERO);
    }

    #[test]
    fn flag_deadline_tracks_running_side() {
        let mut engine = ClockEngine::new(&real_time(60, 0));
        assert_eq!(engine.flag_deadline(), None);

        engine.start(Color::White, base());
        assert_eq!(
            engine.flag_deadline(),
            Some(base() + Duration::from_secs(60))
        );

        // After White moves at 10s with no increment, Black has 60s from t1.
        let t1 = base() + Duration::from_secs(10);
        engine.on_move(Color::White, t1);
        assert_eq!(engine.flag_deadline(), Some(t1 + Duration::from_secs(60)));
    }

    #[test]
    fn idle_side_never_flags() {
        let mut engine = ClockEngine::new(&real_time(1, 0));
        engine.start(Color::White, base());
        // Long after White flags, Black (idle) is never the flagged side.
        let now = base() + Duration::from_secs(1000);
        assert_eq!(engine.flagged(now), Some(Color::White));
    }

    #[test]
    fn unlimited_never_flags_and_reports_zero() {
        let mut engine = ClockEngine::new(&TimeControl::Unlimited);
        assert!(!engine.is_real_time());
        engine.start(Color::White, base());
        let now = base() + Duration::from_secs(1_000_000);
        engine.on_move(Color::White, now);
        assert_eq!(engine.flagged(now), None);
        assert_eq!(engine.flag_deadline(), None);
        assert_eq!(engine.remaining(Color::White, now), Duration::ZERO);
    }

    #[test]
    fn correspondence_uses_per_move_deadline() {
        let mut engine = ClockEngine::new(&TimeControl::Correspondence { days_per_move: 2 });
        assert!(!engine.is_real_time());
        engine.start(Color::White, base());

        let two_days = Duration::from_secs(2 * SECONDS_PER_DAY);
        // One day in, White has one day left and has not flagged.
        let one_day = base() + Duration::from_secs(SECONDS_PER_DAY);
        assert_eq!(
            engine.remaining(Color::White, one_day),
            Duration::from_secs(SECONDS_PER_DAY)
        );
        assert_eq!(engine.flagged(one_day), None);

        // At the deadline, White flags.
        let deadline = base() + two_days;
        assert_eq!(engine.flagged(deadline), Some(Color::White));
        assert_eq!(engine.flag_deadline(), Some(deadline));

        // After White moves, the window resets for Black.
        let moved = base() + Duration::from_secs(SECONDS_PER_DAY);
        engine.on_move(Color::White, moved);
        assert_eq!(
            engine.remaining(Color::Black, moved),
            Duration::from_secs(2 * SECONDS_PER_DAY)
        );
    }

    #[test]
    fn snapshot_captures_live_state() {
        let mut engine = ClockEngine::new(&real_time(60, 0));
        engine.start(Color::White, base());
        let now = base() + Duration::from_secs(10);
        let snap = engine.snapshot(now);
        assert_eq!(snap.white_remaining(), Duration::from_secs(50));
        assert_eq!(snap.black_remaining(), Duration::from_secs(60));
        assert_eq!(snap.turn_started_at(), Some(base()));
    }

    #[test]
    fn backwards_clock_does_not_add_time() {
        let mut engine = ClockEngine::new(&real_time(60, 0));
        engine.start(Color::White, base() + Duration::from_secs(100));
        // `now` earlier than the start instant clamps elapsed to zero.
        let earlier = base();
        assert_eq!(
            engine.remaining(Color::White, earlier),
            Duration::from_secs(60)
        );
    }
}
