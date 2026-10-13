//! Registered port metadata and endpoint wiring.
use crate::{BuildError, GraphPlan};
use task::recording::Transport;

#[derive(Clone)]
pub struct PlannedPort {
    configured: bool,
    pub forwardable: bool,
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
            configured: false,
            forwardable: false,
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
impl GraphPlan {
    /// Registered task names, including tasks without ports.
    pub fn task_names(&self) -> impl Iterator<Item = &str> {
        self.tasks.keys().map(String::as_str)
    }

    /// Ports in declaration order for a registered task.
    pub fn task_ports(&self, callback: &str) -> Result<&[PlannedPort], BuildError> {
        self.tasks
            .get(callback)
            .map(|range| &self.ports[range.clone()])
            .ok_or_else(|| BuildError(format!("unknown task '{callback}'")))
    }

    /// Whether this port already has an exclusive external wiring attachment.
    pub fn port_is_configured(
        &self,
        callback: &str,
        ordinal: usize,
        output: bool,
    ) -> Result<bool, BuildError> {
        Ok(self.port(callback, ordinal, output)?.configured)
    }

    fn claim_port(
        &mut self,
        callback: &str,
        ordinal: usize,
        output: bool,
    ) -> Result<(), BuildError> {
        let range = self
            .tasks
            .get(callback)
            .ok_or_else(|| BuildError(format!("unknown task '{callback}'")))?
            .clone();
        let port = self.ports[range]
            .iter_mut()
            .filter(|port| port.output == output)
            .nth(ordinal)
            .ok_or_else(|| BuildError(format!("unknown port '{callback}' ordinal {ordinal}")))?;
        if port.configured {
            return Err(BuildError(format!(
                "port '{callback}' ordinal {ordinal} already configured"
            )));
        }
        port.configured = true;
        Ok(())
    }

    pub fn can_inject_input(&self, callback: &str, ordinal: usize) -> Result<bool, BuildError> {
        let port = self.port(callback, ordinal, false)?;
        Ok(self
            .channels
            .get(&port.channel)
            .is_some_and(|channel| channel.replay.is_some()))
    }

    pub fn can_observe_output(&self, callback: &str, ordinal: usize) -> Result<bool, BuildError> {
        let port = self.port(callback, ordinal, true)?;
        Ok(self
            .channels
            .get(&port.channel)
            .is_some_and(|channel| channel.capture.is_some()))
    }

    /// Replace one input's sources with an isolated serialized injection endpoint.
    /// Other subscribers on the channel retain their original wiring.
    pub fn inject_input(
        &mut self,
        callback: &str,
        ordinal: usize,
        capacity: usize,
    ) -> Result<super::replay::SourceBinding, BuildError> {
        if capacity == 0 || u32::try_from(capacity).is_err() {
            return Err(BuildError(
                "injection capacity must be in 1..=u32::MAX".into(),
            ));
        }
        let port = self.port(callback, ordinal, false)?;
        let declare = self
            .channels
            .get(&port.channel)
            .and_then(|channel| channel.replay)
            .ok_or_else(|| {
                BuildError(format!(
                    "channel '{}': no context-free decoder",
                    port.channel
                ))
            })?
            .inject_input;
        if port.configured {
            return Err(BuildError(format!(
                "port '{callback}' ordinal {ordinal} already configured"
            )));
        }
        let binding = declare(self, &port, capacity)?;
        self.claim_port(callback, ordinal, false)?;
        Ok(binding)
    }

    /// Attach a serialized observer to a particular publisher before binding tasks.
    pub fn observe_output(
        &mut self,
        callback: &str,
        ordinal: usize,
    ) -> Result<super::capture::OutputBinding, BuildError> {
        let port = self.port(callback, ordinal, true)?;
        let declare = self
            .channels
            .get(&port.channel)
            .and_then(|channel| channel.capture)
            .ok_or_else(|| BuildError(format!("channel '{}': no serializer", port.channel)))?
            .declare_output;
        if port.configured {
            return Err(BuildError(format!(
                "port '{callback}' ordinal {ordinal} already configured"
            )));
        }
        let binding = declare(self, &port)?;
        self.claim_port(callback, ordinal, true)?;
        Ok(binding)
    }

