use std::sync::{Arc, OnceLock};

/// A lifetime-independent scheduling notification. Implementations must not own
/// borrowed callbacks or arena storage and must support concurrent publications.
pub trait Wake: Send + Sync {
    fn wake(&self);
}

pub type WakeHandle = Arc<dyn Wake>;
pub(crate) type WakeRegistration = Arc<OnceLock<WakeHandle>>;
