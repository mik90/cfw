#[cfg(test)]

mod tests {
    use task::forwarded_message::*;
    use task::generic_publisher::*;
    use task::generic_subscriber::*;
    use task::publisher::*;
    use task::subscriber::*;
    use task::time::FrameworkTime;

    #[test]
    fn pub_sub() {
        let mut publisher: Publisher<u32> = Publisher::new(PublisherConfig {
            capacity: 1,
            channel_name: "channel".into(),
        });

        let mut subscriber: Subscriber<u32> = Subscriber::new(SubscriberConfig {
            is_optional: true,
            capacity: 2,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: "channel".into(),
        });

        publisher.add_typed_subscriber(&mut subscriber);
        publisher.increase_arena_size(subscriber.get_config().capacity);
        publisher.allocate_arena();

        assert!(
            subscriber.able_to_run(),
            "Optional inputs should allow execution even when empty"
        );

        {
            let mut output = Output::new_default(&mut publisher);
            *output = 42;
            output.send();
        }

        let timestamp = FrameworkTime::from_nanoseconds(42);
        publisher.flush_loaned_values(timestamp);
        subscriber.drain_writer_to_reader();

        {
            let input = OptionalInput::new(&mut subscriber);
            assert!(input.value().is_some());
            assert_eq!(*input.value().unwrap(), 42);
        }
    }

    #[test]
    fn forwarded_pub_sub() {
        let mut publisher: Publisher<u32> = Publisher::new_with_forwards(
            PublisherConfig {
                capacity: 1,
                channel_name: "channel".into(),
            },
            vec!["channel".into()],
        );

        let mut subscriber: Subscriber<ForwardedMessage<u32>> = Subscriber::new(SubscriberConfig {
            is_optional: true,
            capacity: 2,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: "channel".into(),
        });

        publisher.add_typed_subscriber(&mut subscriber);
        publisher.increase_arena_size(subscriber.get_config().capacity);
        publisher.allocate_arena();

        assert!(
            subscriber.able_to_run(),
            "Optional inputs should allow execution even when empty"
        );

        {
            let mut output = Output::new_default(&mut publisher);
            *output = 42;
            output.send();
        }

        let timestamp = FrameworkTime::from_nanoseconds(42);
        publisher.flush_loaned_values(timestamp);
        subscriber.drain_writer_to_reader();

        {
            let input = OptionalInput::new(&mut subscriber);
            assert!(input.value().is_some());
            assert_eq!(*input.value().unwrap(), 42);
        }
    }
}
