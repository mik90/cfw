use task::callback_builder::CallbackBuilder;
use task::output::OutputUninit;
use task_macros::task_callback;

pub struct MyTask {}
const LARGE_SIZE: usize = 100_000;

#[derive(Clone)]
pub struct LargeMessage {
    big_array: [u64; LARGE_SIZE],
}

#[task_callback]
impl MyTask {
    fn run(&mut self, mut output_uninit: OutputUninit<LargeMessage>) {
        println!("MyTask run");
        // Maybe need to iterate over elements and write them manually
        let large_message_uninit = output_uninit.value_uninit();
        let ptr = large_message_uninit.as_mut_ptr();
        // SAFETY: We're going to initialize the single array in the message field
        unsafe {
            let big_array = &mut (*ptr).message.big_array;
            for element in big_array.iter_mut() {
                *element = 42;
            }
        }
        // SAFETY: We initialized the whole LargeMessage
        unsafe { output_uninit.send_assume_init() };
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_periodic_execution(std::time::Duration::from_millis(500))
            .with_execution_duration_callback(|| std::time::Duration::from_millis(1))
    }
}

mod tests {
    use testing::UnitTestExecutorBuilder;

    use super::*;

    #[test]
    fn send_message() {
        let large_message_channel = "LargeMessage";
        let my_task = MyTask::callback_builder(MyTask {})
            .with_publisher_channels(&[large_message_channel])
            .build()
            .expect("Could not build callback");
        let mut builder = UnitTestExecutorBuilder::new(vec![my_task]);

        let mut large_message_subscriber =
            builder.add_test_subscriber::<LargeMessage>(large_message_channel);

        let mut test_executor = builder.build();

        test_executor.step();

        let messages = large_message_subscriber.messages();
        assert_eq!(messages.len(), 1);
        let first_message = messages.first().unwrap();
        for (index, element) in first_message.message.big_array.iter().enumerate() {
            assert_eq!(*element, 42, "Index {} was not 42", index);
        }
    }
}
