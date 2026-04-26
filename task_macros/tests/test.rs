#[cfg(test)]

mod tests {
    use task::publisher::*;
    use task::subscriber::*;
    use task_macros::task_callback;

    struct MyTask {}

    #[task_callback]
    impl MyTask {
        fn run(&self, my_input: RequiredInput<i32>, mut my_output: Output<i32>) {
            // Task logic here
            let value = *my_input + 10;
            *my_output = value;
            my_output.send();
        }
    }

    #[test]
    fn test_macro() {
        // The publishers should live longer than the subscribers since they own the arenas, which subscribers may still hold ArenaPtrs onto
        let mut test_publisher = Publisher::<i32>::new(PublisherConfig {
            capacity: 1,
            channel_name: "channel".into(),
        });
        let mut task_publishers: Vec<Box<dyn GenericPublisher + 'static>> =
            vec![Box::new(Publisher::<i32>::new(PublisherConfig {
                capacity: 1,
                channel_name: "channel".into(),
            }))];

        let mut task_subscribers: Vec<Box<dyn GenericSubscriber + 'static>> =
            vec![Box::new(Subscriber::<i32>::new(SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: "channel".into(),
            }))];
        let mut test_subscriber = Subscriber::<i32>::new(SubscriberConfig {
            is_optional: false,
            capacity: 1,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: "channel".into(),
        });
        assert!(
            test_publisher
                .connect_to_subscriber(task_subscribers.first_mut().unwrap().as_mut())
                .is_ok()
        );
        assert!(
            task_publishers
                .first_mut()
                .unwrap()
                .as_mut()
                .connect_to_subscriber(&mut test_subscriber as &mut dyn GenericSubscriber)
                .is_ok()
        );

        task_publishers.first_mut().unwrap().allocate_arena();
        test_publisher.allocate_arena();

        {
            let mut output = Output::new_default(&mut test_publisher);
            *output = 22;
            output.send();
        }

        test_publisher.flush_loaned_values(task::time::FrameworkTime::now());

        for subscriber in task_subscribers.iter_mut() {
            subscriber.drain_writer_to_reader();
        }

        let mut task = MyTask {};

        let ctx = task::context::Context::new(task::time::FrameworkTime::now());
        let result = task.run_generic(
            task_subscribers.as_mut_slice(),
            task_publishers.as_mut_slice(),
            &ctx,
        );

        for publisher in task_publishers.iter_mut() {
            publisher.flush_loaned_values(task::time::FrameworkTime::now());
        }

        assert_eq!(result.num_iterations, 1);
    }
    #[test]
    fn make_pub_sub() {
        let task = MyTask {};

        let mut subscribers = task.build_subscribers();
        assert!(subscribers.len() == 1);
        let maybe_typed_subscriber = subscribers[0].as_any().downcast_ref::<Subscriber<i32>>();
        assert!(maybe_typed_subscriber.is_some());
        assert!(maybe_typed_subscriber.unwrap().get_config().is_optional == false);

        let mut publishers = task.build_publishers();
        assert!(publishers.len() == 1);
        let maybe_typed_publisher = publishers[0].as_any().downcast_ref::<Publisher<i32>>();
        assert!(maybe_typed_publisher.is_some());
        assert!(maybe_typed_publisher.unwrap().get_config().capacity == 1);
    }
}
