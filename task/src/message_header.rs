use std::time::Instant;

/// Metadata attached to every message as it passes through the pub/sub system.
/// Set by the executor at flush time — the executor is the sole source of time.
pub struct MessageHeader {
    pub published_at: Instant,
}
