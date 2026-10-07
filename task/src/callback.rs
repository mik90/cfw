use crate::time::FrameworkTime;
use crate::{Context, LoanError, Publisher, Subscriber};

/// Callback body and IO lifecycle, shared by live and deterministic executors.
/// Executors invoke `execute_callback` rather than calling `run` directly.
pub trait Callback: Send {
    // TODO(port-execution-logging): Restore execution-log hooks and replay metadata
    // for inputs, committed outputs, and events within the split callback lifecycle.
    #[cfg(feature = "iceoryx2")]
    fn take_iox2_events(&mut self) -> Vec<crate::iox2::Iox2EventRegistration> {
        Vec::new()
    }
    fn set_waker(&mut self, _wake: crate::wake::WakeHandle) {}
    fn has_pending_inputs(&self) -> bool {
        false
    }
    fn visit_channel_names(&self, _visit: &mut dyn FnMut(&str)) {}
    fn update_inputs(&mut self) {}
    fn required_inputs_ready(&self) -> bool {
        true
    }
    fn run(&mut self, context: &Context) -> Result<(), LoanError>;
    fn flush_outputs(&mut self, _timestamp: FrameworkTime) {}
    fn discard_outputs(&mut self) {}
}

impl<F> Callback for F
where
    F: FnMut(FrameworkTime) -> Result<(), LoanError> + Send,
{
    fn run(&mut self, context: &Context) -> Result<(), LoanError> {
        self(context.now())
    }
}

/// Update inputs, check required inputs, execute, and publish successful outputs.
/// Returns false when required inputs are missing. Error/panic paths discard
/// pending outputs. Publications from earlier callbacks remain committed.
// TODO(port-deferred-commit): Separate preparation/run from output commit so simulation
// can commit completed batches in deterministic order; retain error/panic loan cleanup.
pub fn execute_callback(callback: &mut dyn Callback, context: &Context) -> Result<bool, LoanError> {
    struct Invocation<'a>(&'a mut dyn Callback);
    impl Drop for Invocation<'_> {
        fn drop(&mut self) {
            self.0.discard_outputs();
        }
    }
    let invocation = Invocation(callback);
    invocation.0.update_inputs();
    if !invocation.0.required_inputs_ready() {
        return Ok(false);
    }
    invocation.0.run(context)?;
    invocation.0.flush_outputs(context.now());
    Ok(true)
}

pub trait InputPorts {
    fn update(&self);
    fn set_waker(&mut self, wake: crate::wake::WakeHandle);
    fn has_pending(&self) -> bool;
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str));
}
pub trait OutputPorts {
    fn flush(&mut self, timestamp: FrameworkTime);
    fn discard(&mut self);
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str));
}
impl<T> InputPorts for Subscriber<'_, T> {
    fn set_waker(&mut self, wake: crate::wake::WakeHandle) {
        Subscriber::set_waker(self, wake);
    }
    fn has_pending(&self) -> bool {
        Subscriber::has_pending(self)
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        visit(self.channel_name());
    }
    fn update(&self) {
        Subscriber::update(self);
    }
}
impl<T> OutputPorts for Publisher<'_, T> {
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        visit(self.channel_name());
    }
    fn flush(&mut self, timestamp: FrameworkTime) {
        Publisher::flush(self, timestamp);
    }
    fn discard(&mut self) {
        self.discard_pending();
    }
}
impl InputPorts for () {
    fn set_waker(&mut self, _wake: crate::wake::WakeHandle) {}
    fn has_pending(&self) -> bool {
        false
    }
    fn visit_channel_names(&self, _visit: &mut dyn FnMut(&str)) {}
    fn update(&self) {}
}
impl OutputPorts for () {
    fn visit_channel_names(&self, _visit: &mut dyn FnMut(&str)) {}
    fn flush(&mut self, _timestamp: FrameworkTime) {}
    fn discard(&mut self) {}
}
impl<A: InputPorts, B: InputPorts> InputPorts for (A, B) {
    fn set_waker(&mut self, wake: crate::wake::WakeHandle) {
        self.0.set_waker(wake.clone());
        self.1.set_waker(wake);
    }
    fn has_pending(&self) -> bool {
        self.0.has_pending() || self.1.has_pending()
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        self.0.visit_channel_names(visit);
        self.1.visit_channel_names(visit);
    }
    fn update(&self) {
        self.0.update();
        self.1.update();
    }
}
impl<A: OutputPorts, B: OutputPorts> OutputPorts for (A, B) {
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        self.0.visit_channel_names(visit);
        self.1.visit_channel_names(visit);
    }
    fn flush(&mut self, timestamp: FrameworkTime) {
        self.0.flush(timestamp);
        self.1.flush(timestamp);
    }
    fn discard(&mut self) {
        self.0.discard();
        self.1.discard();
    }
}

/// Hand-written callback with typed IO bundles. The body decides how to handle
/// empty inputs; macro-generated required inputs additionally gate execution.
pub struct CallbackNode<I, O, F> {
    inputs: I,
    outputs: O,
    body: F,
}

impl<I, O, F> CallbackNode<I, O, F> {
    pub fn new(inputs: I, outputs: O, body: F) -> Self {
        Self {
            inputs,
            outputs,
            body,
        }
    }
}

impl<I, O, F> Callback for CallbackNode<I, O, F>
where
    I: InputPorts + Send,
    O: OutputPorts + Send,
    F: FnMut(&I, &mut O, &Context) -> Result<(), LoanError> + Send,
{
    fn set_waker(&mut self, wake: crate::wake::WakeHandle) {
        self.inputs.set_waker(wake);
    }
    fn has_pending_inputs(&self) -> bool {
        self.inputs.has_pending()
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        self.inputs.visit_channel_names(visit);
        self.outputs.visit_channel_names(visit);
    }
    fn update_inputs(&mut self) {
        self.inputs.update();
    }
    fn run(&mut self, context: &Context) -> Result<(), LoanError> {
        (self.body)(&self.inputs, &mut self.outputs, context)
    }
    fn flush_outputs(&mut self, timestamp: FrameworkTime) {
        self.outputs.flush(timestamp);
    }
    fn discard_outputs(&mut self) {
        self.outputs.discard();
    }
}
