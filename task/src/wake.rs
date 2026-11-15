use crate::once::OnceLock;
use alloc::sync::Arc;

/// A lifetime-independent scheduling notification. Implementations must not own
/// borrowed callbacks or arena storage and must support concurrent publications.
pub trait Wake: Send + Sync {
    /// Notify scheduler that something has happened
    fn wake(&self);

    /// Reconsider a previously requested invocation without creating a new one.
    fn readiness_changed(&self);
}

pub type WakeHandle = Arc<dyn Wake>;
// Installed once by an executor before workers start. See `crate::once` for
// why this uses a local publish-once cell instead of `std::sync::OnceLock`.
pub(crate) type WakeRegistration = Arc<OnceLock<WakeHandle>>;
