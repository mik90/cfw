#[cfg(feature = "cxx")]
mod input_span {
    use std::time::Duration;

    use task_cxx_tests::{INPUT_SPAN_IN_CHANNEL, INPUT_SPAN_OUT_CHANNEL, InputSpanProbe};
    use testing::UnitTestExecutorBuilder;

    #[test]
    fn input_span_pop_and_clear() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("probe", InputSpanProbe::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<u64>(INPUT_SPAN_IN_CHANNEL);
        let output = builder.add_test_subscriber::<String>(INPUT_SPAN_OUT_CHANNEL);
        let mut reports = Vec::new();
        builder.run(|mut executor| {
            for value in 10..14 {
                input.send(value);
            }
            executor.step();
            output.messages(&mut executor, |_, message| {
                reports.push(message.message.clone());
            });
        });
        assert_eq!(reports, vec!["4:10:1:3:11:0:0".to_string()]);
    }
}
