use crate::output::{Output, OutputInit, OutputSpan};
use crate::subscriber::SubscriberWriter;
use base::arena::{ArenaAllocator, ArenaPtr, ArenaPtrUninit};

use super::Subscriber;
use crate::message::{Message, MessageHeader};
use crate::time::FrameworkTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoanError {
    LoanCapacityReached,
    ArenaExhausted,
    /// `Publisher::publish_with_header` requires an empty pending batch.
    PendingBatchNotEmpty,
    /// Immediate IPC publication requires transport delivery.
    #[cfg(feature = "iceoryx2")]
    Iox2CannotSendToNative,
    #[cfg(feature = "iceoryx2")]
    Iox2Loan(iceoryx2::port::LoanError),
    #[cfg(feature = "iceoryx2")]
    Iox2Send(iceoryx2::port::SendError),
}

#[cfg(feature = "serde")]
impl serde::Serialize for LoanError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let name = match self {
            Self::LoanCapacityReached => "LoanCapacityReached",
            Self::ArenaExhausted => "ArenaExhausted",
            Self::PendingBatchNotEmpty => "PendingBatchNotEmpty",
            #[cfg(feature = "iceoryx2")]
            Self::Iox2CannotSendToNative => "Iox2CannotSendToNative",
            #[cfg(feature = "iceoryx2")]
            Self::Iox2Loan(_) => "Iox2Loan",
            #[cfg(feature = "iceoryx2")]
            Self::Iox2Send(_) => "Iox2Send",
        };
        serializer.serialize_str(name)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for LoanError {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        fn parse<E: serde::de::Error>(value: &str) -> Result<LoanError, E> {
            match value {
                "LoanCapacityReached" => Ok(LoanError::LoanCapacityReached),
                "ArenaExhausted" => Ok(LoanError::ArenaExhausted),
                "PendingBatchNotEmpty" | "HeaderInjectionWithPendingBatch" => {
                    Ok(LoanError::PendingBatchNotEmpty)
                }
                "Transport(\"header injection requires an empty pending batch\")" => {
                    Ok(LoanError::PendingBatchNotEmpty)
                }
                #[cfg(feature = "iceoryx2")]
                "Iox2CannotSendToNative" | "LocalPublicationRequiresTransportDelivery" => {
                    Ok(LoanError::Iox2CannotSendToNative)
                }
                #[cfg(feature = "iceoryx2")]
                "Transport(\"immediate IPC publication requires transport delivery\")" => {
                    Ok(LoanError::Iox2CannotSendToNative)
                }
                // The underlying iox2 detail is not persisted, so only the
                // category survives a round trip.
                #[cfg(feature = "iceoryx2")]
                "Iox2Loan" => Ok(LoanError::Iox2Loan(
                    iceoryx2::port::LoanError::InternalFailure,
                )),
                #[cfg(feature = "iceoryx2")]
                "Iox2Send" => Ok(LoanError::Iox2Send(
                    iceoryx2::port::SendError::InternalError,
                )),
                other => Err(E::custom(format!("unknown loan error '{other}'"))),
            }
        }
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = LoanError;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a loan error")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                parse(value)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let Some(key) = map.next_key::<String>()? else {
                    return Err(serde::de::Error::custom("empty loan error"));
                };
                if key != "Transport" {
                    return Err(serde::de::Error::custom(format!(
                        "unknown loan error variant '{key}'"
                    )));
                }
                let value = map.next_value::<String>()?;
                parse(&value)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A typed publisher borrowing storage allocated before connection/execution.
/// Connection does not resize its arena: the graph plan owns capacity decisions.
pub struct Publisher<'graph_storage, MessageType> {
    /// pub(crate) so that Output can borrow the minimm amount needed
    pub(crate) allocator: ArenaAllocator<'graph_storage, Message<MessageType>>,
    /// Amount of loans that this publisher is allowed to give
    loan_capacity: usize,
    /// pub(crate) so that Output can borrow the minimm amount needed
    pub(crate) pending: Vec<ArenaPtr<'graph_storage, Message<MessageType>>>,
    /// Subscribers to send data to
    subscribers: Vec<SubscriberWriter<'graph_storage, MessageType>>,
    channel: String,
    publisher_index: u32,
    observers: Vec<PublishObserver<'graph_storage, MessageType>>,
}

