//! Shared C++ task support: framework-owned types exposed to task bridges.
//!
//! Task bridge crates alias [`ffi::Context`] and [`ffi::MessageHeader`] from
//! their own cxx bridge:
//!
//! ```ignore
//! include!("task_cxx/src/lib.rs.h");
//! type Context = ::task_cxx::ffi::Context;
//! type MessageHeader = ::task_cxx::ffi::MessageHeader;
//! ```
//!
//! The whole crate is gated behind the `cxx` feature so default workspace
//! builds need no C++ toolchain.
#![cfg(feature = "cxx")]

use std::pin::Pin;

#[cxx::bridge(namespace = "cfw")]
pub mod ffi {
    /// Executor-provided time for one callback invocation, in nanoseconds.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Context {
        /// Executor time for one callback invocation, in nanoseconds.
        now_nanos: i64,
    }

    /// Metadata attached to every message as it passes through the pub/sub system.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct MessageHeader {
        /// Publication time in nanoseconds.
        published_at: i64,
        /// Declaration-order index within the channel's publisher storage.
        publisher_index: u32,
        /// Send-order index within this publisher's committed batch.
        batch_index: u32,
    }
}

/// Convert the executor context into its C++ view.
pub fn context(context: &task::Context<'_>) -> ffi::Context {
    ffi::Context {
        now_nanos: context.now().to_nanoseconds(),
    }
}

/// Convert a message header into its C++ view.
pub fn message_header(header: &task::message::MessageHeader) -> ffi::MessageHeader {
    ffi::MessageHeader {
        published_at: header.published_at.to_nanoseconds(),
        publisher_index: header.publisher_index,
        batch_index: header.batch_index,
    }
}

/// Exclusive ownership of a C++ task object.
///
/// Executors move callbacks between worker threads but invoke a callback from
/// one thread at a time; a task implementation must not be shared concurrently.
pub struct SendTask<T: cxx::memory::UniquePtrTarget>(cxx::UniquePtr<T>);

impl<T: cxx::memory::UniquePtrTarget> SendTask<T> {
    pub fn new(task: cxx::UniquePtr<T>) -> Self {
        Self(task)
    }

    pub fn pin_mut(&mut self) -> Pin<&mut T> {
        self.0.pin_mut()
    }
}

// SAFETY: see the type-level contract above; `T` is an opaque C++ type whose
// thread-safety cannot be expressed on the Rust side.
unsafe impl<T: cxx::memory::UniquePtrTarget> Send for SendTask<T> {}
