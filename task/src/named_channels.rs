use std::cell::RefCell;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::Arc;

use super::storage::private;
use super::{
    Publisher, PublisherStorage, PublisherStoragePlan, StorageError, StorageLayout, Subscriber,
};

struct Key<T> {
    channel: Arc<str>,
    index: usize,
    payload: PhantomData<fn(T) -> T>,
}

impl<T> Clone for Key<T> {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            index: self.index,
            payload: PhantomData,
        }
    }
}

/// Typed declaration handle: channel name and index within its endpoint list.
pub struct PublisherKey<T>(Key<T>);
pub struct SubscriberKey<T>(Key<T>);

impl<T> Clone for PublisherKey<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Clone for SubscriberKey<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

struct PublisherSpec {
    loan_capacity: usize,
    retained_capacity: usize,
}

struct SubscriberSpec {
    capacity: usize,
    policy: crate::SubscriberPolicy,
    sources: Option<Vec<usize>>,
}

/// Runtime endpoint declarations for one named channel.
///
/// Endpoints may belong to different tasks. Every publisher connects to every
/// subscriber in this plan and gets its own arena. Distinct channel names can
/// carry the same T, including borrowed types, without connecting to each other.
/// Declare all endpoints before allocating the graph.
/// Keys address endpoints by channel name and declaration-order index; names
/// must be unique within the graph.
pub struct ChannelPlan<T> {
    name: Arc<str>,
    publishers: Vec<PublisherSpec>,
    subscribers: Vec<SubscriberSpec>,
    payload: PhantomData<fn(T) -> T>,
}

impl<T> ChannelPlan<T> {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into().into(),
            publishers: Vec::new(),
            subscribers: Vec::new(),
            payload: PhantomData,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn publisher(&mut self, loan_capacity: usize) -> PublisherKey<T> {
        let index = self.publishers.len();
        self.publishers.push(PublisherSpec {
            loan_capacity,
            retained_capacity: 0,
        });
        PublisherKey(Key {
            channel: self.name.clone(),
            index,
            payload: PhantomData,
        })
    }

    pub fn subscriber(&mut self, capacity: usize) -> SubscriberKey<T> {
        self.subscriber_with_policy(capacity, crate::SubscriberPolicy::default())
    }

    pub fn subscriber_with_policy(
        &mut self,
        capacity: usize,
        policy: crate::SubscriberPolicy,
    ) -> SubscriberKey<T> {
        let index = self.subscribers.len();
        self.subscribers.push(SubscriberSpec {
            capacity,
            policy,
            sources: None,
        });
        SubscriberKey(Key {
            channel: self.name.clone(),
            index,
            payload: PhantomData,
        })
    }

    pub fn reserve_retained(
        &mut self,
        publisher: &PublisherKey<T>,
        additional: usize,
    ) -> Result<(), StorageError> {
        if self.name != publisher.0.channel {
            return Err(StorageError::ForeignPublisherKey);
        }
        let spec = self
            .publishers
            .get_mut(publisher.0.index)
            .ok_or(StorageError::InvalidPublisherIndex(publisher.0.index))?;
        spec.retained_capacity = spec
            .retained_capacity
            .checked_add(additional)
            .ok_or(StorageError::CapacityOverflow)?;
        Ok(())
    }

    /// Restrict a subscriber to specified publishers before allocation. Ordinary
    /// channel subscribers receive from every publisher. Capacity budgeting stays
    /// conservative across the channel even for restricted connections.
    pub fn restrict_subscriber_sources(
        &mut self,
        subscriber: &SubscriberKey<T>,
        publishers: &[PublisherKey<T>],
    ) -> Result<(), StorageError> {
        if subscriber.0.channel != self.name {
            return Err(StorageError::InvalidConnection(
                "subscriber names another channel".into(),
            ));
        }
        for publisher in publishers {
            if publisher.0.channel != self.name {
                return Err(StorageError::ForeignPublisherKey);
            }
            if publisher.0.index >= self.publishers.len() {
                return Err(StorageError::InvalidPublisherIndex(publisher.0.index));
            }
        }
        let spec = self
            .subscribers
            .get_mut(subscriber.0.index)
            .ok_or_else(|| {
                StorageError::InvalidConnection("subscriber index is out of range".into())
            })?;
        if spec.sources.is_some() {
            return Err(StorageError::InvalidConnection(
                "subscriber sources already restricted".into(),
            ));
        }
        spec.sources = Some(publishers.iter().map(|key| key.0.index).collect());
        Ok(())
    }

