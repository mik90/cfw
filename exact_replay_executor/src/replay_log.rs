use crate::ReplayError;
use logging::{LogFileReader, SortedLogStreamReader};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use task::{message::MessageHeader, recording::*, time::FrameworkTime};

pub(crate) type Identity = (String, MessageHeader);

pub struct ReplayLog {
    pub(crate) descriptor: ExecutionDescriptor,
    pub(crate) executions: Vec<ExecutionRecord>,
    pub(crate) payloads: BTreeMap<Identity, Vec<u8>>,
    pub(crate) logged: HashSet<String>,
}
impl ReplayLog {
    pub fn from_reader(reader: &dyn LogFileReader) -> Result<Self, ReplayError> {
        logging::incompleteness::reject_incomplete_recording(
            reader.artifact(logging::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT),
        )
        .map_err(|e| ReplayError::InvalidLog(e.to_string()))?;
        let mut log = Self::descriptor(reader.artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT))?;
        let tables = logging::read_intern_tables(reader)
            .map_err(|e| ReplayError::InvalidLog(e.to_string()))?;
        logging::intern_tables::validate_descriptor_tables(&tables, &log.descriptor)
            .map_err(|e| ReplayError::InvalidLog(e.to_string()))?;
        for index in 0..reader.len() {
            let entry = reader
                .entry(index)
                .ok_or_else(|| ReplayError::InvalidLog("reader length/entry mismatch".into()))?;
            log.push(entry.channel_name, entry.header, entry.serialized_body)?;
        }
        log.finish()?;
        Ok(log)
    }
    pub fn from_sorted(mut reader: SortedLogStreamReader) -> Result<Self, ReplayError> {
        logging::incompleteness::reject_incomplete_recording(
            reader.artifact(logging::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT),
        )
        .map_err(|e| ReplayError::InvalidLog(e.to_string()))?;
        let mut log = Self::descriptor(reader.artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT))?;
        let tables =
            logging::intern_tables::decode_intern_tables(reader.artifact(INTERN_TABLES_ARTIFACT))
                .map_err(|e| ReplayError::InvalidLog(e.to_string()))?;
        logging::intern_tables::validate_descriptor_tables(&tables, &log.descriptor)
            .map_err(|e| ReplayError::InvalidLog(e.to_string()))?;
        while let Some(entry) = reader
            .next_entry()
            .map_err(|e| ReplayError::InvalidLog(e.to_string()))?
        {
            log.push(&entry.channel_name, entry.header, &entry.serialized_body)?;
        }
        log.finish()?;
        Ok(log)
    }
    fn descriptor(bytes: Option<&[u8]>) -> Result<Self, ReplayError> {
        let descriptor: ExecutionDescriptor =
            serde_json::from_slice(bytes.ok_or_else(|| {
                ReplayError::InvalidLog("execution descriptor is required".into())
            })?)
            .map_err(|e| ReplayError::InvalidLog(e.to_string()))?;
        let mut names = HashSet::new();
        let mut publishers = HashSet::new();
        for callback in &descriptor.callbacks {
            if callback.recording_mode != RecordingMode::Full {
                return Err(ReplayError::InvalidLog(format!(
                    "callback '{}' was not fully recorded",
                    callback.name
                )));
            }
            if !names.insert(&callback.name) {
                return Err(ReplayError::InvalidLog("duplicate callback name".into()));
            }
            let mut ports = HashSet::new();
            for port in &callback.endpoints {
                let data_output =
                    port.direction == Direction::Published && port.transport != Transport::Event;
                if data_output != port.publisher_index.is_some() {
                    return Err(ReplayError::InvalidLog(
                        "publisher index must identify a data output port".into(),
                    ));
                }
                if let Some(index) = port.publisher_index
                    && !publishers.insert((port.channel.clone(), index))
                {
                    return Err(ReplayError::InvalidLog(format!(
                        "duplicate publisher index on '{}'",
                        port.channel
                    )));
                }
                if !ports.insert((port.direction == Direction::Received, port.ordinal)) {
                    return Err(ReplayError::InvalidLog(format!(
                        "duplicate ordinal on '{}'",
                        callback.name
                    )));
                }
            }
        }
        let logged = descriptor.logged_channels.iter().cloned().collect();
        Ok(Self {
            descriptor,
            executions: vec![],
            payloads: BTreeMap::new(),
            logged,
        })
    }
    fn push(
        &mut self,
        channel: &str,
        header: MessageHeader,
        bytes: &[u8],
    ) -> Result<(), ReplayError> {
        if header.published_at == FrameworkTime::INVALID {
            return Err(ReplayError::InvalidLog("invalid timestamp".into()));
        }
        match channel {
            EXECUTION_LOG_CHANNEL => {
                let record: ExecutionRecord = serde_json::from_slice(bytes)
                    .map_err(|e| ReplayError::InvalidLog(e.to_string()))?;
                if record.execution_time != header.published_at {
                    return Err(ReplayError::InvalidLog(
                        "execution timestamp differs from header".into(),
                    ));
                }
                self.executions.push(record);
            }
            EXECUTION_EVENT_CHANNEL => {
                // Exact replay restores prepared event snapshots, not arrivals.
                let event: ObservedEvent = serde_json::from_slice(bytes)
                    .map_err(|e| ReplayError::InvalidLog(e.to_string()))?;
                if event.observed_at != header.published_at {
                    return Err(ReplayError::InvalidLog(
                        "event timestamp differs from header".into(),
                    ));
                }
                if self
                    .endpoint(
                        event.callback_id.index(),
                        Direction::Received,
                        event.event.ordinal,
                        true,
                    )?
                    .transport
                    != Transport::Event
                {
                    return Err(ReplayError::InvalidLog(
                        "observed event is not addressed to an event port".into(),
                    ));
                }
            }
            _ => {
                if !self.logged.contains(channel) {
                    return Err(ReplayError::InvalidLog(format!(
                        "payload channel '{channel}' is not declared logged"
                    )));
                }
                let key = (channel.into(), header);
                if self.payloads.insert(key, bytes.to_vec()).is_some() {
                    return Err(ReplayError::InvalidLog(format!(
                        "ambiguous payload identity on '{channel}': {header:?}"
                    )));
                }
            }
        }
        Ok(())
    }
    pub(crate) fn endpoint(
        &self,
        callback: usize,
        direction: Direction,
        ordinal: usize,
        event: bool,
    ) -> Result<&EndpointDescriptor, ReplayError> {
        self.descriptor
            .callbacks
            .get(callback)
            .and_then(|c| {
                c.endpoints.iter().find(|p| {
                    p.direction == direction
                        && p.ordinal == ordinal
                        && (if event {
                            p.transport != Transport::Native
                        } else {
                            p.transport != Transport::Event
                        })
                })
            })
            .ok_or_else(|| {
                ReplayError::InvalidLog(format!(
                    "unknown {:?} port {ordinal} on callback {callback}",
                    direction
                ))
            })
    }
    fn finish(&mut self) -> Result<(), ReplayError> {
        self.executions.sort_by_key(|record| record.execution_time);
        let mut produced = BTreeSet::new();
        for record in &self.executions {
            let mut batch_indices = BTreeMap::<usize, u32>::new();
            if record.outcome != Outcome::Committed {
                return Err(ReplayError::InvalidLog(format!(
                    "cannot exactly replay {:?} execution",
                    record.outcome
                )));
            }
            if record.callback_id.index() >= self.descriptor.callbacks.len() {
                return Err(ReplayError::InvalidLog("unknown callback index".into()));
            }
            for (direction, messages) in [
                (Direction::Received, &record.inputs),
                (Direction::Published, &record.outputs),
            ] {
                for message in messages {
                    let port = self.endpoint(
                        record.callback_id.index(),
                        direction,
                        message.ordinal,
                        false,
                    )?;
                    if message.header.published_at == FrameworkTime::INVALID {
                        return Err(ReplayError::InvalidLog("invalid message reference".into()));
                    }
                    if direction == Direction::Published {
                        if !produced.insert((port.channel.clone(), message.header)) {
                            return Err(ReplayError::InvalidLog(format!(
                                "ambiguous publication identity on '{}': {:?}",
                                port.channel, message.header
                            )));
                        }
                        let next = batch_indices.entry(message.ordinal).or_default();
                        if port.publisher_index != Some(message.header.publisher_index)
                            || message.header.batch_index != *next
                            || message.header.published_at != record.execution_time
                        {
                            return Err(ReplayError::InvalidLog(
                                "output header differs from publisher/batch metadata".into(),
                            ));
                        }
                        *next = next.checked_add(1).ok_or_else(|| {
                            ReplayError::InvalidLog("publisher batch index overflow".into())
                        })?;
                    }
                }
            }
            for event in &record.events {
                if self
                    .endpoint(
                        record.callback_id.index(),
                        Direction::Received,
                        event.ordinal,
                        true,
                    )?
                    .transport
                    != Transport::Event
                {
                    return Err(ReplayError::InvalidLog(
                        "input event ordinal is not an event port".into(),
                    ));
                }
            }
            for event in &record.output_events {
                self.endpoint(
                    record.callback_id.index(),
                    Direction::Published,
                    event.ordinal,
                    true,
                )?;
            }
        }
        Ok(())
    }
    pub fn execution_count(&self) -> usize {
        self.executions.len()
    }
    /// Upper bound for a source cache when reproducing this trace without extra
    /// publications. Includes logged source values and unlogged output identities.
    pub fn source_capacity(&self, channel: &str) -> usize {
        let mut headers: BTreeSet<_> = self
            .payloads
            .keys()
            .filter(|(name, _)| name == channel)
            .map(|(_, header)| *header)
            .collect();
        for record in &self.executions {
            for output in &record.outputs {
                if self
                    .endpoint(
                        record.callback_id.index(),
                        Direction::Published,
                        output.ordinal,
                        false,
                    )
                    .is_ok_and(|port| port.channel == channel)
                {
                    headers.insert(output.header);
                }
            }
        }
        headers.len()
    }
    pub fn descriptor_ref(&self) -> &ExecutionDescriptor {
        &self.descriptor
    }
}

#[cfg(test)]
mod recording_mode_tests {
    use super::*;
    #[test]
    fn reduced_recording_is_not_an_exact_trace() {
        for mode in [
            RecordingMode::Off,
            RecordingMode::DurationOnly,
            RecordingMode::Full,
        ] {
            let descriptor = ExecutionDescriptor {
                callbacks: vec![CallbackDescriptor {
                    name: "work".into(),
                    recording_mode: mode,
                    endpoints: vec![],
                }],
                logged_channels: vec![],
            };
            let bytes = serde_json::to_vec(&descriptor).unwrap();
            let result = ReplayLog::descriptor(Some(&bytes));
            assert_eq!(result.is_ok(), mode == RecordingMode::Full);
            if let Err(error) = result {
                assert!(error.to_string().contains("not fully recorded"));
            }
        }
    }
}
