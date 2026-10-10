//! Shared streaming input selection for simulated and wall-clock-paced replay.
use crate::{BoxedLogError, OwnedLogEntry, ReplaySource, SortedLogStreamReader};
use std::collections::{BTreeMap, HashSet};
use task::{recording::*, time::FrameworkTime};
enum Pending {
    Data(OwnedLogEntry),
    Event {
        callback: String,
        channel: String,
        event: ObservedEvent,
    },
}
impl Pending {
    fn time(&self) -> FrameworkTime {
        match self {
            Self::Data(e) => e.header.published_at,
            Self::Event { event, .. } => event.observed_at,
        }
    }
}
pub struct ReplayFeed<'a> {
    reader: SortedLogStreamReader,
    sources: BTreeMap<String, ReplaySource<'a>>,
    descriptor: Option<ExecutionDescriptor>,
    denied: HashSet<String>,
    pending: Option<Pending>,
    exhausted: bool,
    failed: bool,
    last_time: Option<FrameworkTime>,
}
impl<'a> ReplayFeed<'a> {
    pub fn new(
        reader: SortedLogStreamReader,
        sources: impl IntoIterator<Item = ReplaySource<'a>>,
        denied: HashSet<String>,
    ) -> Result<Self, BoxedLogError> {
        let mut map = BTreeMap::new();
        for source in sources {
            let name = source.channel().to_owned();
            if name == EXECUTION_LOG_CHANNEL || name == EXECUTION_EVENT_CHANNEL {
                return Err(format!("reserved replay source channel '{name}'").into());
            }
            if map.insert(name.clone(), source).is_some() {
                return Err(format!("duplicate replay source for '{name}'").into());
            }
        }
        for channel in reader.channel_names() {
            if channel != EXECUTION_LOG_CHANNEL
                && channel != EXECUTION_EVENT_CHANNEL
                && !denied.contains(channel)
                && !map.contains_key(channel)
            {
                return Err(format!(
                    "no replay source for channel '{channel}' (add a source or explicitly deny it)"
                )
                .into());
            }
        }
        let descriptor = reader
            .artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT)
            .map(serde_json::from_slice::<ExecutionDescriptor>)
            .transpose()?;
        if let Some(descriptor) = &descriptor {
            let mut names = HashSet::new();
            for callback in &descriptor.callbacks {
                if !names.insert(&callback.name) {
                    return Err(
                        format!("duplicate recorded callback name '{}'", callback.name).into(),
                    );
                }
                let mut ports = HashSet::new();
                for port in &callback.endpoints {
                    if !ports.insert((port.direction == Direction::Received, port.ordinal)) {
                        return Err(
                            format!("duplicate endpoint ordinal on '{}'", callback.name).into()
                        );
                    }
                }
            }
        }
        let mut feed = Self {
            reader,
            sources: map,
            descriptor,
            denied,
            pending: None,
            exhausted: false,
            failed: false,
            last_time: None,
        };
        feed.advance(&mut || true)?;
        Ok(feed)
    }
    pub fn next_time(&self) -> Option<FrameworkTime> {
        self.pending.as_ref().map(Pending::time)
    }
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }
    pub fn last_time(&self) -> Option<FrameworkTime> {
        self.last_time
    }
    fn advance(&mut self, running: &mut impl FnMut() -> bool) -> Result<(), BoxedLogError> {
        self.pending = None;
        while running() {
            let Some(entry) = self.reader.next_entry()? else {
                self.exhausted = true;
                break;
            };
            self.last_time = Some(entry.header.published_at);
            if self.denied.contains(&entry.channel_name) {
                continue;
            }
            match entry.channel_name.as_str() {
                EXECUTION_LOG_CHANNEL => {
                    let record: ExecutionRecord = serde_json::from_slice(&entry.serialized_body)?;
                    let descriptor = self
                        .descriptor
                        .as_ref()
                        .ok_or("execution record has no descriptor")?;
                    if record.callback_index >= descriptor.callbacks.len()
                        || record.execution_time != entry.header.published_at
                    {
                        return Err("invalid execution record index or timestamp".into());
                    }
                }
                EXECUTION_EVENT_CHANNEL => {
                    let event: ObservedEvent = serde_json::from_slice(&entry.serialized_body)?;
                    if event.observed_at != entry.header.published_at {
                        return Err("event timestamp differs from log header".into());
                    }
                    let callback = self
                        .descriptor
                        .as_ref()
                        .and_then(|d| d.callbacks.get(event.callback_index))
                        .ok_or("event record has no matching callback descriptor")?;
                    let mut ports = callback.endpoints.iter().filter(|p| {
                        p.direction == Direction::Received && p.ordinal == event.event.ordinal
                    });
                    let port = ports
                        .next()
                        .ok_or("event record has unknown subscriber ordinal")?;
                    if ports.next().is_some() || port.transport != Transport::Event {
                        return Err(
                            "event record has ambiguous or non-event subscriber ordinal".into()
                        );
                    }
                    if self.denied.contains(&port.channel) {
                        continue;
                    }
                    self.pending = Some(Pending::Event {
                        callback: callback.name.clone(),
                        channel: port.channel.clone(),
                        event,
                    });
                    return Ok(());
                }
                _ => {
                    self.pending = Some(Pending::Data(entry));
                    return Ok(());
                }
            }
        }
        Ok(())
    }
    /// Publish all selected entries through `time`, preserving stable log order.
    /// Event delivery is provided by the executor's recipient-targeted injector.
    pub fn inject_due(
        &mut self,
        time: FrameworkTime,
        event: impl FnMut(&str, &str, &ObservedEvent) -> Result<(), BoxedLogError>,
    ) -> Result<Option<FrameworkTime>, BoxedLogError> {
        self.inject_due_while(time, event, || true)
    }
    /// Interruptible between entries, including skipped metadata. None may mean
    /// paused scanning rather than EOF; inspect exhausted() to distinguish them.
    pub fn inject_due_while(
        &mut self,
        time: FrameworkTime,
        mut event: impl FnMut(&str, &str, &ObservedEvent) -> Result<(), BoxedLogError>,
        mut running: impl FnMut() -> bool,
    ) -> Result<Option<FrameworkTime>, BoxedLogError> {
        if self.failed {
            return Err("replay feed failed previously".into());
        }
        let result = (|| {
            if self.pending.is_none() && !self.exhausted {
                self.advance(&mut running)?;
            }
            while self
                .pending
                .as_ref()
                .is_some_and(|next| next.time() <= time)
                && running()
            {
                match self.pending.take().unwrap() {
                    Pending::Data(entry) => self
                        .sources
                        .get_mut(&entry.channel_name)
                        .unwrap()
                        .inject(entry.header, &entry.serialized_body)
                        .map_err(|e| {
                            format!(
                                "replay channel '{}' at {}: {e}",
                                entry.channel_name, entry.header.published_at
                            )
                        })?,
                    Pending::Event {
                        callback,
                        channel,
                        event: observed,
                    } => event(&callback, &channel, &observed)?,
                }
                self.advance(&mut running)?;
            }
            Ok(self.next_time())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
