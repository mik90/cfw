use std::num::{NonZeroUsize, TryFromIntError};

/// Index into the storage of pools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PoolIndex(usize);

impl PoolIndex {
    pub const fn new(index: usize) -> Self {
        Self(index)
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl std::fmt::Display for PoolIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Configuration for a thread pool.
// In the future, we may add thread affinity or other settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadPoolConfig {
    thread_count: NonZeroUsize,
}

impl ThreadPoolConfig {
    pub const fn new(thread_count: NonZeroUsize) -> Self {
        Self { thread_count }
    }

    pub const fn thread_count(&self) -> NonZeroUsize {
        self.thread_count
    }
}

impl TryFrom<usize> for ThreadPoolConfig {
    type Error = TryFromIntError;

    fn try_from(thread_count: usize) -> Result<Self, Self::Error> {
        NonZeroUsize::try_from(thread_count).map(Self::new)
    }
}

impl From<NonZeroUsize> for ThreadPoolConfig {
    fn from(thread_count: NonZeroUsize) -> Self {
        Self::new(thread_count)
    }
}

/// List of thread pools and their thread counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadPoolConfigList {
    configs: Vec<ThreadPoolConfig>,
}

impl ThreadPoolConfigList {
    pub const fn new() -> Self {
        Self {
            configs: Vec::new(),
        }
    }

    /// Add a pool and return the index that identifies it.
    pub fn add_pool(&mut self, config: ThreadPoolConfig) -> PoolIndex {
        let index = PoolIndex::new(self.configs.len());
        self.configs.push(config);
        index
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

    pub fn get(&self, pool: PoolIndex) -> Option<&ThreadPoolConfig> {
        self.configs.get(pool.as_usize())
    }

    pub fn contains(&self, pool: PoolIndex) -> bool {
        pool.as_usize() < self.len()
    }

    pub fn iter_indexed(&self) -> impl Iterator<Item = (PoolIndex, &ThreadPoolConfig)> + '_ {
        self.configs
            .iter()
            .enumerate()
            .map(|(index, config)| (PoolIndex::new(index), config))
    }
}

impl From<ThreadPoolConfig> for ThreadPoolConfigList {
    fn from(config: ThreadPoolConfig) -> Self {
        Self {
            configs: vec![config],
        }
    }
}

impl FromIterator<ThreadPoolConfig> for ThreadPoolConfigList {
    fn from_iter<T: IntoIterator<Item = ThreadPoolConfig>>(iter: T) -> Self {
        Self {
            configs: iter.into_iter().collect(),
        }
    }
}

impl<const N: usize> From<[ThreadPoolConfig; N]> for ThreadPoolConfigList {
    fn from(configs: [ThreadPoolConfig; N]) -> Self {
        Self {
            configs: configs.into(),
        }
    }
}

impl TryFrom<Vec<usize>> for ThreadPoolConfigList {
    type Error = TryFromIntError;

    fn try_from(thread_counts: Vec<usize>) -> Result<Self, Self::Error> {
        thread_counts
            .into_iter()
            .map(ThreadPoolConfig::try_from)
            .collect()
    }
}

impl<const N: usize> TryFrom<[usize; N]> for ThreadPoolConfigList {
    type Error = TryFromIntError;

    fn try_from(thread_counts: [usize; N]) -> Result<Self, Self::Error> {
        Self::try_from(thread_counts.to_vec())
    }
}

impl TryFrom<&[usize]> for ThreadPoolConfigList {
    type Error = TryFromIntError;

    fn try_from(thread_counts: &[usize]) -> Result<Self, Self::Error> {
        thread_counts
            .iter()
            .copied()
            .map(ThreadPoolConfig::try_from)
            .collect()
    }
}

impl<'a> IntoIterator for &'a ThreadPoolConfigList {
    type Item = &'a ThreadPoolConfig;
    type IntoIter = std::slice::Iter<'a, ThreadPoolConfig>;

    fn into_iter(self) -> Self::IntoIter {
        self.configs.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_pool_returns_its_position() {
        let mut pools = ThreadPoolConfigList::new();
        let first = pools.add_pool(ThreadPoolConfig::try_from(1).unwrap());
        let second = pools.add_pool(ThreadPoolConfig::try_from(4).unwrap());
        assert_eq!(first, PoolIndex::new(0));
        assert_eq!(second, PoolIndex::new(1));
        assert_eq!(pools.get(second).unwrap().thread_count().get(), 4);
    }

    #[test]
    fn zero_thread_counts_are_rejected() {
        assert!(ThreadPoolConfig::try_from(0).is_err());
        assert!(ThreadPoolConfigList::try_from([1, 0]).is_err());
    }

    #[test]
    fn iter_pairs_indices_with_configs() {
        let pools = ThreadPoolConfigList::try_from([1, 3]).unwrap();
        let entries: Vec<_> = pools.iter_indexed().collect();
        assert_eq!(entries[0].0, PoolIndex::new(0));
        assert_eq!(entries[0].1.thread_count().get(), 1);
        assert_eq!(entries[1].0, PoolIndex::new(1));
        assert_eq!(entries[1].1.thread_count().get(), 3);
    }
}
