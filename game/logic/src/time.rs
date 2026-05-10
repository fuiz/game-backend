//! Timing primitives for game state.
//!
//! [`Timestamp`] is the single timestamp type the rest of the crate works
//! with. Its in-memory representation depends on the `serializable` Cargo
//! feature and the target, but the API is identical:
//!
//! | Build                                         | Backing                         | Serde                                          |
//! | --------------------------------------------- | ------------------------------- | ---------------------------------------------- |
//! | `serializable` ON                             | `u64` nanos since UNIX epoch    | round-trips as a `u64`                         |
//! | `serializable` OFF, native target             | `std::time::Instant`            | not implemented (compile error to serialize)   |
//! | `serializable` OFF, wasm target               | `web_time::Instant`             | not implemented (compile error to serialize)   |
//!
//! `fuiz-cloudflare` enables `serializable` so `Game` round-trips through
//! Durable Object / KV storage. The native `fuiz-server` keeps its games
//! in-memory and runs without the feature, getting the `Instant` fast path.
//! The benchmark harness inherits default features (no `serializable`) for
//! the same reason. On wasm without `serializable`, `web_time::Instant`
//! bridges to `performance.now()`, since `std::time::Instant::now()` panics
//! on `wasm32-unknown-unknown`.
//!
//! When the feature is off, the `Instant`-backed `Timestamp` does not impl
//! `Serialize` / `Deserialize` at all. Game-state types that hold `Timestamp`
//! gate their serde derives on the same feature, so any attempt to persist
//! `Game` from a non-`serializable` build is a compile error rather than a
//! runtime surprise.

pub use std::time::Duration;

#[cfg(feature = "serializable")]
pub use serializable_backing::Timestamp;

#[cfg(all(not(feature = "serializable"), not(target_family = "wasm")))]
pub use instant_backing::Timestamp;

#[cfg(all(not(feature = "serializable"), target_family = "wasm"))]
pub use web_time_backing::Timestamp;

#[cfg(feature = "serializable")]
mod serializable_backing {
    use serde::{Deserialize, Serialize};

    use super::Duration;

    /// Wall-clock timestamp, nanoseconds since the UNIX epoch.
    ///
    /// Subtraction is a single `u64` saturating sub. The serde shape is the
    /// inner field, so persisted state survives Cloudflare KV / DO / fs
    /// round-trips.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
    #[repr(transparent)]
    pub struct Timestamp {
        nanos_since_epoch: u64,
    }

    impl Timestamp {
        /// Current wall-clock time. Sources `clock_gettime(CLOCK_REALTIME)` on
        /// native and bridges to `Date.now()` on wasm via `web_time`.
        #[inline]
        pub fn now() -> Self {
            #[cfg(target_family = "wasm")]
            let nanos = web_time::SystemTime::now()
                .duration_since(web_time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            #[cfg(not(target_family = "wasm"))]
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            Self {
                nanos_since_epoch: u64::try_from(nanos).unwrap_or(u64::MAX),
            }
        }

        /// `self - earlier`, saturating at zero.
        #[inline]
        pub fn duration_since(self, earlier: Self) -> Duration {
            Duration::from_nanos(self.nanos_since_epoch.saturating_sub(earlier.nanos_since_epoch))
        }

        /// `Self::now().duration_since(self)`.
        #[inline]
        pub fn elapsed(self) -> Duration {
            Self::now().duration_since(self)
        }

        #[cfg(test)]
        pub(super) fn from_nanos_since_epoch(nanos_since_epoch: u64) -> Self {
            Self { nanos_since_epoch }
        }
    }
}

#[cfg(all(not(feature = "serializable"), not(target_family = "wasm")))]
mod instant_backing {
    use std::time::Instant;

    use super::Duration;

    /// Wall-clock-shaped timestamp, backed by `std::time::Instant` for the
    /// cheapest possible subtraction. Deliberately *not* `Serialize` /
    /// `Deserialize` — game-state types that hold a `Timestamp` gate their
    /// serde derives on the `serializable` feature, so any attempt to persist
    /// such a type from a non-`serializable` build is a compile error.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Timestamp(Instant);

    impl Timestamp {
        /// Current monotonic instant.
        #[inline]
        pub fn now() -> Self {
            Self(Instant::now())
        }

        /// `self - earlier`, saturating at zero.
        #[inline]
        pub fn duration_since(self, earlier: Self) -> Duration {
            self.0.saturating_duration_since(earlier.0)
        }

        /// `Self::now().duration_since(self)`.
        #[inline]
        pub fn elapsed(self) -> Duration {
            self.0.elapsed()
        }
    }
}

#[cfg(all(not(feature = "serializable"), target_family = "wasm"))]
mod web_time_backing {
    use web_time::Instant;

    use super::Duration;

    /// Wall-clock-shaped timestamp, backed by `web_time::Instant` (which
    /// bridges to `performance.now()` on `wasm32-unknown-unknown` where
    /// `std::time::Instant::now()` would panic). Deliberately *not*
    /// `Serialize` / `Deserialize` — same compile-time gate as the native
    /// `Instant` backing.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Timestamp(Instant);

    impl Timestamp {
        /// Current monotonic instant via `performance.now()`.
        #[inline]
        pub fn now() -> Self {
            Self(Instant::now())
        }

        /// `self - earlier`, saturating at zero.
        #[inline]
        pub fn duration_since(self, earlier: Self) -> Duration {
            self.0.saturating_duration_since(earlier.0)
        }

        /// `Self::now().duration_since(self)`.
        #[inline]
        pub fn elapsed(self) -> Duration {
            self.0.elapsed()
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn now_is_monotonic_within_process() {
        let a = Timestamp::now();
        let b = Timestamp::now();
        assert!(b >= a);
    }

    #[test]
    fn duration_since_is_zero_for_self() {
        let a = Timestamp::now();
        assert_eq!(a.duration_since(a), Duration::ZERO);
    }

    #[test]
    fn duration_since_saturates_when_earlier_is_later() {
        let later = Timestamp::now();
        // sleep is overkill in a unit test; capture an earlier-by-construction
        // pair via two `now()` reads with the second taken first.
        let earlier_after = Timestamp::now();
        // The interesting check: subtracting a later-stamp from an earlier
        // one is saturating, not panicking.
        assert_eq!(later.duration_since(earlier_after), Duration::ZERO);
    }

    #[cfg(feature = "serializable")]
    #[test]
    fn timestamp_serde_roundtrip() {
        let t = Timestamp::from_nanos_since_epoch(123_456_789);
        let json = serde_json::to_string(&t).expect("serialize");
        let back: Timestamp = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(t, back);
    }
}
