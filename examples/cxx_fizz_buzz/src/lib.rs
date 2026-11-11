//! C++ task interop spike (M1).
//!
//! The C++ bridge lives behind the `cxx` feature so the default workspace
//! build needs no C++ toolchain. Enable it with `--features cxx`.

#[cfg(feature = "cxx")]
mod bridge;

#[cfg(feature = "cxx")]
pub use bridge::{FIZZ_BUZZ_STRING_CHANNEL, FizzBuzzCalculator, INTEGER_CHANNEL, OptionalProbe};
