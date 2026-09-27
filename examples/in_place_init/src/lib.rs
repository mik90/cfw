use task::callback_builder::CallbackBuilder;
use task::output::Output;
use task_macros::task_callback;

pub struct MyTask {}

pub struct LargeMessage {
    value: [u64; 100_000],
}

impl Default for LargeMessage {
    #[expect(
        clippy::large_stack_frames,
        reason = "TODO: Remove default() impl and allow in-place init"
    )]
    fn default() -> Self {
        LargeMessage {
            value: [0; 100_000],
        }
    }
}

#[task_callback]
impl MyTask {
    fn run(&mut self, output: Output<LargeMessage>) {
        println!("MyTask run");
        output.send();
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_periodic_execution(std::time::Duration::from_millis(500))
            .with_execution_duration_callback(|| std::time::Duration::from_millis(1))
    }
}
