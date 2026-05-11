#[cfg(test)]

mod tests {
    /*
    use task::generic_publisher::*;
    use task::generic_subscriber::*;
    use task::publisher::*;
    use task::subscriber::*;

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

        publisher.add_typed_subscriber(
            subscriber.get_write_guard(),
            subscriber.get_config().is_trigger,
        );
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

        publisher.flush_loaned_values();
        subscriber.drain_writer_to_reader();

        {
            let input = OptionalInput::new(&mut subscriber);
            assert!(input.value().is_some());
            assert_eq!(*input.value().unwrap(), 42);
        }
    } */
}