    /// Query after declaring downstream subscribers when budgeting forwarding.
    pub fn publisher_capacity(&self, publisher: &PublisherKey<T>) -> Result<usize, StorageError> {
        if self.name != publisher.0.channel {
            return Err(StorageError::ForeignPublisherKey);
        }
        self.publisher_plan(
            self.publishers
                .get(publisher.0.index)
                .ok_or(StorageError::InvalidPublisherIndex(publisher.0.index))?,
        )
        .capacity()
    }

    fn publisher_plan(&self, publisher: &PublisherSpec) -> PublisherStoragePlan<T> {
        let mut plan = PublisherStoragePlan::new(publisher.loan_capacity)
            .with_retained_capacity(publisher.retained_capacity);
        for subscriber in &self.subscribers {
            plan = plan.with_subscriber(subscriber.capacity);
        }
        plan
    }
}

impl<T> private::Sealed for ChannelPlan<T> {}

impl<T> StorageLayout for ChannelPlan<T> {
    type Storage = ChannelStorage<T>;

    fn validate(&self) -> Result<(), StorageError> {
        // Validate disconnected subscribers too; they still own bounded queues.
        if self.subscribers.iter().any(|s| s.capacity == 0) {
            return Err(StorageError::ZeroSubscriberCapacity);
        }
        for publisher in &self.publishers {
            self.publisher_plan(publisher).capacity()?;
        }
        Ok(())
    }

    fn validate_channel_names(&self, names: &mut HashSet<String>) -> Result<(), StorageError> {
        if !names.insert(self.name.to_string()) {
            return Err(StorageError::DuplicateChannel(self.name.to_string()));
        }
        Ok(())
    }

    fn allocate(self) -> Result<Self::Storage, StorageError> {
        let publishers = self
            .publishers
            .iter()
            .map(|spec| self.publisher_plan(spec).allocate())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ChannelStorage {
            name: self.name,
            publishers,
            subscribers: self.subscribers,
        })
    }
}

/// Fixed arenas and endpoint configuration for one named channel.
pub struct ChannelStorage<T> {
    name: Arc<str>,
    publishers: Vec<PublisherStorage<T>>,
    subscribers: Vec<SubscriberSpec>,
}

impl<T> ChannelStorage<T> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn build(&self) -> EndpointBindings<'_, T> {
        let subscribers: Vec<_> = self
            .subscribers
            .iter()
            .map(|spec| {
                let mut subscriber = Subscriber::with_policy(spec.capacity, spec.policy);
                subscriber.set_channel_name(&self.name);
                subscriber
            })
            .collect();
        let publishers = self
            .publishers
            .iter()
            .enumerate()
            .map(|(index, storage)| {
                let mut publisher = storage.publisher();
                publisher.set_channel_name(&self.name);
                for (subscriber, spec) in subscribers.iter().zip(&self.subscribers) {
                    if spec
                        .sources
                        .as_ref()
                        .is_none_or(|sources| sources.contains(&index))
                    {
                        publisher.connect(subscriber);
                    }
                }
                RefCell::new(Some(publisher))
            })
            .collect();
        EndpointBindings {
            channel: self.name.clone(),
            publishers,
            subscribers: subscribers
                .into_iter()
                .map(|s| RefCell::new(Some(s)))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointError {
    WrongChannel,
    InvalidIndex(usize),
    AlreadyTaken,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DeclarationError {
    pub field: &'static str,
    pub expected: String,
    pub actual: String,
}

impl std::fmt::Display for DeclarationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "endpoint '{}' expects channel '{}', got '{}'",
            self.field, self.expected, self.actual
        )
    }
}

