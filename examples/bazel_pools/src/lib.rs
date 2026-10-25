use messages::{Iox2, Value};
use task::Context;

#[derive(Default)]
pub struct Counter {
    next: u64,
}

impl counter_api::Run for Counter {
    fn run(
        &mut self,
        _inputs: counter_api::Inputs<'_, '_>,
        mut outputs: counter_api::Outputs<'_, '_>,
        _context: &Context,
    ) {
        *outputs.output = Iox2(self.next);
        self.next += 1;
        outputs.output.send();
        outputs.notify.send();
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
        let Some(value) = inputs.input.value() else {
            return;
        };
        *outputs.output = Value(value.0 * 2);
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
        println!("{}", inputs.input.0);
    }
}

printer_api::impl_task!(Printer);

#[derive(Default)]
pub struct Pulse;

impl pulse_api::Run for Pulse {
    fn run(
        &mut self,
        inputs: pulse_api::Inputs<'_, '_>,
        _outputs: pulse_api::Outputs<'_, '_>,
        _context: &Context,
    ) {
        println!("pulse {}", inputs.event.count());
    }
}

pulse_api::impl_task!(Pulse);
