#[cfg(test)]
mod tests {
    use task::callback::CallbackViews;
    use task::input::RequiredInput;
    use task::output::Output;

    use task_macros::task_callback;

    struct MyCallback {}

    #[task_callback]
    impl MyCallback {
        fn run(&self, my_input: RequiredInput<i32>, mut my_output: Output<i32>) {
            let value = *my_input + 10;
            *my_output = value;
            my_output.send();
        }
    }

    #[test]
    fn test_build_and_run() {
        let task = MyCallback {}.build();

        let subs = task.collect_subscribers();
        assert!(subs.len() == 1);
        assert!(!subs[0].config().is_optional);

        let pubs = task.collect_publishers();
        assert!(pubs.len() == 1);
    }
}
