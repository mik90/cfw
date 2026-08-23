use std::sync::Arc;

use crate::callback_storage::CallbackStorage;
use crate::string_interner::{CallbackNameTag, ChannelNameTag, StringInterner};
use crate::time::FrameworkTime;

#[derive(Debug)]
pub struct ThreadPoolConfig {
    pub thread_count: usize,
    pub nodes: CallbackStorage,
}

impl ThreadPoolConfig {
    /// Wrap either plain built nodes (`Vec<CallbackNode>`) or an existing
    /// [`CallbackStorage`] into a pool config.
    pub fn new(virtual_thread_count: usize, nodes: impl Into<CallbackStorage>) -> Self {
        ThreadPoolConfig {
            thread_count: virtual_thread_count,
            nodes: nodes.into(),
        }
    }
}

/// Parameters to set up any executor
#[derive(Debug)]
pub struct ExecutorParams {
    pools: Vec<ThreadPoolConfig>,
    channel_interner: StringInterner<ChannelNameTag>,
    callback_interner: StringInterner<CallbackNameTag>,
}

impl ExecutorParams {
    pub fn new(pools: Vec<ThreadPoolConfig>) -> Self {
        let mut channel_interner = StringInterner::<ChannelNameTag>::default();
        let mut callback_interner = StringInterner::<CallbackNameTag>::default();
        let mut node_id = 0;
        for pool in pools.iter() {
            for shared_node in pool.nodes.iter_shared() {
                shared_node.access(|node| {
                    node.bind_id(crate::scheduling::CallbackNodeId(node_id));
                    callback_interner.intern(node.name());
                    node.callback().for_each_subscriber(&mut |subscriber| {
                        channel_interner.intern(&subscriber.config().channel_name);
                    });
                    node.callback().for_each_publisher(&mut |publisher| {
                        channel_interner.intern(&publisher.config().channel_name);
                    });
                });
                node_id += 1;
            }
        }
        ExecutorParams {
            pools,
            channel_interner,
            callback_interner,
        }
    }

    pub fn pools(&self) -> &[ThreadPoolConfig] {
        &self.pools
    }

    pub fn into_parts(
        self,
    ) -> (
        Vec<ThreadPoolConfig>,
        StringInterner<ChannelNameTag>,
        StringInterner<CallbackNameTag>,
    ) {
        (self.pools, self.channel_interner, self.callback_interner)
    }
}

/// A non-blocking handle for signaling an executor to stop.
/// Takes `&self` so it can be held behind `Arc` and called from worker threads
/// without requiring a mutable lock on the executor itself.
pub trait ExecutorStopSignal: Send + Sync {
    fn request_stop(&self);
}

/// Source of monotonic time for an executor.
/// The default `WallClock` implementation uses `CLOCK_MONOTONIC`.
/// Replay executors substitute a log-driven time source that advances
/// at a configured multiplier over wall time so all callbacks see the
/// same replayed time stamp.
pub trait TimeSource: Send + Sync {
    fn now(&self) -> FrameworkTime;
}

/// Default wall-clock monotonic time source.
#[derive(Debug)]
pub struct WallClock;

impl TimeSource for WallClock {
    fn now(&self) -> FrameworkTime {
        FrameworkTime::from_wall_clock()
    }
}

pub trait Executor {
    type Error: std::error::Error;

    /// Start the executor. Callback nodes will begin running after this call.
    fn start(&mut self);

    /// Signal shutdown and block until all threads have joined.
    fn stop(&mut self) -> Result<(), Self::Error>;

    /// Return a shareable handle that can signal shutdown without blocking.
    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal>;

    /// Return whether the executor is still running.
    fn is_running(&self) -> bool;
}
