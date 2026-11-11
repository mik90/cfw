//! C++ task bridge for the example tasks.
//!
//! `build.rs` generates the literal `#[cxx::bridge]` module into
//! `gen/bridge.rs` from the `define_cxx_task!` declarations in `tasks.rs`, then
//! cxx compiles it. The generated module and the task declarations share one
//! endpoint spec, so they cannot drift.

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/gen/bridge.rs"));
include!("tasks.rs");
