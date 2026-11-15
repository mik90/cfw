//! A tiny publish-once cell for executor wakers.
//!
//! `std::sync::OnceLock` is not available in `core`, because a general
//! implementation may need to block while another thread initializes. That is
//! not the contract used here:
//!
//! * Executors install subscriber wakers once, during setup, before workers run.
//! * Publishers only need a lock-free read on the message hot path.
//! * A publisher that observes "not installed yet" is still correct: the
//!   message remains queued, and the executor's initial scheduling/readiness
//!   checks observe it. There is no need to wait for initialization.
//!
//! This cell deliberately has no `get_or_init`, no parking, and no blocking.
//! It uses an atomic pointer-width state word plus inline storage, so it adds
//! no dependency and no setup allocation.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;

/// A single-assignment cell with lock-free reads.
///
/// Publication is release/acquire ordered: a reader that observes `READY` is
/// guaranteed to see the initialized value. A reader racing with the writer
/// observes `None` and must treat that like an uninstalled waker; it must not
/// block or spin waiting for initialization.
pub(crate) struct OnceLock<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: `value` is written only by the unique `set` winner, after winning the
// `EMPTY -> WRITING` CAS. It becomes immutable when `state` is published as
// `READY` with release ordering. Readers only form references after an acquire
// load observes `READY`, and `Drop` has exclusive access to the cell.
unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}

impl<T> OnceLock<T> {
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Publish `value`, returning it unchanged if the cell is already taken.
    ///
    /// `WRITING` is a transient state. A competing `set` loses the CAS and gets
    /// its own value back; it never observes a partially written slot.
    pub(crate) fn set(&self, value: T) -> Result<(), T> {
        if self
            .state
            .compare_exchange(EMPTY, WRITING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(value);
        }

        // SAFETY: this call won the CAS from `EMPTY`, so it is the only writer
        // and no reader can observe `READY` until the release store below.
        unsafe {
            (*self.value.get()).write(value);
        }
        self.state.store(READY, Ordering::Release);
        Ok(())
    }

    /// Read the published value, or `None` if it has not been installed yet.
    pub(crate) fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) != READY {
            return None;
        }

        // SAFETY: `READY` is published with release ordering after the value is
        // written. The value is never mutated or moved after publication, and
        // references cannot outlive `self`.
        Some(unsafe { (*self.value.get()).assume_init_ref() })
    }
}

impl<T> Drop for OnceLock<T> {
    fn drop(&mut self) {
        if self.state.load(Ordering::Acquire) == READY {
            // SAFETY: `Drop` has exclusive access, and `READY` means `value`
            // was initialized by the winning `set`.
            unsafe {
                (*self.value.get()).assume_init_drop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;

    #[test]
    fn set_once_and_read() {
        let cell = OnceLock::new();
        assert_eq!(cell.get(), None);

        cell.set(42_u32).unwrap();
        assert_eq!(cell.get(), Some(&42));
    }

    #[test]
    fn second_set_returns_the_value() {
        let cell = OnceLock::new();
        cell.set(1_u32).unwrap();
        assert_eq!(cell.set(2_u32), Err(2));
        assert_eq!(cell.get(), Some(&1));
    }

    #[test]
    fn drop_runs_only_for_the_published_value() {
        struct Probe<'a>(&'a AtomicUsize);
        impl Drop for Probe<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = AtomicUsize::new(0);
        let cell = OnceLock::new();
        assert!(cell.set(Probe(&drops)).is_ok());
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        assert!(cell.set(Probe(&drops)).is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 1);

        drop(cell);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }
}
