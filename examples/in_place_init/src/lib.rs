use task::callback_builder::CallbackBuilder;
use task::output::OutputUninit;
use task_macros::task_callback;

pub struct MyTask {}
const LARGE_SIZE: usize = 100_000;

pub struct LargeMessage {
    big_array: [u64; LARGE_SIZE],
}

#[task_callback]
impl MyTask {
    fn run(&mut self, mut output_uninit: OutputUninit<LargeMessage>) {
        println!("MyTask run");
        let large_message_uninit = output_uninit.value_uninit();
        let ptr = large_message_uninit.as_mut_ptr();
        // SAFETY: The loan provides exclusive access to this allocation. Raw
        // writes initialize the header and every array element before assuming init.
        unsafe {
            (&raw mut (*ptr).header).write(task::message::MessageHeader::default());
            let elements = (&raw mut (*ptr).message.big_array).cast::<u64>();
            for index in 0..LARGE_SIZE {
                elements.add(index).write(42);
            }
        }
        // SAFETY: The header and the whole LargeMessage have been initialized.
        unsafe { output_uninit.send_assume_init() };
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_periodic_execution(std::time::Duration::from_millis(500))
            .with_execution_duration_callback(|| std::time::Duration::from_millis(1))
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use testing::*;

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

        let count = large_message_subscriber.messages(|index, message| {
            assert_eq!(index, 0);
            assert_eq!(
                message.header.published_at,
                task::time::FrameworkTime::from_nanoseconds(0)
            );
            for (index, element) in message.message.big_array.iter().enumerate() {
                assert_eq!(*element, 42, "Index {} was not 42", index);
            }
        });
        assert_eq!(count, 1);
    }
}
