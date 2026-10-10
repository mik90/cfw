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
        let large_message_uninit = output_uninit.payload_uninit();
        let ptr = large_message_uninit.as_mut_ptr();
        // SAFETY: The loan provides exclusive access to this allocation. Raw
        // writes initialize every array element before assuming init.
        unsafe {
            let array_ptr = (&raw mut (*ptr).big_array).cast::<u64>();
            for index in 0..LARGE_SIZE {
                array_ptr.add(index).write(42);
            }
        }
        // SAFETY: The whole LargeMessage has been initialized.
        unsafe { output_uninit.assume_init() }.send();
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use testing::*;

    #[test]
    fn send_message() {
        let large_message_channel = "LargeMessage";
        let mut builder = UnitTestExecutorBuilder::new();
        builder
            .add_scheduled_task(
                "in_place",
                MyTask {},
                std::time::Duration::from_millis(1),
                task::CallbackSchedule::on_start(),
            )
            .output_channel("output_uninit", large_message_channel);

        let large_message_subscriber =
            builder.add_test_subscriber::<LargeMessage>(large_message_channel);

        let setup = builder.allocate();
        let mut test_executor = setup.build();

        test_executor.step();

        let count = large_message_subscriber.messages(&mut test_executor, |index, message| {
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
