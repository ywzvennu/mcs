//! The actor's injectable source of "now".
//!
//! The pure [`ClockEngine`](crate::clock::ClockEngine) never reads the wall
//! clock — every method takes an explicit instant. The *actor*, however, must
//! decide what "now" is when it stamps a move and must be able to sleep until a
//! flag deadline. Those two concerns live behind [`TimeSource`] so production
//! code uses the real wall clock and Tokio timer while tests drive both from a
//! single, controllable virtual clock.
//!
//! A `TimeSource` must keep its [`now`](TimeSource::now) reading and its
//! [`sleep_until`](TimeSource::sleep_until) timer consistent: sleeping until an
//! instant must return only once [`now`](TimeSource::now) has reached it. The
//! production [`SystemTimeSource`] satisfies this by deriving the Tokio sleep
//! duration from the same wall-clock reading it reports.

use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;

/// A source of the current instant and of deadline-based sleeps.
///
/// Implementations are cheap to clone (the actor holds one behind an
/// `Arc`-free, `Send + Sync` boxed trait object) and must be thread-safe.
#[async_trait]
pub trait TimeSource: Send + Sync + std::fmt::Debug {
    /// Returns the current UTC instant.
    fn now(&self) -> OffsetDateTime;

    /// Sleeps until at least `deadline`, returning immediately if it has
    /// already passed.
    async fn sleep_until(&self, deadline: OffsetDateTime);
}

/// The production [`TimeSource`]: real UTC wall clock plus a Tokio timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTimeSource;

#[async_trait]
impl TimeSource for SystemTimeSource {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    async fn sleep_until(&self, deadline: OffsetDateTime) {
        let now = OffsetDateTime::now_utc();
        let remaining: Duration = (deadline - now).try_into().unwrap_or(Duration::ZERO);
        // `tokio::time::sleep(ZERO)` still yields, which is fine and keeps the
        // actor loop cooperative.
        tokio::time::sleep(remaining).await;
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A virtual [`TimeSource`] for deterministic actor tests.
    //!
    //! [`ManualTimeSource`] reports a UTC instant that the test advances by
    //! hand. Its `sleep_until` is built on Tokio's *paused* timer so that a test
    //! using `#[tokio::test(start_paused = true)]` can drive both the virtual
    //! UTC clock and the actor's pending sleeps together: advance the manual
    //! clock to a UTC deadline, then advance Tokio time by the same span to wake
    //! the sleeper.

    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use time::OffsetDateTime;

    use super::TimeSource;

    /// A hand-advanced clock whose `now` and Tokio sleeps stay in lockstep.
    #[derive(Debug)]
    pub(crate) struct ManualTimeSource {
        /// The base UTC instant corresponding to the moment the source was
        /// created; advancing adds to this.
        base: OffsetDateTime,
        /// Total time the clock has been advanced past `base`.
        advanced: Mutex<Duration>,
    }

    impl ManualTimeSource {
        /// Creates a source reading exactly `base`.
        pub(crate) fn new(base: OffsetDateTime) -> Self {
            Self {
                base,
                advanced: Mutex::new(Duration::ZERO),
            }
        }

        /// Advances the virtual UTC clock by `by` and advances Tokio's paused
        /// timer by the same amount, waking any sleeper whose deadline has now
        /// passed. Requires a paused Tokio runtime.
        pub(crate) async fn advance(&self, by: Duration) {
            {
                let mut advanced = self.advanced.lock().unwrap();
                *advanced += by;
            }
            tokio::time::advance(by).await;
        }
    }

    #[async_trait]
    impl TimeSource for ManualTimeSource {
        fn now(&self) -> OffsetDateTime {
            self.base + *self.advanced.lock().unwrap()
        }

        async fn sleep_until(&self, deadline: OffsetDateTime) {
            let now = self.now();
            let remaining: Duration = (deadline - now).try_into().unwrap_or(Duration::ZERO);
            tokio::time::sleep(remaining).await;
        }
    }
}
