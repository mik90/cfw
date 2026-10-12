use crate::time::FrameworkTime;
use crate::{Context, LoanError, Publisher, Subscriber};

/// Callback body and IO lifecycle, shared by live and deterministic executors.
/// Executors invoke `execute_callback` or `execute_callback_batch` rather than
/// calling `run` directly.
pub trait Callback: Send {
    /// Isolate framework-managed outputs from live channel delivery.
    fn enable_exact_replay(&mut self) -> Result<(), LoanError> {
        Err(LoanError::Transport(
            "callback does not support exact replay".into(),
        ))
    }
    /// Clear prepared and incoming snapshots before exact-replay hydration.
    fn clear_replay_inputs(&mut self) -> Result<(), LoanError> {
        Err(LoanError::Transport(
            "callback does not support replay input reset".into(),
        ))
    }
    fn stage_replay_event(
        &mut self,
        _ordinal: usize,
        _event_id: usize,
        _count: u64,
    ) -> Result<(), LoanError> {
        Err(LoanError::Transport("unknown replay event port".into()))
    }
    fn recording_endpoints(&self) -> Option<Vec<crate::recording::EndpointDescriptor>> {
        None
    }
    fn visit_prepared_messages(&self, _visit: &mut dyn FnMut(crate::recording::LoggedMessage)) {}
    fn visit_pending_messages(&self, _visit: &mut dyn FnMut(crate::recording::LoggedMessage)) {}
    fn visit_prepared_events(&self, _visit: &mut dyn FnMut(crate::recording::LoggedEvent)) {}
    fn visit_pending_events(&self, _visit: &mut dyn FnMut(crate::recording::LoggedEvent)) {}
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
    /// Preflight readiness may include incoming queues. It must not consume them.
    fn required_inputs_available(&self) -> bool {
        self.required_inputs_ready()
    }
    /// Readiness of the prepared read buffers, excluding later publications.
    fn required_inputs_ready(&self) -> bool {
        true
    }
    fn run(&mut self, context: &Context) -> Result<(), LoanError>;
    fn finish_inputs(&mut self) {}
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
pub fn execute_callback(callback: &mut dyn Callback, context: &Context) -> Result<bool, LoanError> {
    let Some(prepared) = prepare_callback(callback) else {
        return Ok(false);
    };
    prepared.run(context)?.commit();
    Ok(true)
}

pub struct Invocation<'a> {
    callback: &'a mut dyn Callback,
    ran: bool,
}
impl Drop for Invocation<'_> {
    fn drop(&mut self) {
        self.callback.discard_outputs();
        if self.ran {
            self.callback.finish_inputs();
        }
    }
}

/// Exclusive, prepared input snapshot. Dropping it cancels pending outputs.
#[must_use]
pub struct PreparedCallback<'a>(Invocation<'a>);

/// Completed body with unpublished outputs. Drop cancels; commit publishes at
/// the invocation timestamp. The callback cannot be reused while this exists.
#[must_use]
pub struct CompletedCallback<'a> {
    invocation: Invocation<'a>,
    timestamp: FrameworkTime,
}

pub fn prepare_callback(callback: &mut dyn Callback) -> Option<PreparedCallback<'_>> {
    let invocation = Invocation {
        callback,
        ran: false,
    };
    invocation.callback.update_inputs();
    invocation
        .callback
        .required_inputs_ready()
        .then_some(PreparedCallback(invocation))
}
impl<'a> PreparedCallback<'a> {
    pub fn run(mut self, context: &Context) -> Result<CompletedCallback<'a>, LoanError> {
        self.0.ran = true;
        self.0.callback.run(context)?;
        Ok(CompletedCallback {
            invocation: self.0,
            timestamp: context.now(),
        })
    }
}
impl CompletedCallback<'_> {
    pub fn commit(self) {
        self.invocation.callback.flush_outputs(self.timestamp);
    }
}

pub trait InputPorts {
    fn update(&self);
    fn finish(&self);
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
    fn finish(&self) {
        self.finish_iteration();
    }
    fn set_waker(&mut self, wake: crate::wake::WakeHandle) {
        Subscriber::set_waker(self, wake);
    }
    fn has_pending(&self) -> bool {
        self.requests_execution()
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
    fn finish(&self) {}
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
    fn finish(&self) {
        self.0.finish();
        self.1.finish();
    }
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
pub struct CallbackNode<InputsType, OutputsType, BodyFunc> {
    inputs: InputsType,
    outputs: OutputsType,
    body: BodyFunc,
}

impl<InputsType, OutputsType, BodyFunc> CallbackNode<InputsType, OutputsType, BodyFunc> {
    pub fn new(inputs: InputsType, outputs: OutputsType, body: BodyFunc) -> Self {
        Self {
            inputs,
            outputs,
            body,
        }
    }
}

impl<InputsType, OutputsType, BodyFunc> Callback for CallbackNode<InputsType, OutputsType, BodyFunc>
where
    InputsType: InputPorts + Send,
    OutputsType: OutputPorts + Send,
    BodyFunc: FnMut(&InputsType, &mut OutputsType, &Context) -> Result<(), LoanError> + Send,
{
    fn finish_inputs(&mut self) {
        self.inputs.finish();
    }
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
