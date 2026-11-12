use std::num::NonZeroUsize;

/// Index into the storage of pools
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PoolIndex(usize);

/// Configuration for a thread pool.
// In the future, we may add thread affinity or other settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadPoolConfig {
    thread_count: NonZeroUsize,
}

impl ThreadPoolConfig {
    pub fn new(thread_count: NonZeroUsize) -> Self {
        Self { thread_count }
    }

    pub fn thread_count(&self) -> NonZeroUsize {
        self.thread_count
    }
}

/// List of thread pools and their thread counts
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadPoolConfigList {
    configs: Vec<ThreadPoolConfig>,
}

impl ThreadPoolConfigList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_pool(&mut self, config: ThreadPoolConfig) {
        self.configs.push(config);
    }

    pub fn with_pool(mut self, config: ThreadPoolConfig) -> Self {
        self.configs.push(config);
        self
    }

    pub fn len(&self) -> usize {
        self.configs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.configs.is_empty()
    }

    pub fn as_slice(&self) -> &[ThreadPoolConfig] {
        &self.configs
    }
}
