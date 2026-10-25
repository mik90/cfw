use task::{Context, Forwarded};
use types::{Sources, Value};

#[derive(Default)]
pub struct Produce {
    next: u64,
}

impl produce_api::Run for Produce {
    fn run(
        &mut self,
        _inputs: produce_api::Inputs<'_, '_>,
        mut outputs: produce_api::Outputs<'_, '_>,
        _context: &Context,
    ) {
        *outputs.output = Value(self.next);
        self.next += 1;
        outputs.output.send();
    }
}

produce_api::impl_task!(Produce);

#[derive(Default)]
pub struct Forward;

impl forward_api::Run for Forward {
    fn run<'storage>(
        &mut self,
        mut inputs: forward_api::Inputs<'_, 'storage>,
        outputs: forward_api::Outputs<'_, 'storage>,
        _context: &Context,
    ) {
        let value = inputs.input.value().0;
        let source = inputs.input.forward().expect("input is present");
        outputs
            .output
            .write(Forwarded::new(Value(value), Sources { source }))
            .send();
    }
}

forward_api::impl_task!(Forward);

#[derive(Default)]
pub struct Consume;

impl consume_api::Run for Consume {
    fn run(
        &mut self,
        inputs: consume_api::Inputs<'_, '_>,
        mut outputs: consume_api::Outputs<'_, '_>,
        _context: &Context,
    ) {
        let forwarded = inputs.input.value();
        let source = &forwarded.forwarded_messages.source;
        let value = forwarded.message.0 + source.message.0;
        println!("{value}");
        *outputs.output = Value(value);
        outputs.output.send();
    }
}

consume_api::impl_task!(Consume);