impl<'graph_storage, T> std::fmt::Debug for Publisher<'graph_storage, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("allocator_capacity", &self.allocator.capacity())
            .field("loan_capacity", &self.loan_capacity)
            .field("pending_length", &self.pending.len())
            .field("subscriber_writers", &self.subscribers)
            .field("channel", &self.channel)
            .field("publisher_index", &self.publisher_index)
            .finish_non_exhaustive()
    }
}

type PublishObserver<'a, T> = Box<dyn FnMut(&Message<T>) + Send + 'a>;

#[cfg(all(test, feature = "serde"))]
mod serde_tests {
    use super::*;

    #[test]
    fn loan_error_round_trips() {
        let mut errors = vec![
            LoanError::LoanCapacityReached,
            LoanError::ArenaExhausted,
            LoanError::PendingBatchNotEmpty,
        ];
        #[cfg(feature = "iceoryx2")]
        errors.push(LoanError::Iox2CannotSendToNative);
        for error in errors {
            let json = serde_json::to_string(&error).unwrap();
            assert_eq!(serde_json::from_str::<LoanError>(&json).unwrap(), error);
        }
    }
}

impl<'graph_storage, T> Publisher<'graph_storage, T> {
    pub fn new(
        allocator: ArenaAllocator<'graph_storage, Message<T>>,
        loan_capacity: usize,
    ) -> Self {
        assert!(
            u32::try_from(loan_capacity).is_ok(),
            "publisher batch capacity exceeds u32"
        );
        Self {
            allocator,
            loan_capacity,
            pending: Vec::with_capacity(loan_capacity),
            subscribers: Vec::new(),
            channel: String::new(),
            publisher_index: 0,
            observers: Vec::new(),
        }
    }

    pub(crate) fn set_channel_name(&mut self, name: &str) {
        self.channel = name.into();
    }

    pub fn loan_capacity(&self) -> usize {
        self.loan_capacity
    }

    pub fn channel_name(&self) -> &str {
        &self.channel
    }

    pub fn publisher_index(&self) -> u32 {
        self.publisher_index
    }

    /// Configure the channel-local index of an unplanned publisher.
    /// Planned endpoints receive their index automatically during binding.
    pub fn set_publisher_index(&mut self, index: u32) {
        self.publisher_index = index;
    }

    pub fn connect(&mut self, subscriber: &Subscriber<'graph_storage, T>) {
        self.subscribers.push(subscriber.writer());
    }

    /// Observe this publisher's initialized, stamped outputs before fan-out.
    /// References are scoped to the observer call and cannot escape it.
    pub fn observe(&mut self, observer: impl FnMut(&Message<T>) + Send + 'graph_storage) {
        self.observers.push(Box::new(observer));
    }

    pub fn loan_uninit(&mut self) -> Result<Output<'_, 'graph_storage, T>, LoanError> {
        if self.pending.len() >= self.loan_capacity {
            return Err(LoanError::LoanCapacityReached);
        }
        let ptr = allocate_output(&self.allocator)?;
        Ok(Output::new(self, ptr))
    }

    pub fn loan(&mut self, value: T) -> Result<OutputInit<'_, 'graph_storage, T>, LoanError> {
        self.loan_uninit().map(|loan| loan.write(value))
    }

    pub fn publish(&mut self, value: T) -> Result<(), LoanError> {
        self.loan(value)?.send();
        Ok(())
    }