impl std::error::Error for DeclarationError {}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongChannel => f.write_str("endpoint key names another channel"),
            Self::InvalidIndex(index) => {
                write!(f, "channel endpoint index {index} is out of range")
            }
            Self::AlreadyTaken => f.write_str("endpoint has already been taken by another factory"),
        }
    }
}

impl std::error::Error for EndpointError {}

/// Temporary construction bindings. Taken endpoints borrow storage, not this
/// table, so it can be dropped immediately after building callbacks and fixtures.
pub struct EndpointBindings<'storage, T> {
    channel: Arc<str>,
    publishers: Vec<RefCell<Option<Publisher<'storage, T>>>>,
    subscribers: Vec<RefCell<Option<Subscriber<'storage, T>>>>,
}

impl<'storage, T> EndpointBindings<'storage, T> {
    pub fn configure_publisher<R>(
        &self,
        key: &PublisherKey<T>,
        configure: impl FnOnce(&mut Publisher<'storage, T>) -> R,
    ) -> Result<R, EndpointError> {
        if self.channel != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        let mut slot = self
            .publishers
            .get(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .borrow_mut();
        Ok(configure(slot.as_mut().ok_or(EndpointError::AlreadyTaken)?))
    }
    pub fn take_publisher(
        &self,
        key: &PublisherKey<T>,
    ) -> Result<Publisher<'storage, T>, EndpointError> {
        if self.channel != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        self.publishers
            .get(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }

    pub fn take_subscriber(
        &self,
        key: &SubscriberKey<T>,
    ) -> Result<Subscriber<'storage, T>, EndpointError> {
        if self.channel != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        self.subscribers
            .get(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key<T>(name: &str, index: usize) -> Key<T> {
        Key {
            channel: Arc::from(name),
            index,
            payload: PhantomData,
        }
    }
    #[test]
    fn channel_names_match_by_value_and_indices_are_checked() {
        let mut plan = ChannelPlan::<u64>::new("numbers");
        let original_pub = plan.publisher(1);
        let original_sub = plan.subscriber(1);
        let pub_key = PublisherKey(key("numbers", 0));
        let sub_key = SubscriberKey(key("numbers", 0));
        let invalid_pub = PublisherKey(key("numbers", 9));
        assert_eq!(
            plan.reserve_retained(&invalid_pub, 1),
            Err(StorageError::InvalidPublisherIndex(9))
        );
        assert_eq!(
            plan.publisher_capacity(&invalid_pub),
            Err(StorageError::InvalidPublisherIndex(9))
        );
        plan.reserve_retained(&pub_key, 2).unwrap();
        assert_eq!(plan.publisher_capacity(&pub_key).unwrap(), 6);
        let storage = crate::GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build();
        assert!(matches!(
            bindings.take_publisher(&invalid_pub),
            Err(EndpointError::InvalidIndex(9))
        ));
        assert!(matches!(
            bindings.take_subscriber(&SubscriberKey(key("numbers", 9))),
            Err(EndpointError::InvalidIndex(9))
        ));
        assert!(matches!(
            bindings.take_subscriber(&SubscriberKey(key("other", 0))),
            Err(EndpointError::WrongChannel)
        ));
        let mut publisher = bindings.take_publisher(&pub_key).unwrap();
        let subscriber = bindings.take_subscriber(&sub_key).unwrap();
        assert!(matches!(
            bindings.take_publisher(&original_pub),
            Err(EndpointError::AlreadyTaken)
        ));
        assert!(matches!(
            bindings.take_subscriber(&original_sub),
            Err(EndpointError::AlreadyTaken)
        ));
        publisher.publish(42).unwrap();
        publisher.flush(crate::time::FrameworkTime::from_nanoseconds(0));
        subscriber.update();
        assert_eq!(subscriber.input().value(), Some(&42));
    }
}
