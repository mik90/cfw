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
/// C++ calls [`SpanHandle::take_entry`] / [`SpanHandle::take_entry_uninit`];
/// each call returns an owning output handle that sends or releases its own
/// slot.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpanHandle {
    /// The live view; only valid during the C++ `run` call.
    pub context: *mut c_void,
    /// Takes an uninitialized entry.
    pub take_entry_uninit_fn: unsafe extern "C" fn(*mut c_void) -> SpanEntryOutcome,
    /// Takes an entry initialized from a moved-in payload.
    pub take_entry_fn: unsafe extern "C" fn(*mut c_void, *mut c_void) -> SpanEntryOutcome,
}

/// Result of taking an entry through a span thunk.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpanEntryOutcome {
    /// The reserved entry, or a null handle when no entry was taken.
    pub output: OutputUninit,
    /// Why the entry was or was not taken.
    pub status: SpanEntryStatus,
}

/// Status returned by a span entry thunk.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SpanEntryStatus {
    /// An entry was reserved.
    Taken,
    /// Every entry in the span is already reserved or sent.
    Full,
    /// The publisher arena is smaller than the span's declared capacity.
    ArenaExhausted,
}

// SAFETY: `SpanHandle` is a plain C-layout struct of pointers with no drop
// glue. C++ asserts the same triviality in `output.hpp`.
unsafe impl cxx::ExternType for SpanHandle {
    type Id = cxx::type_id!("cfw::SpanHandle");
    type Kind = cxx::kind::Trivial;
}
