use std::{
    ops::{Add, AddAssign},
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

use libc::{self, CLOCK_MONOTONIC};
/// Monotonic clock with fixed size
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FrameworkTime {
    nanoseconds: i64,
}

impl FrameworkTime {
    pub const MAX: FrameworkTime = FrameworkTime::from_nanoseconds(i64::MAX);
    pub const INVALID: FrameworkTime = FrameworkTime::from_nanoseconds(i64::MIN);

    // Convert nanoseconds to an instant
    pub const fn from_nanoseconds(nanoseconds: i64) -> FrameworkTime {
        FrameworkTime { nanoseconds }
    }

    // Get current time via monotonic libc clock
    pub fn from_wall_clock() -> FrameworkTime {
        let mut timespec = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };

        // SAFETY: We check error return on libc call, and pass valid pointer
        let result = unsafe { libc::clock_gettime(CLOCK_MONOTONIC, &mut timespec) };
        if result == 0 {
            // TODO handle wrapping/overflow
            let nanoseconds = timespec.tv_nsec + (timespec.tv_sec * 1_000_000_000);
            FrameworkTime::from_nanoseconds(nanoseconds)
        } else {
            FrameworkTime::INVALID
        }
    }

    pub const fn to_nanoseconds(self) -> i64 {
        self.nanoseconds
    }

    pub fn checked_duration_since(&self, earlier: FrameworkTime) -> Option<Duration> {
        let difference_ns = self.to_nanoseconds() - earlier.to_nanoseconds();
        if difference_ns >= 0 {
            Some(Duration::from_nanos(difference_ns as u64))
        } else {
            None
        }
    }
}

impl Add<std::time::Duration> for FrameworkTime {
    type Output = FrameworkTime;
    fn add(self, rhs: std::time::Duration) -> Self::Output {
        let rhs_nanos = rhs.as_nanos();
        if rhs_nanos > FrameworkTime::MAX.to_nanoseconds() as u128 {
            return FrameworkTime::MAX;
        };
        let sum_nanos = self.to_nanoseconds() + rhs_nanos as i64;
        FrameworkTime::from_nanoseconds(sum_nanos)
    }
}
impl AddAssign<std::time::Duration> for FrameworkTime {
    fn add_assign(&mut self, rhs: std::time::Duration) {
        self.nanoseconds = self.add(rhs).nanoseconds;
    }
}

impl std::fmt::Display for FrameworkTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: split by SECONDS.NANOS w/ fixed width nanos when formatting?
        write!(f, "{}ns", self.to_nanoseconds())
    }
}

/// Atomic variant of [`FrameworkTime`], for sharing a single time value
/// across threads (e.g. a task publishing its next wakeup time to a
/// scheduling thread).
///
/// Convention: [`FrameworkTime::INVALID`] is the "no time" sentinel —
/// `load` returns it when unset, and `store(INVALID)` clears the value.
#[derive(Debug, Default)]
pub struct AtomicFrameworkTime(AtomicI64);

impl AtomicFrameworkTime {
    pub const fn new(time: FrameworkTime) -> Self {
        AtomicFrameworkTime(AtomicI64::new(time.to_nanoseconds()))
    }

    pub const fn from_nanoseconds(nanoseconds: i64) -> Self {
        AtomicFrameworkTime(AtomicI64::new(nanoseconds))
    }

    pub fn load(&self, ordering: Ordering) -> FrameworkTime {
        FrameworkTime::from_nanoseconds(self.0.load(ordering))
    }

    pub fn store(&self, time: FrameworkTime, ordering: Ordering) {
        self.0.store(time.to_nanoseconds(), ordering);
    }

    pub fn swap(&self, time: FrameworkTime, ordering: Ordering) -> FrameworkTime {
        FrameworkTime::from_nanoseconds(self.0.swap(time.to_nanoseconds(), ordering))
    }
}

impl From<FrameworkTime> for AtomicFrameworkTime {
    fn from(time: FrameworkTime) -> Self {
        AtomicFrameworkTime::new(time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TODO test all the add/substract/etc

    #[test]
    fn atomic_framework_time_round_trips() {
        let t = AtomicFrameworkTime::new(FrameworkTime::from_nanoseconds(1234));
        assert_eq!(
            t.load(Ordering::Relaxed),
            FrameworkTime::from_nanoseconds(1234)
        );

        t.store(FrameworkTime::from_nanoseconds(5678), Ordering::Release);
        assert_eq!(
            t.load(Ordering::Acquire),
            FrameworkTime::from_nanoseconds(5678)
        );

        assert_eq!(
            t.swap(FrameworkTime::INVALID, Ordering::AcqRel),
            FrameworkTime::from_nanoseconds(5678)
        );
        assert_eq!(t.load(Ordering::Relaxed), FrameworkTime::INVALID);
    }

    #[test]
    fn atomic_framework_time_from_conversion() {
        let t: AtomicFrameworkTime = FrameworkTime::from_nanoseconds(-42).into();
        assert_eq!(
            t.load(Ordering::Relaxed),
            FrameworkTime::from_nanoseconds(-42)
        );
        assert_eq!(
            t.load(Ordering::Relaxed).to_nanoseconds(),
            AtomicFrameworkTime::from_nanoseconds(-42)
                .load(Ordering::Relaxed)
                .to_nanoseconds()
        );
    }
}
