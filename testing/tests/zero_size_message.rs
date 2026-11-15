use googletest::prelude::*;
use std::time::Duration;
use task::{Output, RequiredInput, time::FrameworkTime};
use task_macros::task_callback;
use testing::UnitTestExecutorBuilder;

struct ZstMessageA {}
struct ZstMessageB {}

struct ZstTask;
#[task_callback]
impl ZstTask {
    fn run(
        &self,
        #[trigger(true)]
        #[channel("input")]
        _input: RequiredInput<ZstMessageA>,
        #[channel("output")] output: Output<ZstMessageB>,
    ) {
        output.write(ZstMessageB {}).send();
    }
}

#[gtest]
fn send_recv_zst() {
    assert_eq!(std::mem::size_of::<ZstMessageA>(), 0);
    assert_eq!(std::mem::size_of::<ZstMessageB>(), 0);

    let mut builder = UnitTestExecutorBuilder::new();
    builder.add_task("ZstTask", ZstTask, Duration::from_millis(1));
    let mut test_publisher = builder.add_test_publisher::<ZstMessageA>("input");
    let test_subscriber = builder.add_test_subscriber::<ZstMessageB>("output");

    let storage = builder.allocate();
    let mut executor = storage.build();

    test_publisher.send(ZstMessageA {});
    executor.step();
    test_subscriber.consume(&executor, |index, message| {
        expect_eq!(index, 0);
        expect_eq!(message.header.published_at, FrameworkTime::from_millis(0));
    });
}
