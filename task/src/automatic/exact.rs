//! Per-port planning metadata for isolated replay of named tasks.
use super::{
    BuildError, NamedBindings, NamedPlan, port_capture::PortCapture, replay::SerializedSource,
};
use crate::recording::{Direction, ExecutionDescriptor, Transport};
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct PlannedPort {
    pub name: Option<String>,
    pub channel: String,
    pub output: bool,
    pub transport: Transport,
    pub payload_type: &'static str,
    pub index: usize,
    pub capacity: usize,
}
impl PlannedPort {
    pub fn new<T>(
        channel: &str,
        output: bool,
        ipc: bool,
        event: bool,
        index: usize,
        capacity: usize,
    ) -> Self {
        Self {
            name: None,
            channel: channel.into(),
            output,
            transport: if event {
                Transport::Event
            } else if ipc {
                Transport::Ipc
            } else {
                Transport::Native
            },
            payload_type: std::any::type_name::<T>(),
            index,
            capacity,
        }
    }
}
pub(super) trait ExactFactory {
    fn bind<'a>(self: Box<Self>, bindings: &NamedBindings<'a>)
    -> Result<ExactPort<'a>, BuildError>;
}
pub enum ExactPort<'a> {
    Input(SerializedSource<'a>),
    Output(PortCapture),
}
/// A caller-supplied port binding, omitted from automatic declaration/binding.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExplicitReplayPort {
    callback: String,
    output: bool,
    ordinal: usize,
}
impl ExplicitReplayPort {
    pub fn input(callback: impl Into<String>, ordinal: usize) -> Self {
        Self {
            callback: callback.into(),
            output: false,
            ordinal,
        }
    }
    pub fn output(callback: impl Into<String>, ordinal: usize) -> Self {
        Self {
            callback: callback.into(),
            output: true,
            ordinal,
        }
    }
}
pub struct NamedExactPlan {
    ports: Vec<(String, usize, Box<dyn ExactFactory>)>,
}
impl NamedExactPlan {
    pub fn bind<'a>(
        self,
        bindings: &NamedBindings<'a>,
    ) -> Result<Vec<(String, usize, ExactPort<'a>)>, BuildError> {
        self.ports
            .into_iter()
            .map(|(name, ordinal, factory)| Ok((name, ordinal, factory.bind(bindings)?)))
            .collect()
    }
}
impl NamedPlan {
    pub(super) fn port(
        &self,
        callback: &str,
        ordinal: usize,
        output: bool,
    ) -> Result<PlannedPort, BuildError> {
        self.tasks
            .get(callback)
            .and_then(|ports| {
                ports
                    .iter()
                    .filter(|port| port.output == output)
                    .nth(ordinal)
            })
            .cloned()
            .ok_or_else(|| {
                BuildError(format!(
                    "unknown registered port '{callback}' ordinal {ordinal}"
                ))
            })
    }
    /// Typed key access for explicit per-port configuration alongside automatic planning.
    pub fn native_input_key<T: Send + Sync + 'static>(
        &mut self,
        callback: &str,
        ordinal: usize,
    ) -> Result<crate::SubscriberKey<T>, BuildError> {
        let port = self.port(callback, ordinal, false)?;
        if port.transport != Transport::Native {
            return Err(BuildError("expected native data input".into()));
        }
        Ok(self.native::<T>(&port.channel)?.subscriber_key(port.index))
    }
    pub fn native_output_key<T: Send + Sync + 'static>(
        &mut self,
        callback: &str,
        ordinal: usize,
    ) -> Result<crate::PublisherKey<T>, BuildError> {
        let port = self.port(callback, ordinal, true)?;
        if port.transport != Transport::Native {
            return Err(BuildError("expected native data output".into()));
        }
        Ok(self.native::<T>(&port.channel)?.publisher_key(port.index))
    }
    #[cfg(feature = "iceoryx2")]
    pub fn ipc_input_key<
        T: std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static,
    >(
        &mut self,
        callback: &str,
        ordinal: usize,
    ) -> Result<crate::iox2::Iox2SubscriberKey<T>, BuildError> {
        let port = self.port(callback, ordinal, false)?;
        if port.transport != Transport::Ipc {
            return Err(BuildError("expected IPC data input".into()));
        }
        Ok(self.ipc::<T>(&port.channel)?.subscriber_key(port.index))
    }
    #[cfg(feature = "iceoryx2")]
    pub fn ipc_output_key<
        T: std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static,
    >(
        &mut self,
        callback: &str,
        ordinal: usize,
    ) -> Result<crate::iox2::Iox2PublisherKey<T>, BuildError> {
        let port = self.port(callback, ordinal, true)?;
        if port.transport != Transport::Ipc {
            return Err(BuildError("expected IPC data output".into()));
        }
        Ok(self.ipc::<T>(&port.channel)?.publisher_key(port.index))
    }
    /// Validate recorded layout before declaring hydration publishers. Requires
    /// task instances registered via TaskRegistration; graph insertion order is irrelevant.
    pub fn declare_exact_replay(
        &mut self,
        descriptor: &ExecutionDescriptor,
    ) -> Result<NamedExactPlan, BuildError> {
        self.declare_exact_replay_with_explicit(descriptor, &Default::default())
    }
    pub fn declare_exact_replay_with_explicit(
        &mut self,
        descriptor: &ExecutionDescriptor,
        explicit: &std::collections::BTreeSet<ExplicitReplayPort>,
    ) -> Result<NamedExactPlan, BuildError> {
        if self.exact_planned {
            return Err(BuildError("exact replay has already been planned".into()));
        }
        if self.tasks.len() != descriptor.callbacks.len() {
            return Err(BuildError(
                "replay task count differs from descriptor".into(),
            ));
        }
        let mut requests = Vec::new();
        let mut remaining = explicit.clone();
        let mut names = std::collections::BTreeSet::new();
        for callback in &descriptor.callbacks {
            if !names.insert(&callback.name) {
                return Err(BuildError("duplicate recorded callback name".into()));
            }
            let ports = self.tasks.get(&callback.name).ok_or_else(|| {
                BuildError(format!("missing registered callback '{}'", callback.name))
            })?;
            let mut ordinals = [0, 0];
            let mut mapped = BTreeMap::new();
            for port in ports {
                let direction = usize::from(port.output);
                mapped.insert((port.output, ordinals[direction]), port);
                ordinals[direction] += 1;
            }
            if ports.len() != callback.endpoints.len() {
                return Err(BuildError(format!(
                    "endpoint count differs for '{}'",
                    callback.name
                )));
            }
            for expected in &callback.endpoints {
                let output = expected.direction == Direction::Published;
                let port = mapped.remove(&(output, expected.ordinal)).ok_or_else(|| {
                    BuildError(format!(
                        "unknown/duplicate port for '{}' ordinal {}",
                        callback.name, expected.ordinal
                    ))
                })?;
                if port.channel != expected.channel
                    || port.payload_type != expected.payload_type
                    || port.transport != expected.transport
                {
                    return Err(BuildError(format!(
                        "endpoint layout differs for '{}' ordinal {}",
                        callback.name, expected.ordinal
                    )));
                }
                if port.transport != Transport::Event {
                    if remaining.remove(&ExplicitReplayPort {
                        callback: callback.name.clone(),
                        output,
                        ordinal: expected.ordinal,
                    }) {
                        continue;
                    }
                    if !(if output {
                        self.captures.contains_key(&port.channel)
                    } else {
                        self.replay_sources.contains_key(&port.channel)
                    }) {
                        return Err(BuildError(format!(
                            "channel '{}': missing {} capability; use an explicit port binding",
                            port.channel,
                            if output {
                                "serialization"
                            } else {
                                "context-free decoding"
                            }
                        )));
                    }
                    requests.push((callback.name.clone(), expected.ordinal, port.clone()));
                }
            }
        }
        if !remaining.is_empty() {
            return Err(BuildError(format!(
                "explicit replay ports do not identify data endpoints: {remaining:?}"
            )));
        }
        let declarations = std::mem::take(&mut self.replay_sources);
        let outputs = std::mem::take(&mut self.captures);
        let result = (|| {
            let mut ports = Vec::new();
            for (name, ordinal, port) in requests {
                let factory = if port.output {
                    outputs[&port.channel].declare_output(self, &port)?
                } else {
                    declarations[&port.channel].declare_exact(self, &port)?
                };
                ports.push((name, ordinal, factory));
            }
            Ok(NamedExactPlan { ports })
        })();
        self.replay_sources = declarations;
        self.captures = outputs;
        if result.is_ok() {
            self.exact_planned = true;
        }
        result
    }
}
