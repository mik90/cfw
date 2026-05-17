use crate::arena::ArenaReaderPtr;

pub trait ForwardableTrait {}

pub struct Forwardable<T>(T);

impl<T> ForwardMessageTrait for Forwardable<T> {}

pub trait ForwardMessageTrait {}

/// A single forwarded message.
///
/// For multiple, i think i need multiple trait spceializations since we don't have variadics?
/// And then the getters would need to be named more uniquely, or we return a tuple.
pub struct ForwardedMessage<T, F> {
    message: T,
    forwarded_message: ArenaReaderPtr<F>,
}

impl<T, F> ForwardedMessage<T, F> {
    pub fn get_message(&self) -> &T {
        &self.message
    }

    pub fn get_message_mut(&mut self) -> &mut T {
        &mut self.message
    }

    pub fn get_forwarded_message(&self) -> &F {
        &self.forwarded_message
    }
}

impl<T, F> ForwardMessageTrait for ForwardedMessage<T, F> {}

impl<T: Default, F> ForwardedMessage<T, F> {
    pub(crate) fn new_default(forwarded_message: ArenaReaderPtr<F>) -> Self {
        Self {
            message: T::default(),
            forwarded_message,
        }
    }
}

#[cfg(test)]
mod tests {
    /*
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
        let mut normal_publisher: Publisher<u32> = Publisher::new(PublisherConfig {
            capacity: 1,
            channel_name: forwarded_channel.into(),
        });

        // Subscribes to message with intention of forwarding it
        let mut normal_subscriber: Subscriber<Forwardable<u32>> =
            Subscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: normal_channel.into(),
            });

        // Forwards normal message downstream
        let mut forwarding_publisher: Publisher<ForwardedMessage<bool, u32>> =
            Publisher::new_with_forwards(
                PublisherConfig {
                    capacity: 1,
                    channel_name: forwarded_channel.into(),
                },
                vec![forwarded_channel.into()],
            );

        // Listens to forwarded message
        let mut forwarded_subscriber: Subscriber<ForwardedMessage<bool, u32>> =
            Subscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: forwarded_channel.into(),
            });

        normal_publisher.add_typed_forwarded_subscriber(&mut normal_subscriber);
        normal_publisher.allocate_arena();

        forwarding_publisher.add_typed_subscriber(&mut forwarded_subscriber);
        forwarding_publisher.allocate_arena();
        assert_eq!(
            forwarding_publisher.get_forwarded_channels(),
            vec![forwarded_channel]
        );

        let timestamp = FrameworkTime::from_nanoseconds(42);
        {
            // Publishes message that'll be forwarded downstream
            let mut output = Output::new_default(&mut normal_publisher);
            *output = 42;
            output.send();
            normal_publisher.flush_loaned_values(timestamp);
        }
        {
            // Callback that actually forwards from its subscriber to publisher
            normal_subscriber.drain_writer_to_reader();
            let input = OptionalInput::new(&mut normal_subscriber);
            assert!(input.value().is_some());
            assert_eq!(*input.value().unwrap(), 42);

            let mut output = Output::new_default(&mut forwarding_publisher);
        }

        {
            // TODO: Some callback that listens to the forwarded message
        }
    } */
}
