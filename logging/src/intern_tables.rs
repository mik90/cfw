//! Name tables are the first artifact in a logging stream. Array positions are IDs.
use crate::{BoxedLogError, LogFileWriter};
use std::sync::{Arc, Mutex};
use task::recording::{EXECUTION_EVENT_CHANNEL, EXECUTION_LOG_CHANNEL};
pub use task::recording::{INTERN_TABLES_ARTIFACT, InternTables};

#[cfg(feature = "serde")]
pub fn read_intern_tables(
    reader: &dyn crate::LogFileReader,
) -> Result<InternTables, BoxedLogError> {
    decode_intern_tables(reader.artifact(INTERN_TABLES_ARTIFACT))
}
#[cfg(feature = "serde")]
pub fn decode_intern_tables(bytes: Option<&[u8]>) -> Result<InternTables, BoxedLogError> {
    Ok(serde_json::from_slice(
        bytes.ok_or("missing intern_tables artifact")?,
    )?)
}

#[cfg(feature = "serde")]
pub fn validate_descriptor_tables(
    tables: &InternTables,
    descriptor: &task::recording::ExecutionDescriptor,
) -> Result<(), BoxedLogError> {
    for (index, callback) in descriptor.callbacks.iter().enumerate() {
        let id = task::string_interner::CallbackId::from_index(index)
            .ok_or("callback ID out of range")?;
        if tables.callbacks.try_lookup_by_id(id) != Some(callback.name.as_str()) {
            return Err(format!(
                "callback '{}' does not match intern table ID {index}",
                callback.name
            )
            .into());
        }
        for endpoint in &callback.endpoints {
            if tables.channels.lookup_by_value(&endpoint.channel).is_none() {
                return Err(format!(
                    "channel '{}' is absent from intern tables",
                    endpoint.channel
                )
                .into());
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct State {
    tables: InternTables,
    started: bool,
    #[cfg(feature = "serde")]
    descriptor: Option<task::recording::ExecutionDescriptor>,
}
/// Shared across shards, including the startup write barrier.
#[derive(Clone, Default)]
pub(crate) struct Registry(Arc<Mutex<State>>);
impl Registry {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub fn new<'a>(channels: impl IntoIterator<Item = &'a str>) -> Self {
        let registry = Self::default();
        {
            let mut state = registry.state();
            for channel in channels {
                state.tables.channels.intern(channel);
            }
            state.tables.channels.intern(EXECUTION_LOG_CHANNEL);
            state.tables.channels.intern(EXECUTION_EVENT_CHANNEL);
        }
        registry
    }
    pub fn tables(&self) -> InternTables {
        self.state().tables.clone()
    }
    pub fn channel(&self, name: &str) -> task::string_interner::ChannelId {
        self.state()
            .tables
            .channels
            .lookup_by_value(name)
            .expect("diagnostic channel must be registered before startup")
    }
    pub fn configure(&self, mut tables: InternTables) -> Result<(), BoxedLogError> {
        let mut state = self.state();
        if state.started {
            return Err("logging name tables are already frozen".into());
        }
        for name in state.tables.channels.names() {
            tables.channels.intern(name);
        }
        state.tables = tables;
        Ok(())
    }
    #[cfg(feature = "serde")]
    pub fn recording(
        &self,
        descriptor: task::recording::ExecutionDescriptor,
    ) -> Result<(), BoxedLogError> {
        let mut state = self.state();
        if state.started {
            return Err("recording must be attached before logging starts".into());
        }
        state.descriptor = Some(descriptor);
        Ok(())
    }
    pub fn start(&self, writer: &mut dyn LogFileWriter) -> Result<(), BoxedLogError> {
        let mut state = self.state();
        if state.started {
            return Ok(());
        }
        #[cfg(feature = "serde")]
        {
            if let Some(descriptor) = &state.descriptor {
                validate_descriptor_tables(&state.tables, descriptor)?;
            }
            writer.write_artifact(INTERN_TABLES_ARTIFACT, &serde_json::to_vec(&state.tables)?)?;
            if let Some(descriptor) = &state.descriptor {
                writer.write_artifact(
                    task::recording::EXECUTION_LOG_DESCRIPTOR_ARTIFACT,
                    &serde_json::to_vec(descriptor)?,
                )?;
            }
        }
        #[cfg(not(feature = "serde"))]
        let _ = writer;
        state.started = true;
        Ok(())
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;
    #[test]
    fn table_wire_format_preserves_ids_and_rejects_invalid_names() {
        use task::{recording::*, string_interner::ChannelId};
        let mut tables = InternTables::default();
        let z = tables.channels.intern("z");
        let a = tables.channels.intern("a");
        tables.callbacks.intern("work");
        assert_eq!(serde_json::to_string(&z).unwrap(), "0");
        assert_eq!(serde_json::to_string(&a).unwrap(), "1");
        let bytes = serde_json::to_vec(&tables).unwrap();
        let restored = decode_intern_tables(Some(&bytes)).unwrap();
        assert_eq!(restored.channels.lookup_by_id(z), "z");
        assert_eq!(restored.channels.lookup_by_id(a), "a");
        assert!(
            restored
                .channels
                .try_lookup_by_id(ChannelId::from_index(99).unwrap())
                .is_none()
        );
        assert!(decode_intern_tables(None).is_err());
        assert!(decode_intern_tables(Some(br#"{"channels":["z","z"],"callbacks":[]}"#)).is_err());
        let mut descriptor = ExecutionDescriptor {
            callbacks: vec![CallbackDescriptor {
                name: "other".into(),
                recording_mode: RecordingMode::Full,
                endpoints: vec![],
            }],
            logged_channels: vec![],
        };
        assert!(validate_descriptor_tables(&restored, &descriptor).is_err());
        descriptor.callbacks[0].name = "work".into();
        assert!(validate_descriptor_tables(&restored, &descriptor).is_ok());
        descriptor.callbacks[0].endpoints.push(EndpointDescriptor {
            ordinal: 0,
            channel: "missing".into(),
            direction: Direction::Received,
            transport: Transport::Native,
            payload_type: "u64".into(),
            publisher_index: None,
        });
        assert!(validate_descriptor_tables(&restored, &descriptor).is_err());
    }
}
