use std::fs;
use std::io::Write;
use tempfile::TempDir;

fn setup_session_dir() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join(".tracevault/sessions/test-session-123");
    fs::create_dir_all(&session_dir).unwrap();
    (tmp, session_dir)
}

#[test]
fn test_read_new_transcript_lines() {
    let (_tmp, session_dir) = setup_session_dir();
    let transcript_path = session_dir.join("transcript.jsonl");
    let offset_path = session_dir.join(".stream_offset");

    fs::write(
        &transcript_path,
        "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n",
    )
    .unwrap();

    let (lines, start_offset, new_offset) =
        tracevault_cli::commands::stream::read_new_transcript_lines(&transcript_path, &offset_path)
            .unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(start_offset, 0, "first read starts at byte 0");
    assert!(new_offset > 0);

    fs::write(&offset_path, new_offset.to_string()).unwrap();

    let (lines, start_offset2, _) =
        tracevault_cli::commands::stream::read_new_transcript_lines(&transcript_path, &offset_path)
            .unwrap();
    assert_eq!(lines.len(), 0);
    assert_eq!(
        start_offset2, new_offset,
        "second read's start_offset must equal the first read's persisted end offset"
    );

    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&transcript_path)
        .unwrap();
    writeln!(f, "{{\"type\":\"user\",\"message\":\"more\"}}").unwrap();

    let (lines, start_offset3, _) =
        tracevault_cli::commands::stream::read_new_transcript_lines(&transcript_path, &offset_path)
            .unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        start_offset3, new_offset,
        "third read's start_offset must still equal the last persisted end offset"
    );
}

#[test]
fn test_pending_queue_write_and_drain() {
    let (_tmp, session_dir) = setup_session_dir();
    let pending_path = session_dir.join("pending.jsonl");

    tracevault_cli::commands::stream::append_pending(&pending_path, "event1").unwrap();
    tracevault_cli::commands::stream::append_pending(&pending_path, "event2").unwrap();

    let events = tracevault_cli::commands::stream::drain_pending(&pending_path).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], "event1");
    assert!(!pending_path.exists());
}