    /// Borrow this publisher for a span of simultaneously outstanding loans.
    /// Sent outputs join the ordinary pending queue in send order; the executor
    /// stamps and publishes them only after the callback succeeds.
    ///
    /// ```
    /// use base::arena::Arena;
    /// use task::{Publisher, message::Message, time::FrameworkTime};
    /// let storage = Arena::<Message<u64>>::new(2);
    /// let mut publisher = Publisher::new(storage.allocator(), 2);
    /// {
    ///     let span = publisher.span();
    ///     let first = span.loan_uninit().unwrap();
    ///     let second = span.loan_uninit().unwrap();
    ///     let first = first.write(10);
    ///     let second = second.write(*first + 1);
    ///     second.send();
    ///     first.send();
    /// }
    /// publisher.flush(FrameworkTime::from_nanos(123));
    /// ```
    pub fn span(&mut self) -> OutputSpan<'_, 'graph_storage, T> {
        OutputSpan::new(self)
    }

    /// Release sent-but-unpublished outputs after a failed callback.
    pub fn discard_pending(&mut self) {
        self.pending.clear();
    }

    pub fn visit_pending_headers(&self, mut visit: impl FnMut(MessageHeader)) {
        for (index, message) in self.pending.iter().enumerate() {
            // SAFETY: pending contains initialized loans; publication/mutation
            // requires an exclusive publisher borrow.
            let mut header = unsafe { message.assume_init_ref() }.header;
            header.publisher_index = self.publisher_index;
            header.batch_index = u32::try_from(index).expect("publisher batch index overflow");
            visit(header);
        }
    }

    /// Publish a fully initialized batch at an executor-provided timestamp.
    /// Unsent outputs are released when their output handle is dropped.
    pub fn flush(&mut self, timestamp: FrameworkTime) {
        let publisher_index = self.publisher_index;
        self.flush_with(|index| MessageHeader {
            published_at: timestamp,
            publisher_index,
            batch_index: u32::try_from(index).expect("publisher batch index overflow"),
        });
    }

    /// Publish the pending batch using a caller-supplied header stamp.
    /// Unsent outputs are released when their output handle is dropped.
    pub fn flush_with(&mut self, stamp: impl FnMut(usize) -> MessageHeader) {
        self.flush_headers(stamp);
    }

    /// Inject one message with its original publication identity intact.
    /// Cannot be mixed with an outstanding ordinary output batch.
    pub fn publish_with_header(
        &mut self,
        header: MessageHeader,
        value: T,
    ) -> Result<(), LoanError> {
        if !self.pending.is_empty() {
            return Err(LoanError::PendingBatchNotEmpty);
        }
        self.publish(value)?;
        self.flush_headers(|_| header);
        Ok(())
    }

    fn flush_headers(&mut self, mut header: impl FnMut(usize) -> MessageHeader) {
        for (index, ptr) in self.pending.drain(..).enumerate() {
            // SAFETY: Pending pointers are initialized and exclusively owned;
            // no clones are exposed before the header is stamped.
            unsafe { (*ptr.payload.get()).assume_init_mut().header = header(index) };
            for observer in &mut self.observers {
                // SAFETY: pending outputs are fully initialized and immutable for
                // observer access; stamping precedes observation and publication.
                observer(unsafe { ptr.assume_init_ref() });
            }
            for subscriber in &self.subscribers {
                subscriber.write(ptr.clone());
            }
        }
    }
}

pub(crate) fn allocate_output<'graph_storage, T>(
    allocator: &ArenaAllocator<'graph_storage, Message<T>>,
) -> Result<ArenaPtrUninit<'graph_storage, Message<T>>, LoanError> {
    let mut ptr = allocator
        .try_allocate_uninit()
        .ok_or(LoanError::ArenaExhausted)?;
    let message = ptr.payload_uninit().as_mut_ptr();
    // SAFETY: This reservation exclusively owns storage. Raw field writes
    // initialize the header without creating a reference to uninitialized T.
    unsafe { (&raw mut (*message).header).write(MessageHeader::default()) };
    Ok(ptr)
}
