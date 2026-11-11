//! C++ task interop spike (M1).
//!
//! The C++ bridge lives behind the `cxx` feature so the default workspace
//! build needs no C++ toolchain. Enable it with `--features cxx`.

#[cfg(feature = "cxx")]
mod bridge;

#[cfg(feature = "cxx")]
pub use bridge::{
    FIZZ_BUZZ_STRING_CHANNEL, FizzBuzzCalculator, INTEGER_CHANNEL, NORM_IN_CHANNEL,
    NORM_OUT_CHANNEL, NormCalculator, OPTIONAL_PROBE_IN_CHANNEL, OPTIONAL_PROBE_OUT_CHANNEL,
    OptionalProbe,
};
