use std::any::{Any, TypeId};
use std::collections::HashMap;

use task::callback_storage::CallbackStorage;

use super::{SimulationState, StepError};
use crate::FrameworkTime;

#[cfg(test)]
mod tests;

/// Error while adding a synthetic iox2 input to a simulation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Iox2InputError {
    /// The graph has no iox2 data subscriber for the requested channel.
    UnknownDataChannel(String),
    /// The requested data channel carries a different payload type.
    PayloadTypeMismatch(String),
    /// The graph has no iox2 event subscriber for the requested channel.
    UnknownEventChannel(String),
}

impl std::fmt::Display for Iox2InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDataChannel(channel) => {
                write!(f, "no iox2 data input is registered for channel {channel}")
            }
            Self::PayloadTypeMismatch(channel) => {
                write!(f, "iox2 data payload type does not match channel {channel}")
            }
            Self::UnknownEventChannel(channel) => {
                write!(f, "no iox2 event input is registered for channel {channel}")
            }
        }
    }
}

impl std::error::Error for Iox2InputError {}

struct ScheduledIox2Input {
    at: FrameworkTime,
    sequence: u64,
    kind: ScheduledIox2InputKind,
}

enum ScheduledIox2InputKind {
    Data {
        channel: String,
        payload: Box<dyn Any + Send>,
    },
    Event {
        channel: String,
        event_id: iceoryx2::prelude::EventId,
        count: u64,
    },
}

impl ScheduledIox2Input {
    fn order_key(&self) -> (FrameworkTime, u64) {
        (self.at, self.sequence)
    }
}

type SyntheticPublisherMap = HashMap<String, Box<dyn task::iox2::Iox2SyntheticPublisher>>;

type SimulationIox2Registrations = (
    Vec<task::iox2::Iox2EventRegistration>,
    SyntheticPublisherMap,
);

fn register_simulation_iox2_inputs(
    nodes: &CallbackStorage,
    context: &mut task::iox2::Iox2Context,
) -> Result<SimulationIox2Registrations, StepError> {
    let mut event_listeners = Vec::new();
    let mut input_publishers = HashMap::new();
    for node_handle in nodes.iter_shared() {
        let result = node_handle.access(|node| {
            let node_name = node.name().to_owned();
            let mut error: Option<StepError> = None;
            node.callback_mut()
                .for_each_subscriber_mut(&mut |subscriber| {
                    if error.is_some() {
                        return;
                    }
                    match subscriber.iox2_take_event_registration(context) {
                        Ok(Some(registration)) => event_listeners.push(registration),
                        Ok(None) => {}
                        Err(reason) => {
                            error =
                                Some(StepError::Iox2Event(format!("node {node_name}: {reason}")))
                        }
                    }

                    if error.is_some() {
                        return;
                    }
                    let channel = subscriber.config().channel_name.clone();
                    if input_publishers.contains_key(&channel) {
                        return;
                    }
                    match subscriber.iox2_create_simulation_publisher(context) {
                        Ok(Some(publisher)) => {
                            input_publishers.insert(channel, publisher);
                        }
                        Ok(None) => {}
                        Err(reason) => {
                            error =
                                Some(StepError::Iox2Input(format!("node {node_name}: {reason}")))
                        }
                    }
                });
            error.map_or(Ok(()), Err)
        });
        result?;
    }
    Ok((event_listeners, input_publishers))
}

pub(super) struct Iox2SimulationState {
    event_listeners: Vec<task::iox2::Iox2EventRegistration>,
    iox2_input_publishers: SyntheticPublisherMap,
    scheduled_iox2_inputs: Vec<ScheduledIox2Input>,
    next_input_sequence: u64,
    context: Option<task::iox2::Iox2Context>,
}

impl Iox2SimulationState {
    pub(super) fn new(
        nodes: &CallbackStorage,
        mut context: Option<task::iox2::Iox2Context>,
    ) -> Result<Self, StepError> {
        let (event_listeners, iox2_input_publishers) = if let Some(context) = context.as_mut() {
            register_simulation_iox2_inputs(nodes, context)?
        } else {
            (Vec::new(), HashMap::new())
        };
        Ok(Self {
            event_listeners,
            iox2_input_publishers,
            scheduled_iox2_inputs: Vec::new(),
            next_input_sequence: 0,
            context,
        })
    }

