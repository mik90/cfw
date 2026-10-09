#![cfg(feature = "serde")]
use logging::{LogFileWriter, LogReadError, OwnedLogEntry, SortedLogStreamReader};
use std::{
    collections::HashMap,
    io::{Cursor, Read},
};
use task::{message::MessageHeader, time::FrameworkTime};

fn entry(time: i64, value: u8) -> OwnedLogEntry {
    OwnedLogEntry {
        header: MessageHeader::new(FrameworkTime::from_nanoseconds(time)),
        channel_name: "input".into(),
        serialized_body: vec![value],
    }
}
#[test]
fn memory_reader_is_stable_and_preserves_artifacts() {
    let mut reader = SortedLogStreamReader::from_entries(
        vec![entry(30, 3), entry(10, 1), entry(10, 2)],
        HashMap::from([("metadata".into(), b"{\"key\":42}".to_vec())]),
    )
    .unwrap();
    assert_eq!(
        reader.first_log_time(),
        Some(FrameworkTime::from_nanoseconds(10))
    );
    assert_eq!(reader.len(), 3);
    assert_eq!(
        reader.artifact("metadata"),
        Some(b"{\"key\":42}".as_slice())
    );
    let (batch, next) = reader
        .read_until(FrameworkTime::from_nanoseconds(10))
        .unwrap();
    assert_eq!(
        batch
            .iter()
            .map(|e| e.serialized_body[0])
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(next, Some(FrameworkTime::from_nanoseconds(30)));
    assert_eq!(reader.next_entry().unwrap().unwrap().serialized_body, [3]);
    assert!(reader.next_entry().unwrap().is_none());
}

#[test]
#[cfg_attr(miri, ignore = "requires filesystem")]
fn external_sort_handles_artifacts_and_stable_ties_across_many_runs() {
    let mut bytes = Vec::new();
    let mut writer = logging::log_file_json::JsonLogFileWriter::new(&mut bytes);
    writer.write_artifact("first", b"{}").unwrap();
    for (index, time) in [30, 10, 20, 10, 10, 5, 20, 10, 40].into_iter().enumerate() {
        let row = entry(time, index as u8);
        writer
            .store_message(&row.channel_name, &row.header, &row.serialized_body)
            .unwrap();
        if index == 3 {
            writer.write_artifact("middle", b"[1,2]").unwrap();
        }
    }
    for batch_size in [1, 2, 3, 100] {
        let mut reader =
            SortedLogStreamReader::from_reader(Cursor::new(&bytes), batch_size).unwrap();
        assert_eq!(
            reader.first_log_time(),
            Some(FrameworkTime::from_nanoseconds(5))
        );
        assert_eq!(reader.artifact("middle"), Some(b"[1,2]".as_slice()));
        let (batch, next) = reader
            .read_until(FrameworkTime::from_nanoseconds(100))
            .unwrap();
        assert_eq!(
            batch
                .iter()
                .map(|e| e.serialized_body[0])
                .collect::<Vec<_>>(),
            [5, 1, 3, 4, 7, 2, 6, 0, 8]
        );
        assert!(next.is_none());
    }
    // Already-sorted input must not interpret artifacts between messages as EOF.
    let mut sorted = Vec::new();
    let mut writer = logging::log_file_json::JsonLogFileWriter::new(&mut sorted);
    for i in 0..3 {
        let row = entry(i, i as u8);
        writer
            .store_message("input", &row.header, &row.serialized_body)
            .unwrap();
        writer
            .write_artifact(&format!("artifact_{i}"), b"null")
            .unwrap();
    }
    let mut reader = SortedLogStreamReader::from_reader(sorted.as_slice(), 1).unwrap();
    assert_eq!(
        reader
            .read_until(FrameworkTime::from_nanoseconds(100))
            .unwrap()
            .0
            .len(),
        3
    );
}

#[test]
#[cfg_attr(miri, ignore = "requires filesystem")]
fn malformed_input_and_io_failure_are_errors_not_eof() {
    let mut bytes = Vec::new();
    logging::log_file_json::JsonLogFileWriter::new(&mut bytes)
        .store_message("input", &entry(0, 0).header, b"42")
        .unwrap();
    bytes.extend_from_slice(b"not json\n");
    assert!(matches!(
        SortedLogStreamReader::from_reader(bytes.as_slice(), 1),
        Err(LogReadError::Json { line: 2, .. })
    ));
    let duplicate = b"{\"artifact\":\"a\",\"body\":1}\n{\"artifact\":\"a\",\"body\":2}\n";
    assert!(matches!(
        SortedLogStreamReader::from_reader(duplicate.as_slice(), 1),
        Err(LogReadError::Invalid(_))
    ));
    assert!(SortedLogStreamReader::from_reader(b"".as_slice(), 0).is_err());
    struct FailsAfter(Cursor<Vec<u8>>);
    impl Read for FailsAfter {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.0.position() == self.0.get_ref().len() as u64 {
                return Err(std::io::Error::other("injected read failure"));
            }
            self.0.read(out)
        }
    }
    let first_line = bytes
        .split_inclusive(|b| *b == b'\n')
        .next()
        .unwrap()
        .to_vec();
    assert!(matches!(
        SortedLogStreamReader::from_reader(
            std::io::BufReader::new(FailsAfter(Cursor::new(first_line))),
            1
        ),
        Err(LogReadError::Io(_))
    ));
}

#[test]
fn empty_and_invalid_timestamps() {
    let reader = SortedLogStreamReader::from_entries(vec![], HashMap::new()).unwrap();
    assert!(reader.is_empty());
    assert!(reader.first_log_time().is_none());
    let mut invalid = entry(0, 0);
    invalid.header.published_at = FrameworkTime::INVALID;
    assert!(SortedLogStreamReader::from_entries(vec![invalid], HashMap::new()).is_err());
}
