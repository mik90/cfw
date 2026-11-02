use crate::output::{Output, OutputSpan, OutputUninit};
use crate::subscriber::SubscriberWriter;
use base::arena::{ArenaAllocator, ArenaPtr, ArenaPtrUninit};

use super::Subscriber;
use crate::message::{Message, MessageHeader};
use crate::time::FrameworkTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum LoanError {
    LoanCapacityReached,
    ArenaExhausted,
    Transport(TransportError),
}

/// Failure reported by a real message transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    HeaderInjectionWithPendingBatch,
    Iox2CannotSendToNative,
    #[cfg(feature = "iceoryx2")]
    Iox2Loan(iceoryx2::port::LoanError),
    #[cfg(feature = "iceoryx2")]
    Iox2Send(iceoryx2::port::SendError),
}

/// Failure from replay-only operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayError(String);
impl ReplayError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ReplayError {}

#[cfg(feature = "serde")]
impl serde::Serialize for TransportError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let name = match self {
            Self::HeaderInjectionWithPendingBatch => "HeaderInjectionWithPendingBatch",
            Self::Iox2CannotSendToNative => "LocalPublicationRequiresTransportDelivery",
            #[cfg(feature = "iceoryx2")]
            Self::Iox2Loan(_) => "Iox2Loan",
            #[cfg(feature = "iceoryx2")]
            Self::Iox2Send(_) => "Iox2Send",
        };
        serializer.serialize_str(name)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for TransportError {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = TransportError;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a transport error variant name")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                match value {
                    "HeaderInjectionWithPendingBatch" => {
                        Ok(TransportError::HeaderInjectionWithPendingBatch)
                    }
                    "LocalPublicationRequiresTransportDelivery" => {
                        Ok(TransportError::Iox2CannotSendToNative)
                    }
                    // The underlying iox2 detail is not persisted, so only the
                    // category survives a round trip.
                    #[cfg(feature = "iceoryx2")]
                    "Iox2Loan" => Ok(TransportError::Iox2Loan(
                        iceoryx2::port::LoanError::InternalFailure,
                    )),
                    #[cfg(feature = "iceoryx2")]
                    "Iox2Send" => Ok(TransportError::Iox2Send(
                        iceoryx2::port::SendError::InternalError,
                    )),
                    other => Err(E::custom(format!(
                        "unknown transport error variant '{other}'"
                    ))),
                }
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for LoanError {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = LoanError;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a loan error")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                match value {
                    "LoanCapacityReached" => Ok(LoanError::LoanCapacityReached),
                    "ArenaExhausted" => Ok(LoanError::ArenaExhausted),
                    "Transport(\"header injection requires an empty pending batch\")" => Ok(
                        LoanError::Transport(TransportError::HeaderInjectionWithPendingBatch),
                    ),
                    "Transport(\"immediate IPC publication requires transport delivery\")" => {
                        Ok(LoanError::Transport(TransportError::Iox2CannotSendToNative))
                    }
                    other => Err(E::custom(format!("unknown loan error '{other}'"))),
                }
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
                let error = map.next_value::<TransportError>()?;
                Ok(LoanError::Transport(error))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A typed publisher borrowing storage allocated before connection/execution.
/// Connection does not resize its arena: the graph plan owns capacity decisions.
pub struct Publisher<'storage, MessageType> {
    /// pub(crate) so that Output can borrow the minimm amount needed
    pub(crate) allocator: ArenaAllocator<'storage, Message<MessageType>>,
    /// Amount of loans that this publisher is allowed to give
    loan_capacity: usize,
    /// pub(crate) so that Output can borrow the minimm amount needed
    pub(crate) pending: Vec<ArenaPtr<'storage, Message<MessageType>>>,
    /// Subscribers to send data to
    subscribers: Vec<SubscriberWriter<'storage, MessageType>>,
    channel: String,
    publisher_index: u32,
    observers: Vec<PublishObserver<'storage, MessageType>>,
}

impl<'storage, T> std::fmt::Debug for Publisher<'storage, T> {
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
        for error in [
            LoanError::LoanCapacityReached,
            LoanError::ArenaExhausted,
            LoanError::Transport(TransportError::HeaderInjectionWithPendingBatch),
            LoanError::Transport(TransportError::Iox2CannotSendToNative),
        ] {
            let json = serde_json::to_string(&error).unwrap();
            assert_eq!(serde_json::from_str::<LoanError>(&json).unwrap(), error);
        }
    }
}

impl<'storage, T> Publisher<'storage, T> {
    pub fn new(allocator: ArenaAllocator<'storage, Message<T>>, loan_capacity: usize) -> Self {
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

    pub fn connect(&mut self, subscriber: &Subscriber<'storage, T>) {
        self.subscribers.push(subscriber.writer());
    }

    /// Observe this publisher's initialized, stamped outputs before fan-out.
    /// References are scoped to the observer call and cannot escape it.
    pub fn observe(&mut self, observer: impl FnMut(&Message<T>) + Send + 'storage) {
        self.observers.push(Box::new(observer));
    }

    pub fn loan_uninit(&mut self) -> Result<OutputUninit<'_, 'storage, T>, LoanError> {
        if self.pending.len() >= self.loan_capacity {
            return Err(LoanError::LoanCapacityReached);
        }
        let ptr = allocate_output(&self.allocator)?;
        Ok(OutputUninit::new(self, ptr))
    }

    pub fn loan(&mut self, value: T) -> Result<Output<'_, 'storage, T>, LoanError> {
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
    /// publisher.flush(FrameworkTime::from_nanoseconds(123));
    /// ```
    pub fn span(&mut self) -> OutputSpan<'_, 'storage, T> {
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
        self.flush_headers(|index| MessageHeader {
            published_at: timestamp,
            publisher_index,
            batch_index: u32::try_from(index).expect("publisher batch index overflow"),
        });
    }

    /// Inject one message with its original publication identity intact.
    /// Cannot be mixed with an outstanding ordinary output batch.
    pub fn publish_with_header(
        &mut self,
        header: MessageHeader,
        value: T,
    ) -> Result<(), LoanError> {
        if !self.pending.is_empty() {
            return Err(LoanError::Transport(
                TransportError::HeaderInjectionWithPendingBatch,
            ));
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

pub(crate) fn allocate_output<'storage, T>(
    allocator: &ArenaAllocator<'storage, Message<T>>,
) -> Result<ArenaPtrUninit<'storage, Message<T>>, LoanError> {
    let mut ptr = allocator
        .try_allocate_uninit()
        .ok_or(LoanError::ArenaExhausted)?;
    let message = ptr.payload_uninit().as_mut_ptr();
    // SAFETY: This reservation exclusively owns storage. Raw field writes
    // initialize the header without creating a reference to uninitialized T.
    unsafe { (&raw mut (*message).header).write(MessageHeader::default()) };
    Ok(ptr)
}
