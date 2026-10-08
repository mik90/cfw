use std::cell::RefCell;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::Arc;

use super::storage::private;
use super::{
    Publisher, PublisherStorage, PublisherStoragePlan, StorageError, StorageLayout, Subscriber,
};

struct Key<T> {
    channel: Arc<()>,
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

/// Typed declaration handle. Channel-plan identity is checked before using its index.
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
}

/// Runtime endpoint declarations for one named channel.
///
/// Endpoints may belong to different tasks. Every publisher connects to every
/// subscriber in this plan and gets its own arena. Distinct channel names can
/// carry the same T, including borrowed types, without connecting to each other.
/// Declare all endpoints before allocating the graph.
pub struct ChannelPlan<T> {
    name: String,
    identity: Arc<()>,
    publishers: Vec<PublisherSpec>,
    subscribers: Vec<SubscriberSpec>,
    payload: PhantomData<fn(T) -> T>,
}

impl<T> ChannelPlan<T> {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            identity: Arc::new(()),
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
            channel: self.identity.clone(),
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
        self.subscribers.push(SubscriberSpec { capacity, policy });
        SubscriberKey(Key {
            channel: self.identity.clone(),
            index,
            payload: PhantomData,
        })
    }

    pub fn reserve_retained(
        &mut self,
        publisher: &PublisherKey<T>,
        additional: usize,
    ) -> Result<(), StorageError> {
        if !Arc::ptr_eq(&self.identity, &publisher.0.channel) {
            return Err(StorageError::ForeignPublisherKey);
        }
        let spec = &mut self.publishers[publisher.0.index];
        spec.retained_capacity = spec
            .retained_capacity
            .checked_add(additional)
            .ok_or(StorageError::CapacityOverflow)?;
        Ok(())
    }

    /// Query after declaring downstream subscribers when budgeting forwarding.
    pub fn publisher_capacity(&self, publisher: &PublisherKey<T>) -> Result<usize, StorageError> {
        if !Arc::ptr_eq(&self.identity, &publisher.0.channel) {
            return Err(StorageError::ForeignPublisherKey);
        }
        self.publisher_plan(&self.publishers[publisher.0.index])
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
        if !names.insert(self.name.clone()) {
            return Err(StorageError::DuplicateChannel(self.name.clone()));
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
            identity: self.identity,
            publishers,
            subscribers: self.subscribers,
        })
    }
}

/// Fixed arenas and endpoint configuration for one named channel.
pub struct ChannelStorage<T> {
    name: String,
    identity: Arc<()>,
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
            .map(|storage| {
                let mut publisher = storage.publisher();
                publisher.set_channel_name(&self.name);
                for subscriber in &subscribers {
                    publisher.connect(subscriber);
                }
                RefCell::new(Some(publisher))
            })
            .collect();
        EndpointBindings {
            identity: self.identity.clone(),
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
            Self::WrongChannel => f.write_str("endpoint key belongs to another channel plan"),
            Self::AlreadyTaken => f.write_str("endpoint has already been taken by another factory"),
        }
    }
}

impl std::error::Error for EndpointError {}

/// Temporary construction bindings. Taken endpoints borrow storage, not this
/// table, so it can be dropped immediately after building callbacks and fixtures.
pub struct EndpointBindings<'storage, T> {
    identity: Arc<()>,
    publishers: Vec<RefCell<Option<Publisher<'storage, T>>>>,
    subscribers: Vec<RefCell<Option<Subscriber<'storage, T>>>>,
}

impl<'storage, T> EndpointBindings<'storage, T> {
    pub fn take_publisher(
        &self,
        key: &PublisherKey<T>,
    ) -> Result<Publisher<'storage, T>, EndpointError> {
        if !Arc::ptr_eq(&self.identity, &key.0.channel) {
            return Err(EndpointError::WrongChannel);
        }
        self.publishers[key.0.index]
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }

    pub fn take_subscriber(
        &self,
        key: &SubscriberKey<T>,
    ) -> Result<Subscriber<'storage, T>, EndpointError> {
        if !Arc::ptr_eq(&self.identity, &key.0.channel) {
            return Err(EndpointError::WrongChannel);
        }
        self.subscribers[key.0.index]
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
}
