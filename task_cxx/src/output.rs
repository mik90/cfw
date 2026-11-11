//! Trivially-relocatable slot descriptor shared with C++ output handles.
//!
//! C++ receives [`OutputUninit`] by value and turns it into an owning
//! `cfw::Output<T>` handle through its `init<T>(...)` member template. The
//! thunks are generated per endpoint by `define_cxx_task!`; they point back at
//! the live view for the duration of one `run` call.

use core::ffi::c_void;

/// Callback invoked by the C++ handle with the view context and slot index.
pub type SlotThunk = unsafe extern "C" fn(*mut c_void, usize);

/// A reserved but uninitialized output slot.
///
/// This type must stay trivially relocatable: cxx returns it by value from the
/// view methods and the C++ side stores it on the stack.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputUninit {
    /// Payload storage for in-place construction, or null when no slot was
    /// available.
    pub payload: *mut c_void,
    /// The live view; only valid during the C++ `run` call.
    pub context: *mut c_void,
    /// Publishes the initialized payload.
    pub send: SlotThunk,
    /// Releases the loan without publishing it.
    pub discard: SlotThunk,
    /// Slot index for span outputs; zero for single outputs.
    pub index: usize,
}

// SAFETY: `OutputUninit` is a plain C-layout struct of pointers and `usize`
// with no drop glue. C++ asserts the same triviality in `output.hpp`.
unsafe impl cxx::ExternType for OutputUninit {
    type Id = cxx::type_id!("cfw::OutputUninit");
    type Kind = cxx::kind::Trivial;
}

/// Trivial proxy for a span publisher, mirroring Rust's `OutputSpan`.
///
/// C++ calls [`SpanHandle::loan`] / [`SpanHandle::loan_uninit`]; each call
/// returns an owning output handle that sends or releases its own slot.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpanHandle {
    /// The live view; only valid during the C++ `run` call.
    pub context: *mut c_void,
    /// Reserves an uninitialized slot.
    pub loan_uninit_fn: unsafe extern "C" fn(*mut c_void) -> OutputUninit,
    /// Reserves a slot initialized from a moved-in payload.
    pub loan_fn: unsafe extern "C" fn(*mut c_void, *mut c_void) -> OutputUninit,
}

// SAFETY: `SpanHandle` is a plain C-layout struct of pointers with no drop
// glue. C++ asserts the same triviality in `output.hpp`.
unsafe impl cxx::ExternType for SpanHandle {
    type Id = cxx::type_id!("cfw::SpanHandle");
    type Kind = cxx::kind::Trivial;
}
