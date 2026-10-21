//! Owned, deterministic graph inspection before or after storage allocation.
use task::recording::Transport;

pub use task::channel::ChannelTopology;
#[derive(Clone, Debug)]
pub struct EndpointTopology {
    pub forwardable: bool,
    pub callback: String,
    pub name: String,
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
    pub forwarding: Vec<crate::bundle::ForwardingEdge>,
    pub callbacks: Vec<String>,
    pub channels: Vec<ChannelTopology>,
    pub endpoints: Vec<EndpointTopology>,
}
impl std::fmt::Display for Topology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for edge in &self.forwarding {
            writeln!(
                f,
                "forward {}.{} -> {}.{}: {} -> {}",
                edge.callback,
                edge.input,
                edge.output,
                edge.field,
                edge.source_channel,
                edge.output_channel
            )?;
        }
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
            writeln!(
                f,
                "  forwarding retention per publisher: {}",
                channel.forwarding_retention_per_publisher
            )?;
        }
        for endpoint in &self.endpoints {
            writeln!(
                f,
                "  {}.{} [{} {}] {} {:?}, capacity={}, endpoint={}, forwardable={}",
                endpoint.callback,
                endpoint.name,
                if endpoint.output { "output" } else { "input" },
                endpoint.ordinal,
                if endpoint.output { "->" } else { "<-" },
                endpoint.channel,
                endpoint.capacity,
                endpoint.endpoint_index,
                endpoint.forwardable
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
        for edge in &self.forwarding {
            writeln!(
                text,
                "  {:?} -> {:?} [style=dashed,label={:?}];",
                format!("channel:{}", edge.source_channel),
                format!("channel:{}", edge.output_channel),
                format!("{}.{}.{}", edge.callback, edge.output, edge.field)
            )
            .unwrap();
        }
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
        for endpoint in &self.endpoints {
            let task = format!("task:{}", endpoint.callback);
            let channel = format!("channel:{}", endpoint.channel);
            let (from, to) = if endpoint.output {
                (&task, &channel)
            } else {
                (&channel, &task)
            };
            writeln!(
                text,
                "  {from:?} -> {to:?} [label={:?}];",
                format!(
                    "{} [{}] capacity={} forwardable={}",
                    endpoint.name, endpoint.ordinal, endpoint.capacity, endpoint.forwardable
                )
            )
            .unwrap();
        }
        text.push_str("}\n");
        text
    }
}
