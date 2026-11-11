use exact_replay_executor::ReplayLog;
use logging::{OwnedLogEntry, SortedLogStreamReader, testing::LoggedData};
use std::sync::{Arc, Mutex};

pub fn replay_log_from_recorded_data(data: &Arc<Mutex<LoggedData>>) -> ReplayLog {
    let data = data.lock().unwrap();
    let entries = data
        .messages()
        .iter()
        .map(|message| OwnedLogEntry {
            header: *message.header(),
            channel_name: message.channel().clone(),
            serialized_body: message.body().to_vec(),
        })
        .collect();
    let artifacts = data
        .artifacts()
        .iter()
        .map(|artifact| (artifact.name().to_owned(), artifact.body().to_vec()))
        .collect();
    let reader = SortedLogStreamReader::from_entries(entries, artifacts).unwrap();
    ReplayLog::from_sorted(reader).unwrap()
}
