use super::{Callback, ports::*};
use crate::{
    Context, LoanError,
    recording::{Direction, EndpointDescriptor, LoggedEvent, LoggedMessage},
    time::FrameworkTime,
    wake::WakeHandle,
};

/// A callback with typed endpoint collections and a statically dispatched body.
/// Collections may be individual endpoints, pairs, or structs implementing
/// [`Inputs`] and [`Outputs`]. The body can capture its user state by value.
/// Required native inputs are wrapped in [`Required`]; readiness is checked on
/// their prepared snapshots before the body constructs any input or output views.
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
    I: Inputs + Send,
    O: Outputs + Send,
    F: FnMut(&I, &mut O, &Context) -> Result<(), LoanError> + Send,
{
    fn run(&mut self, context: &Context) -> Result<(), LoanError> {
        (self.body)(&self.inputs, &mut self.outputs, context)
    }
    fn update_inputs(&mut self) {
        self.inputs.visit_mut(&mut Update);
    }
    fn finish_inputs(&mut self) {
        self.inputs.visit_mut(&mut Finish);
    }
    fn clear_replay_inputs(&mut self) -> Result<(), LoanError> {
        self.inputs.visit_mut(&mut Clear);
        Ok(())
    }
    fn set_waker(&mut self, wake: WakeHandle) {
        self.inputs.visit_mut(&mut Wake(wake));
    }
    fn has_pending_inputs(&self) -> bool {
        let mut pending = Pending(false);
        self.inputs.visit(&mut pending);
        pending.0
    }
    fn required_inputs_ready(&self) -> bool {
        let mut readiness = Readiness {
            available: false,
            ready: true,
        };
        self.inputs.visit(&mut readiness);
        readiness.ready
    }
    fn required_inputs_available(&self) -> bool {
        let mut readiness = Readiness {
            available: true,
            ready: true,
        };
        self.inputs.visit(&mut readiness);
        readiness.ready
    }
    fn flush_outputs(&mut self, timestamp: FrameworkTime) {
        self.outputs.visit_mut(&mut Flush(timestamp));
    }
    fn discard_outputs(&mut self) {
        self.outputs.visit_mut(&mut Discard);
    }
    fn outputs_are_local(&self) -> bool {
        let mut local = LocalOutputs(true);
        self.outputs.visit(&mut local);
        local.0
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        let mut names = Names(visit);
        self.inputs.visit(&mut names);
        self.outputs.visit(&mut names);
    }
    fn recording_endpoints(&self) -> Option<Vec<EndpointDescriptor>> {
        let mut descriptors = Descriptors {
            endpoints: Vec::new(),
            ordinal: 0,
        };
        self.inputs.visit(&mut descriptors);
        descriptors.ordinal = 0;
        self.outputs.visit(&mut descriptors);
        Some(descriptors.endpoints)
    }
    fn visit_prepared_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        self.inputs.visit(&mut Messages { visit, ordinal: 0 });
    }
    fn visit_pending_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        self.outputs.visit(&mut Messages { visit, ordinal: 0 });
    }
    fn visit_prepared_events(&self, visit: &mut dyn FnMut(LoggedEvent)) {
        self.inputs.visit(&mut Events { visit, ordinal: 0 });
    }
    fn visit_pending_events(&self, visit: &mut dyn FnMut(LoggedEvent)) {
        self.outputs.visit(&mut Events { visit, ordinal: 0 });
    }
    fn stage_replay_event(
        &mut self,
        ordinal: usize,
        event_id: usize,
        count: u64,
    ) -> Result<(), LoanError> {
        let mut stage = StageEvent {
            remaining: ordinal,
            event_id,
            count,
            result: None,
        };
        self.inputs.visit_mut(&mut stage);
        stage
            .result
            .unwrap_or_else(|| Err(LoanError::Transport("unknown replay event port".into())))
    }
    #[cfg(feature = "iceoryx2")]
    fn take_iox2_events(&mut self) -> Vec<crate::iox2::Iox2EventRegistration> {
        let mut registrations = Registrations(Vec::new());
        self.inputs.visit_mut(&mut registrations);
        registrations.0
    }
}

