use std::time::Duration;

use task::Context;
use task_graph::CallbackSchedule;

#[derive(Default)]
pub struct Counter {
    next: u64,
}

impl Counter {
    pub fn schedule() -> CallbackSchedule {
        CallbackSchedule::periodic(Duration::from_millis(100))
            .with_execution_duration(Duration::from_millis(1))
    }
}

impl counter_api::Run for Counter {
    fn run(
        &mut self,
        _inputs: counter_api::Inputs<'_, '_>,
        mut outputs: counter_api::Outputs<'_, '_>,
        _context: &Context,
    ) {
        *outputs.output = self.next;
        self.next += 1;
        outputs.output.send();
    }
}

counter_api::impl_task!(Counter);

#[derive(Default)]
pub struct Doubler;

impl doubler_api::Run for Doubler {
    fn run(
        &mut self,
        inputs: doubler_api::Inputs<'_, '_>,
        mut outputs: doubler_api::Outputs<'_, '_>,
        _context: &Context,
    ) {
        *outputs.output = *inputs.input * 2;
        outputs.output.send();
    }
}

doubler_api::impl_task!(Doubler);

#[derive(Default)]
pub struct Printer;

impl printer_api::Run for Printer {
    fn run(
        &mut self,
        inputs: printer_api::Inputs<'_, '_>,
        _outputs: printer_api::Outputs<'_, '_>,
        _context: &Context,
    ) {
        println!("{}", *inputs.input);
    }
}

printer_api::impl_task!(Printer);
