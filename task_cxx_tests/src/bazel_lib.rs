//! Bazel-facing re-exports for the generated C++ task crate.

pub use input_span_probe::InputSpanProbe;

pub const INPUT_SPAN_IN_CHANNEL: &str = "input_span_in";
pub const INPUT_SPAN_OUT_CHANNEL: &str = "input_span_out";