struct Update;
impl InputVisitorMut for Update {
    fn visit(&mut self, port: &mut impl InputEndpoint) {
        port.update();
    }
}
struct Finish;
impl InputVisitorMut for Finish {
    fn visit(&mut self, port: &mut impl InputEndpoint) {
        port.finish();
    }
}
struct Clear;
impl InputVisitorMut for Clear {
    fn visit(&mut self, port: &mut impl InputEndpoint) {
        port.clear();
    }
}
struct Wake(WakeHandle);
impl InputVisitorMut for Wake {
    fn visit(&mut self, port: &mut impl InputEndpoint) {
        port.set_waker(self.0.clone());
    }
}
struct Pending(bool);
impl InputVisitor for Pending {
    fn visit(&mut self, port: &impl InputEndpoint) {
        self.0 |= port.requests_execution();
    }
}
struct Readiness {
    available: bool,
    ready: bool,
}
impl InputVisitor for Readiness {
    fn visit(&mut self, port: &impl InputEndpoint) {
        self.ready &= if self.available {
            port.required_available()
        } else {
            port.required_ready()
        };
    }
}
struct Flush(FrameworkTime);
impl OutputVisitorMut for Flush {
    fn visit(&mut self, port: &mut impl OutputEndpoint) {
        port.flush(self.0);
    }
}
struct Discard;
impl OutputVisitorMut for Discard {
    fn visit(&mut self, port: &mut impl OutputEndpoint) {
        port.discard();
    }
}
struct LocalOutputs(bool);
impl OutputVisitor for LocalOutputs {
    fn visit(&mut self, port: &impl OutputEndpoint) {
        self.0 &= port.is_local();
    }
}
struct Names<'a>(&'a mut dyn FnMut(&str));
impl InputVisitor for Names<'_> {
    fn visit(&mut self, port: &impl InputEndpoint) {
        (self.0)(port.channel_name());
    }
}
impl OutputVisitor for Names<'_> {
    fn visit(&mut self, port: &impl OutputEndpoint) {
        (self.0)(port.channel_name());
    }
}
struct Descriptors {
    endpoints: Vec<EndpointDescriptor>,
    ordinal: usize,
}
impl Descriptors {
    fn push<P: Endpoint>(&mut self, port: &P, direction: Direction) {
        self.endpoints.push(EndpointDescriptor {
            ordinal: self.ordinal,
            direction,
            channel: port.channel_name().into(),
            transport: port.transport(),
            payload_type: std::any::type_name::<P::Payload>().into(),
            publisher_index: port.publisher_index(),
        });
        self.ordinal += 1;
    }
}
impl InputVisitor for Descriptors {
    fn visit(&mut self, port: &impl InputEndpoint) {
        self.push(port, Direction::Received);
    }
}
impl OutputVisitor for Descriptors {
    fn visit(&mut self, port: &impl OutputEndpoint) {
        self.push(port, Direction::Published);
    }
}
struct Messages<'a> {
    visit: &'a mut dyn FnMut(LoggedMessage),
    ordinal: usize,
}
impl InputVisitor for Messages<'_> {
    fn visit(&mut self, port: &impl InputEndpoint) {
        port.visit_headers(|header| {
            (self.visit)(LoggedMessage {
                ordinal: self.ordinal,
                header,
            })
        });
        self.ordinal += 1;
    }
}
impl OutputVisitor for Messages<'_> {
    fn visit(&mut self, port: &impl OutputEndpoint) {
        port.visit_headers(|header| {
            (self.visit)(LoggedMessage {
                ordinal: self.ordinal,
                header,
            })
        });
        self.ordinal += 1;
    }
}
struct Events<'a> {
    visit: &'a mut dyn FnMut(LoggedEvent),
    ordinal: usize,
}
impl InputVisitor for Events<'_> {
    fn visit(&mut self, port: &impl InputEndpoint) {
        port.visit_events(|event_id, count| {
            (self.visit)(LoggedEvent {
                ordinal: self.ordinal,
                event_id,
                count,
            })
        });
        self.ordinal += 1;
    }
}
impl OutputVisitor for Events<'_> {
    fn visit(&mut self, port: &impl OutputEndpoint) {
        port.visit_events(|event_id, count| {
            (self.visit)(LoggedEvent {
                ordinal: self.ordinal,
                event_id,
                count,
            })
        });
        self.ordinal += 1;
    }
}
struct StageEvent {
    remaining: usize,
    event_id: usize,
    count: u64,
    result: Option<Result<(), LoanError>>,
}
impl InputVisitorMut for StageEvent {
    fn visit(&mut self, port: &mut impl InputEndpoint) {
        if self.result.is_some() {
            return;
        }
        if self.remaining == 0 {
            self.result = Some(port.stage_event(self.event_id, self.count));
        } else {
            self.remaining -= 1;
        }
    }
}
#[cfg(feature = "iceoryx2")]
struct Registrations(Vec<crate::iox2::Iox2EventRegistration>);
#[cfg(feature = "iceoryx2")]
impl InputVisitorMut for Registrations {
    fn visit(&mut self, port: &mut impl InputEndpoint) {
        if let Some(registration) = port.take_registration() {
            self.0.push(registration);
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{
        ChannelPlan, Publisher, Subscriber, SubscriberPolicy,
        callback::execute_callback,
        context::testing,
        string_interner::{CallbackNameInterner, ChannelNameInterner},
    };
    use googletest::prelude::*;

    #[gtest]
    fn create_manual_node() {
        let mut small_number_channel_plan = ChannelPlan::<u8>::new("small_numbers");
        let small_number_subscriber_key = small_number_channel_plan.subscriber(5);
        let small_number_publisher_key = small_number_channel_plan.publisher(1);

        let mut big_number_channel_plans = ChannelPlan::<u64>::new("big_numbers");
        let big_number_publisher_key = big_number_channel_plans.publisher(1);
        let big_number_subscriber_key = big_number_channel_plans.subscriber(1);

        let small_number_channel_storage = {
            let storage = small_number_channel_plan.allocate();
            assert_that!(storage, ok(anything()));
            storage.unwrap()
        };

        let big_number_channel_storage = {
            let storage = big_number_channel_plans.allocate();
            assert_that!(storage, ok(anything()));
            storage.unwrap()
        };

        let small_number_channel_bindings = small_number_channel_storage.build();

        // Callback will use this
        let small_number_subscriber = {
            let subscriber =
                small_number_channel_bindings.take_subscriber(&small_number_subscriber_key);
            assert_that!(subscriber, ok(anything()));
            subscriber.unwrap()
        };

        // For injecting via test
        let mut small_number_publisher = {
            let publisher =
                small_number_channel_bindings.take_publisher(&small_number_publisher_key);
            assert_that!(publisher, ok(anything()));
            publisher.unwrap()
        };

        let big_number_channel_bindings = big_number_channel_storage.build();

        // Callback will use this
        let big_number_publisher = {
            let publisher = big_number_channel_bindings.take_publisher(&big_number_publisher_key);
            assert_that!(publisher, ok(anything()));
            publisher.unwrap()
        };

        // For listening to callback from test.
        let big_number_subscriber = {
            let subscriber =
                big_number_channel_bindings.take_subscriber(&big_number_subscriber_key);
            assert_that!(subscriber, ok(anything()));
            subscriber.unwrap()
        };

        let required_subscriber = Required(small_number_subscriber);

        let body = |inputs: &Required<Subscriber<'_, u8>>,
                    outputs: &mut Publisher<'_, u64>,
                    _: &Context|
         -> Result<(), LoanError> {
            let number = inputs.0.input_readonly().value().copied();
            assert_that!(number, some(anything()));

            outputs.publish(u64::from(number.unwrap()))?;
            Ok(())
        };

        let mut callback_node = CallbackNode::new(required_subscriber, big_number_publisher, body);

        let now = FrameworkTime::from_nanoseconds(42);
        small_number_publisher.publish(7).unwrap();
        small_number_publisher.flush(now);

        let ran = execute_callback(
            &mut callback_node,
            &testing::ContextBuilder::with_time(now).build(),
        )
        .unwrap();
        expect_true!(ran);

        big_number_subscriber.update();
        expect_that!(
            big_number_subscriber
                .input_forwardable()
                .pop()
                .unwrap()
                .message,
            eq(7)
        );
    }
    #[gtest]
    fn manual_node_preserves_readiness_recording_and_cancellation_across_heterogeneous_ports() {
        let mut numbers = ChannelPlan::<u32>::new("numbers");
        let number_out = numbers.publisher(1);
        let number_in = numbers.subscriber_with_policy(
            1,
            SubscriberPolicy {
                trigger: true,
                keep_across_runs: false,
            },
        );
        let mut labels = ChannelPlan::<String>::new("labels");
        let label_out = labels.publisher(1);
        let label_in = labels.subscriber(1);
        let mut results = ChannelPlan::<u64>::new("results");
        let result_out = results.publisher(1);
        let result_in = results.subscriber(1);
        let numbers = numbers.allocate().unwrap();
        let labels = labels.allocate().unwrap();
        let results = results.allocate().unwrap();
        let numbers = numbers.build();
        let labels = labels.build();
        let results = results.build();
        let mut number_out = numbers.take_publisher(&number_out).unwrap();
        let mut label_out = labels.take_publisher(&label_out).unwrap();
        let result_in = results.take_subscriber(&result_in).unwrap();
        let mut fail = true;
        let mut node = CallbackNode::new(
            (
                Required(numbers.take_subscriber(&number_in).unwrap()),
                labels.take_subscriber(&label_in).unwrap(),
            ),
            results.take_publisher(&result_out).unwrap(),
            move |inputs: &(Required<Subscriber<'_, u32>>, Subscriber<'_, String>),
                  output: &mut Publisher<'_, u64>,
                  _: &Context| {
                let number = inputs.0.0.input_readonly().value().copied().unwrap();
                output.publish(u64::from(number))?;
                if std::mem::take(&mut fail) {
                    return Err(LoanError::LoanCapacityReached);
                }
                Ok(())
            },
        );
        let descriptors = node.recording_endpoints().unwrap();
        assert_eq!(
            descriptors
                .iter()
                .map(|p| (p.channel.as_str(), p.ordinal))
                .collect::<Vec<_>>(),
            [("numbers", 0), ("labels", 1), ("results", 0)]
        );
        assert_eq!(descriptors[0].payload_type, std::any::type_name::<u32>());
        assert_eq!(descriptors[1].payload_type, std::any::type_name::<String>());
        assert_eq!(descriptors[2].direction, Direction::Published);
        assert!(node.outputs_are_local());
        assert!(node.stage_replay_event(0, 1, 1).is_err());
        let now = FrameworkTime::from_nanoseconds(10);
        node.update_inputs();
        number_out.publish(42).unwrap();
        number_out.flush(now);
        label_out.publish("label".into()).unwrap();
        label_out.flush(now);
        assert!(node.required_inputs_available());
        assert!(!node.required_inputs_ready());
        node.update_inputs();
        let mut records = Vec::new();
        node.visit_prepared_messages(&mut |record| records.push(record));
        assert_eq!(
            records.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
            [0, 1]
        );
        assert!(records.iter().all(|r| r.header.published_at == now));
        node.clear_replay_inputs().unwrap();
        assert!(!node.required_inputs_available());
        assert!(!node.required_inputs_ready());
        let names = ChannelNameInterner::new();
        let callbacks = CallbackNameInterner::new();
        let context = Context::new(now, &names, &callbacks);
        assert!(!execute_callback(&mut node, &context).unwrap());
        number_out.publish(7).unwrap();
        number_out.flush(now);
        assert!(execute_callback(&mut node, &context).is_err());
        result_in.update();
        assert!(result_in.is_empty());
        assert!(!node.required_inputs_ready());
        node.visit_pending_messages(&mut |_| panic!("failed invocation retained an output"));
        number_out.publish(9).unwrap();
        number_out.flush(now);
        assert!(execute_callback(&mut node, &context).unwrap());
        result_in.update();
        assert_eq!(result_in.input_forwardable().pop().unwrap().message, 9);
    }
}
