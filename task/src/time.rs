use std::{ops::Add, ops::Sub, time::Duration};

use libc::{self, CLOCK_MONOTONIC};
/// Monotonic clock with fixed size
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
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

        let result = unsafe { libc::clock_gettime(CLOCK_MONOTONIC, &mut timespec) };
        if result == 0 {
            // TODO handle wrapping/overflow
            let nanoseconds = timespec.tv_nsec + (timespec.tv_sec * 1_000_000_000);
            FrameworkTime::from_nanoseconds(nanoseconds)
        } else {
            FrameworkTime::INVALID
        }
    }

    pub fn to_nanoseconds(self) -> i64 {
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

impl std::fmt::Display for FrameworkTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: split by SECONDS.NANOS w/ fixed width nanos when formatting?
        write!(f, "{}ns", self.to_nanoseconds())
    }
}