    pub fn port(
        &self,
        callback: &str,
        ordinal: usize,
        output: bool,
    ) -> Result<PlannedPort, BuildError> {
        self.tasks
            .get(callback)
            .and_then(|ports| {
                self.ports[ports.clone()]
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
    ) -> Result<task::SubscriberKey<T>, BuildError> {
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
    ) -> Result<task::PublisherKey<T>, BuildError> {
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
    ) -> Result<task::iox2::Iox2SubscriberKey<T>, BuildError> {
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
    ) -> Result<task::iox2::Iox2PublisherKey<T>, BuildError> {
        let port = self.port(callback, ordinal, true)?;
        if port.transport != Transport::Ipc {
            return Err(BuildError("expected IPC data output".into()));
        }
        Ok(self.ipc::<T>(&port.channel)?.publisher_key(port.index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use task::{
        SubscriberPolicy,
        loggable::{DeserializeError, Loggable, SerializeError},
        message::MessageHeader,
        time::FrameworkTime,
    };

    struct Value(u8);
    impl Loggable for Value {
        type Context<'a> = ();
        fn serialize(&self, writer: &mut dyn std::io::Write) -> Result<(), SerializeError> {
            writer.write_all(&[self.0])?;
            Ok(())
        }
        fn deserialize_with_ctx<'a>(bytes: &[u8], _: ()) -> Result<Self, DeserializeError>
        where
            Self: 'a,
        {
            Ok(Self(*bytes.first().ok_or("empty value")?))
        }
    }

    #[test]
    fn input_injection_is_port_local_and_can_only_be_attached_once() {
        let mut plan = GraphPlan::default();
        let publisher = plan.native::<Value>("data").unwrap().publisher(1);
        let first = plan
            .subscriber::<Value>("data", 2, SubscriberPolicy::default())
            .unwrap();
        let second = plan
            .subscriber::<Value>("data", 2, SubscriberPolicy::default())
            .unwrap();
        plan.tasks.insert("reader".into(), 0..2);
        plan.register_replay_native::<Value>("data").unwrap();
        assert!(plan.inject_input("reader", 0, 0).is_err());
        assert!(!plan.port_is_configured("reader", 0, false).unwrap());
        let source = plan.inject_input("reader", 0, 2).unwrap();
        let publisher_count = plan.topology().channels[0].publishers.len();
        assert!(plan.inject_input("reader", 0, 2).is_err());
        assert_eq!(
            plan.topology().channels[0].publishers.len(),
            publisher_count
        );

        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut source = source(&bindings).unwrap();
        let channel = bindings.native::<Value>("data").unwrap();
        let mut publisher = channel.take_publisher(&publisher).unwrap();
        let first = channel.take_subscriber(&first).unwrap();
        let second = channel.take_subscriber(&second).unwrap();
        let now = FrameworkTime::from_nanoseconds(1);
        publisher.publish(Value(3)).unwrap();
        publisher.flush(now);
        source.inject(MessageHeader::new(now), &[7]).unwrap();
        first.update();
        second.update();
        let received: Vec<_> = first
            .input_forwardable()
            .drain()
            .map(|message| message.message.0)
            .collect();
        assert_eq!(received, [7]);
        let received: Vec<_> = second
            .input_forwardable()
            .drain()
            .map(|message| message.message.0)
            .collect();
        assert_eq!(received, [3, 7]);
    }

    #[test]
    fn output_observation_is_publisher_local_and_preserves_delivery() {
        let mut plan = GraphPlan::default();
        let first = plan.publisher::<Value>("data", 1).unwrap();
        let second = plan.publisher::<Value>("data", 1).unwrap();
        plan.tasks.insert("writer".into(), 0..2);
        let input = plan.native::<Value>("data").unwrap().subscriber(2);
        plan.register_loggable_native::<Value>("data").unwrap();
        let capture = plan.observe_output("writer", 0).unwrap();
        assert!(plan.observe_output("writer", 0).is_err());
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut capture = capture(&bindings).unwrap();
        let channel = bindings.native::<Value>("data").unwrap();
        let mut first = channel.take_publisher(&first).unwrap();
        let mut second = channel.take_publisher(&second).unwrap();
        let input = channel.take_subscriber(&input).unwrap();
        let now = FrameworkTime::from_nanoseconds(1);
        first.publish(Value(3)).unwrap();
        first.flush(now);
        second.publish(Value(7)).unwrap();
        second.flush(now);
        let captured = capture.take().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].1, [3]);
        input.update();
        let received: Vec<_> = input
            .input_forwardable()
            .drain()
            .map(|message| message.message.0)
            .collect();
        assert_eq!(received, [3, 7]);
    }
}
