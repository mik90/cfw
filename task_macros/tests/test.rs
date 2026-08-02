#[cfg(test)]
mod tests {
    use std::time::Duration;

    use task::callback::CallbackViews;
    use task::callback_builder::CallbackBuilder;
    use task::input::RequiredInput;
    use task::output::Output;

    use task_macros::task_callback;

    struct MyCallback {}

    #[task_callback]
    impl MyCallback {
        fn run(
            &self,
            #[channel("in")] my_input: RequiredInput<i32>,
            #[channel("out")] mut my_output: Output<i32>,
        ) {
            let value = *my_input + 10;
            *my_output = value;
            my_output.send();
        }

        fn callback_builder(self) -> CallbackBuilder {
            self.builder()
                .with_execution_duration_callback(|| Duration::from_millis(1))
        }
    }

    #[test]
    fn test_build_and_run() {
        let task = MyCallback {}.callback_builder().build().unwrap();
        let node = task.callback();

        let subs = node.collect_subscribers();
        assert_eq!(subs.len(), 1);
        assert!(!subs[0].config().is_optional);
        assert_eq!(subs[0].config().channel_name, "in");

        let pubs = node.collect_publishers();
        assert_eq!(pubs.len(), 1);
        assert_eq!(pubs[0].config().channel_name, "out");
    }
}
