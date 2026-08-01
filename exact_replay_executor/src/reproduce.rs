//! Storage for payloads that were not logged but reproduced during replay.
//!
//! A channel that the logging build step did not log has no ordinary-log
//! payloads, so replay cannot hydrate its consumers from the log. Instead the
//! replay re-runs the producing node; its captured output is stored here keyed
//! by `(channel, published_at_ns)`, and the consuming node's hydration pulls
//! the same payload back out.
//!
//! Producers always execute before consumers in replay order (replay is a
//! single time-ordered pass and a message is published before it is received),
//! so a consumer finds its payload by the time it hydrates. Multiple messages
//! sharing a `(channel, published_at)` are handed out FIFO.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use task::pub_sub::ChannelName;

/// Reproduced payloads for unlogged channels, shared behind a mutex so the
/// store can outlive the single replay thread if replay is ever made
/// multi-threaded.
#[derive(Clone, Default)]
pub(crate) struct ReproducedPayloadStore {
    inner: Arc<StoreMap>,
}

/// `(channel, published_at_ns) -> queue of serialized bodies`.
type StoreMap = Mutex<HashMap<(ChannelName, i64), VecDeque<Vec<u8>>>>;

impl ReproducedPayloadStore {
    pub(crate) fn new() -> Self {
        ReproducedPayloadStore::default()
    }

    /// Record a reproduced payload for the message a node published on
    /// `channel` at `published_at_ns`.
    pub(crate) fn store(&self, channel: ChannelName, published_at_ns: i64, body: Vec<u8>) {
        self.inner
            .lock()
            .expect("reproduction store lock poisoned")
            .entry((channel, published_at_ns))
            .or_default()
            .push_back(body);
    }

    /// Pull the next reproduced payload for a `(channel, published_at)`, or
    /// `None` if the producing node has not been replayed yet.
    pub(crate) fn take(&self, channel: &str, published_at_ns: i64) -> Option<Vec<u8>> {
        let mut map = self.inner.lock().expect("reproduction store lock poisoned");
        let key = (channel.to_owned(), published_at_ns);
        let queue = map.get_mut(&key)?;
        let body = queue.pop_front();
        if queue.is_empty() {
            map.remove(&key);
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_take_round_trips() {
        let store = ReproducedPayloadStore::new();
        store.store("ch".into(), 100, b"a".to_vec());
        store.store("ch".into(), 100, b"b".to_vec());
        store.store("other".into(), 100, b"c".to_vec());

        assert_eq!(store.take("ch", 100), Some(b"a".to_vec()));
        assert_eq!(store.take("ch", 100), Some(b"b".to_vec()));
        assert_eq!(store.take("ch", 100), None, "queue should drain FIFO");
        assert_eq!(store.take("other", 100), Some(b"c".to_vec()));
    }

    #[test]
    fn take_missing_key_is_none() {
        let store = ReproducedPayloadStore::new();
        assert_eq!(store.take("ch", 100), None);
    }
}
