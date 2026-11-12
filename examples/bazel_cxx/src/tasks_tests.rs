#[cfg(all(test, not(miri)))]
mod tests {
    use std::time::Duration;

    use cxx_fizz_buzz_messages::Vec3;
    use task::time::FrameworkTime;
    use task_graph::CallbackSchedule;
    use test_tasks::{IncrementingIntegerPublisher, StringCollector, fizz_buzz};
    use testing::UnitTestExecutorBuilder;

    use super::{FizzBuzzCalculator, NormCalculator, OptionalProbe, SpanProbe};

    #[test]
    fn cxx_calculator_matches_rust_reference() {
        let collector = StringCollector::default();
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task(
            "calculator",
            FizzBuzzCalculator::new(),
            Duration::from_millis(5),
        );
        builder.add_task("collector", collector.clone(), Duration::from_millis(2));
        let mut input = builder.add_test_publisher::<u64>("integer");
        builder.run(|mut executor| {
            for value in 0..16 {
                input.send(value);
                executor.step();
                executor.step();
            }
        });
        assert_eq!(
            collector.stored_strings(),
            (0..16).map(fizz_buzz).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cxx_calculator_wires_declared_channels() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task(
            "calculator",
            FizzBuzzCalculator::new(),
            Duration::from_millis(5),
        );
        let _input = builder.add_test_publisher::<u64>("integer");
        let _output = builder.add_test_subscriber::<String>("fizz_buzz_string");
        builder.run(|_| {});
    }

    #[test]
    fn cxx_optional_input_queue_and_header_semantics() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("probe", OptionalProbe::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<u64>("optional_probe_in");
        let output = builder.add_test_subscriber::<String>("optional_probe_out");
        let mut reports = Vec::new();
        builder.run(|mut executor| {
            input.send_at(FrameworkTime::from_millis(3), 7);
            input.send_at(FrameworkTime::from_millis(3), 9);
            for _ in 0..3 {
                executor.step();
                output.messages(&mut executor, |_, message| {
                    reports.push(message.message.clone());
                });
            }
        });
        assert_eq!(reports.first().map(String::as_str), Some("7:2:7:3000000"));
        assert!(reports.len() <= 2);
        if let Some(second) = reports.get(1) {
            assert_eq!(second, "9:1:9:3000000");
        }
    }

    #[test]
    fn cxx_span_and_uninit_outputs() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("span", SpanProbe::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<u64>("span_in");
        let numbers = builder.add_test_subscriber::<u64>("span_out");
        let labels = builder.add_test_subscriber::<String>("span_label");
        let mut collected_numbers = Vec::new();
        let mut collected_labels = Vec::new();
        builder.run(|mut executor| {
            input.send(6);
            executor.step();
            numbers.messages(&mut executor, |_, message| {
                collected_numbers.push(message.message);
            });
            labels.messages(&mut executor, |_, message| {
                collected_labels.push(message.message.clone());
            });
        });
        assert_eq!(collected_numbers, vec![1, 0, 2, 3]);
        assert_eq!(collected_labels, vec!["span:4".to_string()]);
    }

    #[test]
    fn cxx_uninit_output_unsent_is_discarded() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("span", SpanProbe::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<u64>("span_in");
        let labels = builder.add_test_subscriber::<String>("span_label");
        let mut collected = Vec::new();
        builder.run(|mut executor| {
            input.send(0);
            executor.step();
            labels.messages(&mut executor, |_, message| {
                collected.push(message.message.clone());
            });
            input.send(2);
            executor.step();
            labels.messages(&mut executor, |_, message| {
                collected.push(message.message.clone());
            });
        });
        assert_eq!(collected, vec!["span:2".to_string()]);
    }

    #[test]
    fn cxx_shared_struct_payload_runs_through_cxx() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("norm", NormCalculator::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<Vec3>("norm_in");
        let output = builder.add_test_subscriber::<f64>("norm_out");
        let mut lengths = Vec::new();
        builder.run(|mut executor| {
            input.send(Vec3 {
                x: 3.0,
                y: 4.0,
                z: 12.0,
            });
            executor.step();
            output.messages(&mut executor, |_, message| lengths.push(message.message));
        });
        assert_eq!(lengths, vec![13.0]);
    }

    #[test]
    fn cxx_fallible_run_surfaces_exceptions() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("norm", NormCalculator::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<Vec3>("norm_in");
        let error = builder.run(|mut executor| {
            input.send(Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            });
            executor.try_step().unwrap_err()
        });
        match error {
            testing::StepError::Callback {
                failure: testing::BatchFailure::Panic,
                ..
            } => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn cxx_calculator_runs_under_live_executor() {
        let collector = StringCollector::default();
        let mut app = task_graph::ApplicationBuilder::new();
        app.add_task(
            "producer",
            IncrementingIntegerPublisher::default(),
            CallbackSchedule::periodic(Duration::from_millis(10))
                .with_execution_duration(Duration::from_millis(1)),
        );
        app.add_task(
            "calculator",
            FizzBuzzCalculator::new(),
            CallbackSchedule::input_triggered().with_execution_duration(Duration::from_millis(1)),
        );
        app.add_task(
            "collector",
            collector.clone(),
            CallbackSchedule::input_triggered().with_execution_duration(Duration::from_millis(1)),
        );
        let storage = app.allocate().unwrap();
        let (graph, bindings) = storage.bind_and_build().unwrap();
        live_executor::LiveExecutor::new(
            task_graph::ThreadPoolConfig::try_from(2).unwrap(),
            graph,
        )
            .unwrap()
            .run_with(|stop| {
                let start = std::time::Instant::now();
                while collector.len() < 4 {
                    assert!(
                        start.elapsed() < Duration::from_secs(5),
                        "timed out waiting for C++ callback output"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                stop.request_stop();
            })
            .unwrap();
        drop(bindings);

        let collected = collector.stored_strings();
        assert!(!collected.is_empty());
        for (index, value) in collected.iter().enumerate() {
            assert_eq!(*value, fizz_buzz(index as u64));
        }
    }
}
