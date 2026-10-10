//! Owned, deterministic graph inspection before or after storage allocation.
use crate::recording::Transport;

#[derive(Clone, Debug)]
pub struct ChannelTopology {
    pub name: String,
    pub payload_type: &'static str,
    pub transport: Transport,
    /// Per-publisher loan capacities, in declaration order.
    pub publishers: Vec<usize>,
    /// Per-subscriber retained window capacities, in declaration order.
    pub subscribers: Vec<usize>,
    /// Allowed publisher indices for each subscriber; None accepts every source.
    pub sources: Vec<Option<Vec<usize>>>,
}
#[derive(Clone, Debug)]
pub struct PortTopology {
    pub callback: String,
    pub port: String,
    pub ordinal: usize,
    pub output: bool,
    pub channel: String,
    pub transport: Transport,
    pub payload_type: &'static str,
    pub capacity: usize,
    pub endpoint_index: usize,
}
#[derive(Clone, Debug, Default)]
pub struct Topology {
    pub callbacks: Vec<String>,
    pub channels: Vec<ChannelTopology>,
    pub ports: Vec<PortTopology>,
}
impl std::fmt::Display for Topology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for callback in &self.callbacks {
            writeln!(f, "callback {callback:?}")?;
        }
        for channel in &self.channels {
            writeln!(
                f,
                "channel {:?}: {:?} {} publishers={:?} subscribers={:?} sources={:?}",
                channel.name,
                channel.transport,
                channel.payload_type,
                channel.publishers,
                channel.subscribers,
                channel.sources
            )?;
        }
        for port in &self.ports {
            writeln!(
                f,
                "  {}.{} [{} {}] {} {:?}, capacity={}, endpoint={}",
                port.callback,
                port.port,
                if port.output { "output" } else { "input" },
                port.ordinal,
                if port.output { "->" } else { "<-" },
                port.channel,
                port.capacity,
                port.endpoint_index
            )?;
        }
        Ok(())
    }
}
impl Topology {
    /// Render without side effects. Quoted DOT IDs keep arbitrary names distinct.
    pub fn dot(&self) -> String {
        use std::fmt::Write;
        let mut text = String::from("digraph workload {\n");
        for callback in &self.callbacks {
            writeln!(
                text,
                "  {:?} [shape=box,label={callback:?}];",
                format!("task:{callback}")
            )
            .unwrap();
        }
        for channel in &self.channels {
            writeln!(
                text,
                "  {:?} [shape=ellipse,label={:?}];",
                format!("channel:{}", channel.name),
                format!(
                    "{}\n{:?} {}",
                    channel.name, channel.transport, channel.payload_type
                )
            )
            .unwrap();
        }
        for port in &self.ports {
            let task = format!("task:{}", port.callback);
            let channel = format!("channel:{}", port.channel);
            let (from, to) = if port.output {
                (&task, &channel)
            } else {
                (&channel, &task)
            };
            writeln!(
                text,
                "  {from:?} -> {to:?} [label={:?}];",
                format!(
                    "{} [{}] capacity={}",
                    port.port, port.ordinal, port.capacity
                )
            )
            .unwrap();
        }
        text.push_str("}\n");
        text
    }
}