    pub(super) fn dispatch_due_inputs(&mut self, now: FrameworkTime) -> Result<(), StepError> {
        while self
            .scheduled_iox2_inputs
            .first()
            .is_some_and(|input| input.at <= now)
        {
            let input = self.scheduled_iox2_inputs.remove(0);
            match input.kind {
                ScheduledIox2InputKind::Data { channel, payload } => {
                    let publisher =
                        self.iox2_input_publishers
                            .get_mut(&channel)
                            .ok_or_else(|| {
                                StepError::Iox2Input(format!(
                                    "simulation publisher for channel {channel} disappeared"
                                ))
                            })?;
                    publisher
                        .publish(task::message::MessageHeader::new(input.at), payload)
                        .map_err(|error| StepError::Iox2Input(error.to_string()))?;
                }
                ScheduledIox2InputKind::Event {
                    channel,
                    event_id,
                    count,
                } => {
                    for registration in self
                        .event_listeners
                        .iter()
                        .filter(|registration| registration.channel == channel)
                    {
                        let _ = registration
                            .staging
                            .push(task::iox2::EventRecord { event_id, count });
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn poll_listeners(&mut self) -> Result<(), StepError> {
        use task::iox2::EventRecord;
        for registration in &mut self.event_listeners {
            let mut folded: Vec<EventRecord> = Vec::new();
            registration
                .listener
                .try_wait(|activation| {
                    if let Some(record) = folded.iter_mut().find(|r| r.event_id == activation.id) {
                        record.count = record.count.saturating_add(activation.count);
                    } else {
                        folded.push(EventRecord {
                            event_id: activation.id,
                            count: activation.count,
                        });
                    }
                })
                .map_err(|error| StepError::Iox2Event(error.to_string()))?;
            for record in folded {
                let _ = registration.staging.push(record);
            }
        }
        Ok(())
    }

    pub(super) fn next_input_time(&self, now: FrameworkTime) -> Option<FrameworkTime> {
        self.scheduled_iox2_inputs
            .first()
            .map(|input| input.at)
            .filter(|&time| time > now)
    }

    fn push_scheduled_iox2_input(&mut self, at: FrameworkTime, kind: ScheduledIox2InputKind) {
        let sequence = self.next_input_sequence;
        self.next_input_sequence = self.next_input_sequence.saturating_add(1);
        self.scheduled_iox2_inputs
            .push(ScheduledIox2Input { at, sequence, kind });
        self.scheduled_iox2_inputs
            .sort_by_key(ScheduledIox2Input::order_key);
    }
}

impl SimulationState {
    /// Schedule an owned iox2 payload for delivery at a simulation-time boundary.
    ///
    /// Delivery uses a simulation-owned publisher and the graph's real IPC service. The message
    /// header records `at`; a request scheduled in the past is delivered at the next step without
    /// rewinding simulation time. Data inputs remain non-triggering.
    pub fn schedule_iox2_data<T>(
        &mut self,
        at: FrameworkTime,
        channel: &str,
        payload: T,
    ) -> Result<(), Iox2InputError>
    where
        T: std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static,
    {
        let publisher = self
            .iox2
            .iox2_input_publishers
            .get(channel)
            .ok_or_else(|| Iox2InputError::UnknownDataChannel(channel.to_owned()))?;
        if publisher.payload_type_id() != TypeId::of::<T>() {
            return Err(Iox2InputError::PayloadTypeMismatch(channel.to_owned()));
        }
        self.iox2.push_scheduled_iox2_input(
            at,
            ScheduledIox2InputKind::Data {
                channel: channel.to_owned(),
                payload: Box::new(payload),
            },
        );
        Ok(())
    }

    /// Schedule an event activation for the named channel and simulation time.
    ///
    /// Every registered event subscriber on the channel receives one staged record carrying the
    /// full `count`; the count is not expanded into individual notifications. A past timestamp is
    /// delivered at the next step without rewinding simulation time.
    pub fn schedule_iox2_event(
        &mut self,
        at: FrameworkTime,
        channel: &str,
        event_id: iceoryx2::prelude::EventId,
        count: u64,
    ) -> Result<(), Iox2InputError> {
        if !self
            .iox2
            .event_listeners
            .iter()
            .any(|registration| registration.channel == channel)
        {
            return Err(Iox2InputError::UnknownEventChannel(channel.to_owned()));
        }
        self.iox2.push_scheduled_iox2_input(
            at,
            ScheduledIox2InputKind::Event {
                channel: channel.to_owned(),
                event_id,
                count,
            },
        );
        Ok(())
    }
}
