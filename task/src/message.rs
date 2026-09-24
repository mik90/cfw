use crate::time::FrameworkTime;
#[cfg(feature = "iceoryx2")]
use iceoryx2::prelude::ZeroCopySend;
#[cfg(feature = "iceoryx2")]
use std::fmt::Debug;

/// Metadata attached to every message as it passes through the pub/sub system.
/// Set by the executor at flush time — the executor is the sole source of time.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "iceoryx2", repr(C), derive(iceoryx2::prelude::ZeroCopySend))]
pub struct MessageHeader {
    pub published_at: FrameworkTime,
}

impl MessageHeader {
    pub fn new(published_at: FrameworkTime) -> Self {
        MessageHeader { published_at }
    }
}

impl Default for MessageHeader {
    fn default() -> Self {
        MessageHeader {
            published_at: FrameworkTime::INVALID,
        }
    }
}

/// Contiguous message struct for payload and header.
/// Meant for allocation in arenas
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "iceoryx2", repr(C))]
pub struct Message<T> {
    pub header: MessageHeader,
    pub message: T,
}

#[cfg(feature = "iceoryx2")]
// SAFETY: `Message<T>` is `#[repr(C)]` under this feature; its only fields are
// `MessageHeader` (`repr(C)`, `ZeroCopySend`) and `T` (`ZeroCopySend` via the
// bound), so the layout is uniform and self-contained exactly when `T` is.
unsafe impl<T: iceoryx2::prelude::ZeroCopySend + Debug> iceoryx2::prelude::ZeroCopySend
    for Message<T>
{
}

impl<T: Clone> Clone for Message<T> {
    fn clone(&self) -> Self {
        Self {
            header: self.header,
            message: self.message.clone(),
        }
    }
}

/// Default constructible T means Message is default constructible
impl<T: Default> Default for Message<T> {
    fn default() -> Self {
        Self {
            header: MessageHeader::default(),
            message: T::default(),
        }
    }
}

#[cfg(all(test, feature = "iceoryx2"))]
mod iox2_layout_tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[repr(C)]
    #[repr(align(16))]
    #[derive(Debug, iceoryx2::prelude::ZeroCopySend)]
    struct Align16 {
        data: [u8; 16],
    }

    fn assert_zero_copy<T: iceoryx2::prelude::ZeroCopySend>() {}

    #[test]
    fn wire_layout_is_stable() {
        assert_eq!(size_of::<FrameworkTime>(), 8);
        assert_eq!(align_of::<FrameworkTime>(), 8);
        assert_eq!(size_of::<MessageHeader>(), 8);
        assert_eq!(offset_of!(MessageHeader, published_at), 0);
        assert_eq!(offset_of!(Message<u64>, message), 8);
        assert_eq!(size_of::<Message<u64>>(), 16);
        // Align16 requires padding between the 8-byte header and payload.
        assert_eq!(offset_of!(Message<Align16>, message), 16);
        assert_eq!(size_of::<Message<Align16>>(), 32);
        assert_zero_copy::<Message<Align16>>();
    }
}
