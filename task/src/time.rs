use libc::{self, CLOCK_MONOTONIC};
/// Monotonic clock with fixed size
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    nanoseconds: i64,
}

impl Instant {
    pub const MAX: Instant = Instant::from_nanoseconds(i64::MAX);
    pub const INVALID: Instant = Instant::from_nanoseconds(i64::MIN);

    // Convert nanoseconds to an instant
    pub const fn from_nanoseconds(nanoseconds: i64) -> Instant {
        Instant { nanoseconds }
    }

    // Get current time via monotonic libc clock
    pub fn now() -> Instant {
        let mut timespec = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };

        let result = unsafe { libc::clock_gettime(CLOCK_MONOTONIC, &mut timespec) };
        if result == 0 {
            // TODO handle wrapping/overflow
            let nanoseconds = timespec.tv_nsec + (timespec.tv_sec * 1_000_000_000);
            Instant::from_nanoseconds(nanoseconds)
        } else {
            Instant::INVALID
        }
    }

    pub fn to_nanoseconds(self) -> i64 {
        self.nanoseconds
    }
}

impl std::fmt::Display for Instant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: split by SECONDS.NANOS w/ fixed width nanos when formatting?
        write!(f, "{}ns", self.to_nanoseconds())
    }
}
