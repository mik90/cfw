use crate::arena::ArenaReaderPtr;
use crate::message::Message;

pub trait ForwardableTrait {}

pub struct Forwardable<T>(T);

impl<T> ForwardMessageTrait for Forwardable<T> {}

pub trait ForwardMessageTrait {}

/// A single forwarded message.
///
/// For multiple, i think i need multiple trait spceializations since we don't have variadics?
/// And then the getters would need to be named more uniquely, or we return a tuple.
pub struct ForwardedMessage<T, F> {
    pub(crate) message: T,
    forwarded_message: ArenaReaderPtr<Message<F>>,
}

impl<T, F> ForwardedMessage<T, F> {
    pub fn get_message(&self) -> &T {
        &self.message
    }

    pub fn get_message_mut(&mut self) -> &mut T {
        &mut self.message
    }

    pub fn get_forwarded_message(&self) -> &Message<F> {
        &self.forwarded_message
    }
}

impl<T, F> ForwardMessageTrait for ForwardedMessage<T, F> {}

impl<T: Default, F> ForwardedMessage<T, F> {
    pub fn new_with_forward(forwarded_message: ArenaReaderPtr<Message<F>>) -> Self {
        Self {
            message: T::default(),
            forwarded_message,
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::generic_publisher::*;
    use crate::publisher::*;
    use crate::subscriber::*;
    use crate::time::FrameworkTime;

    /// More of an integration test, but checks that forwarded pub/sub works
    #[test]
    fn forwarded_pub_sub() {
        let normal_channel = "channel";
        let forwarded_channel = "forwarded_channel";

        // Owner of messages being forwarded in the arena
        let mut normal_publisher = Publisher::<u32>::new(PublisherConfig {
            capacity: 1,
            channel_name: forwarded_channel.into(),
        });

        // Subscribes to message with intention of forwarding it
        let mut forwardable_subscriber = ForwardableSubscriber::<u32>::new(SubscriberConfig {
            is_optional: true,
            capacity: 2,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: normal_channel.into(),
        });

        // Forwards normal message downstream with an additional bool payload
        let mut forwarding_publisher = ForwardingPublisher::<bool, u32>::new(
            PublisherConfig {
                capacity: 1,
                channel_name: forwarded_channel.into(),
            },
            vec![forwarded_channel.into()],
        );

        // Listens to forwarded message
        let mut forwarded_subscriber =
            Subscriber::<ForwardedMessage<bool, u32>>::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: forwarded_channel.into(),
            });

        normal_publisher.add_typed_forwarded_subscriber(&mut forwardable_subscriber);
        normal_publisher.allocate_arena();

        forwarding_publisher.add_typed_subscriber(&mut forwarded_subscriber);
        forwarding_publisher.allocate_arena();
        assert_eq!(
            forwarding_publisher.get_forwarded_channels(),
            vec![forwarded_channel]
        );

        let t_original = FrameworkTime::from_nanoseconds(42);
        let t_forwarding = FrameworkTime::from_nanoseconds(99);
        {
            // Publishes message that'll be forwarded downstream
            let mut output = Output::new_default(&mut normal_publisher);
            *output = 42u32;
            output.send();
            normal_publisher.flush_loaned_values(t_original);
        }
        {
            // Callback that forwards from its subscriber to publisher, adding a bool payload
            forwardable_subscriber.subscriber.drain_writer_to_reader();
            let input = ForwardableOptionalInput::new(&mut forwardable_subscriber);
            assert!(input.value().is_some());
            assert_eq!(*input.value().unwrap(), 42u32);

            let forwarded_ptr = input.forward().unwrap();

            let mut output = ForwardingOutput::new(&mut forwarding_publisher, forwarded_ptr);
            *output = true;
            output.send();
            forwarding_publisher.flush_loaned_values(t_forwarding);
        }
        {
            // Callback that reads the forwarded message
            forwarded_subscriber.drain_writer_to_reader();
            let guard = forwarded_subscriber.get_read_buffer();
            let msg = guard.front().unwrap();
            assert_eq!(msg.header.published_at, t_forwarding);
            assert_eq!(*msg.message.get_message(), true);
            let fwd = msg.message.get_forwarded_message();
            assert_eq!(fwd.message, 42u32);
            assert_eq!(fwd.header.published_at, t_original);
        }
    }
}
