use std::{
    ops::{Add, AddAssign, Sub, SubAssign},
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

#[cfg(feature = "iceoryx2")]
use iceoryx2::prelude::ZeroCopySend;
use libc::{self, CLOCK_MONOTONIC};
/// Monotonic clock with fixed size
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "iceoryx2", repr(C), derive(iceoryx2::prelude::ZeroCopySend))]
pub struct FrameworkTime {
    nanoseconds: i64,
}

impl FrameworkTime {
    pub const MAX: FrameworkTime = FrameworkTime::from_nanos(i64::MAX);
    pub const MIN: FrameworkTime = FrameworkTime::from_nanos(i64::MIN + 1);
    pub const INVALID: FrameworkTime = FrameworkTime::from_nanos(i64::MIN);

    // Convert nanoseconds to an instant
    pub const fn from_nanos(nanoseconds: i64) -> FrameworkTime {
        FrameworkTime { nanoseconds }
    }

    pub const fn from_micros(microseconds: i64) -> FrameworkTime {
        FrameworkTime::from_nanos(microseconds * 1_000)
    }

    pub const fn from_millis(milliseconds: i64) -> FrameworkTime {
        FrameworkTime::from_nanos(milliseconds * 1_000_000)
    }

    pub const fn from_secs(seconds: i64) -> FrameworkTime {
        FrameworkTime::from_nanos(seconds * 1_000_000_000)
    }

    /// Formats as $SECONDS.$NANOSECONDS, kidna like journalctl's `-o short-monotonic` but more sig figs
    pub fn fmt_as_fractional_sec(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seconds = self.nanoseconds / 1_000_000_000;
        let nanos = (self.nanoseconds % 1_000_000_000).abs();
        write!(f, "{}.{}", seconds, nanos)
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
            FrameworkTime::from_nanos(nanoseconds)
        } else {
            FrameworkTime::INVALID
        }
    }

    pub const fn to_nanoseconds(self) -> i64 {
        self.nanoseconds
    }

    pub fn checked_add_duration(self, duration: Duration) -> Option<Self> {
        let nanos =
            i128::from(self.nanoseconds).checked_add(i128::try_from(duration.as_nanos()).ok()?)?;
        Some(Self::from_nanos(i64::try_from(nanos).ok()?))
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
        // Naively attempt to clamp duration to i64 even though we'd just overflow anyways if we add
        let rhs_nanos = rhs
            .as_nanos()
            .min(FrameworkTime::MAX.to_nanoseconds() as u128);

        let sum_nanos = self.to_nanoseconds() + (rhs_nanos as i64);
        FrameworkTime::from_nanos(sum_nanos)
    }
}

impl AddAssign<std::time::Duration> for FrameworkTime {
    fn add_assign(&mut self, rhs: std::time::Duration) {
        self.nanoseconds = self.add(rhs).nanoseconds;
    }
}

impl Sub<std::time::Duration> for FrameworkTime {
    type Output = FrameworkTime;

    fn sub(self, rhs: std::time::Duration) -> Self::Output {
        // Naively attempt to clamp duration to i64 even though we'd just overflow anyways if we add
        let rhs_nanos = rhs
            .as_nanos()
            .min(FrameworkTime::MAX.to_nanoseconds() as u128);

        let sum_nanos = self.to_nanoseconds() - (rhs_nanos as i64);
        FrameworkTime::from_nanos(sum_nanos)
    }
}

impl SubAssign<std::time::Duration> for FrameworkTime {
    fn sub_assign(&mut self, rhs: std::time::Duration) {
        self.nanoseconds = self.sub(rhs).nanoseconds
    }
}

impl std::fmt::Display for FrameworkTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_as_fractional_sec(f)
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
        FrameworkTime::from_nanos(self.0.load(ordering))
    }

    pub fn store(&self, time: FrameworkTime, ordering: Ordering) {
        self.0.store(time.to_nanoseconds(), ordering);
    }

    pub fn swap(&self, time: FrameworkTime, ordering: Ordering) -> FrameworkTime {
        FrameworkTime::from_nanos(self.0.swap(time.to_nanoseconds(), ordering))
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
    use googletest::prelude::*;

    #[gtest]
    fn time_add() {
        let time = FrameworkTime::from_nanos(1);
        expect_eq!((time + Duration::from_nanos(2)).to_nanoseconds(), 3);
    }

    #[gtest]
    fn time_add_assign() {
        let mut time = FrameworkTime::from_nanos(1);
        time += Duration::from_nanos(2);
        expect_eq!(time.to_nanoseconds(), 3);
    }

    #[gtest]
    fn time_conversions() {
        expect_eq!(FrameworkTime::from_millis(3).to_nanoseconds(), 3_000_000);
        expect_eq!(FrameworkTime::from_micros(3).to_nanoseconds(), 3_000);
        expect_eq!(FrameworkTime::from_secs(3).to_nanoseconds(), 3_000_000_000);
    }

    #[gtest]
    fn time_sub() {
        let time = FrameworkTime::from_nanos(1);
        expect_eq!((time - Duration::from_nanos(2)).to_nanoseconds(), -1);
    }

    #[gtest]
    fn time_sub_assign() {
        let mut time = FrameworkTime::from_nanos(1);
        time -= Duration::from_nanos(2);
        expect_eq!(time.to_nanoseconds(), -1);
    }

    #[gtest]
    fn format_output() {
        expect_eq!(format!("{}", FrameworkTime::from_nanos(1)), "0.1");

        expect_eq!(
            format!("{}", FrameworkTime::from_nanos(1) + Duration::from_secs(1)),
            "1.1"
        );

        expect_eq!(format!("{}", FrameworkTime::MAX), "9223372036.854775807");

        expect_eq!(format!("{}", FrameworkTime::MIN), "-9223372036.854775807");

        expect_eq!(
            format!("{}", FrameworkTime::INVALID),
            "-9223372036.854775808"
        );
    }

    #[gtest]
    fn atomic_framework_time_round_trips() {
        let t = AtomicFrameworkTime::new(FrameworkTime::from_nanos(1234));
        assert_eq!(t.load(Ordering::Relaxed), FrameworkTime::from_nanos(1234));

        t.store(FrameworkTime::from_nanos(5678), Ordering::Release);
        assert_eq!(t.load(Ordering::Acquire), FrameworkTime::from_nanos(5678));

        assert_eq!(
            t.swap(FrameworkTime::INVALID, Ordering::AcqRel),
            FrameworkTime::from_nanos(5678)
        );
        assert_eq!(t.load(Ordering::Relaxed), FrameworkTime::INVALID);
    }

    #[gtest]
    fn atomic_framework_time_from_conversion() {
        let t: AtomicFrameworkTime = FrameworkTime::from_nanos(-42).into();
        assert_eq!(t.load(Ordering::Relaxed), FrameworkTime::from_nanos(-42));
        assert_eq!(
            t.load(Ordering::Relaxed).to_nanoseconds(),
            AtomicFrameworkTime::from_nanoseconds(-42)
                .load(Ordering::Relaxed)
                .to_nanoseconds()
        );
    }
}
