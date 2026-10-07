//! Standalone build harness for the borrowed task endpoint migration.
//! Uses the task crate's message, time, and experimental endpoint source files.

#[path = "../../src/borrowed/mod.rs"]
mod endpoints;
#[path = "../../src/message.rs"]
pub mod message;
#[path = "../../src/time.rs"]
pub mod time;

pub use endpoints::*;
