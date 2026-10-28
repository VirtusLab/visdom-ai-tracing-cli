//! Claude Code subagent transcripts.
//!
//! Claude Code (2.1.x) writes each subagent's conversation to a file of its
//! own, `<dir of transcript_path>/<session id>/subagents/agent-<id>.jsonl`,
//! next to an `agent-<id>.meta.json`. The main transcript holds none of the
//! subagent's assistant lines or `usage`, so a hook that tails only
//! `transcript_path` misses every token a subagent spends (VIS-1078).
//!
//! Every line in those files carries the subagent's `agentId` and the parent
//! `sessionId`. The stream hook sends each file's new lines under the parent
//! session, one file per request, with `transcript_offset` counted within that
//! file; the server keys the chunks on the line's own `agentId`, so they never
//! collide with the main transcript's chunk indexes.

use std::fs;
use std::io::{self, BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Where the per-file read offsets live inside a session directory.
const OFFSETS_DIR: &str = ".subagent_offsets";

/// The session's subagent transcripts, sorted by path so a hook sends them in
/// a stable order. Empty when the session has none (or `transcript_path` is
/// empty, as with fileless agents). Only `agent-<id>.jsonl` is taken: the
/// directory also holds `agent-<id>.meta.json` and forked-skill sidecars.
pub fn transcripts(transcript_path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(session)) = (transcript_path.parent(), transcript_path.file_stem()) else {
        return vec![];
    };
    let Ok(entries) = fs::read_dir(dir.join(session).join("subagents")) else {
        return vec![];
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("agent-"))
                .and_then(|n| n.strip_suffix(".jsonl"))
                .is_some_and(|id| !id.is_empty() && !id.contains('.'))
        })
        .collect();
    files.sort();
    files
}

/// The file that holds how far `transcript` has been sent, keyed by its file
/// stem (`agent-<id>`). Separate from the main transcript's `.stream_offset`.
pub fn offset_path(session_dir: &Path, transcript: &Path) -> PathBuf {
    let stem = transcript.file_stem().unwrap_or_default();
    session_dir.join(OFFSETS_DIR).join(stem)
}

/// Persist `offset` as the next read position of the file `offset_path` keys.
/// Replaced atomically: a hook killed mid-write must leave the old offset,
/// not an empty file every later read rejects as corrupt.
pub fn write_offset(offset_path: &Path, offset: i64) -> io::Result<()> {
    let Some(dir) = offset_path.parent() else {
        return fs::write(offset_path, offset.to_string());
    };
    fs::create_dir_all(dir)?;
    let tmp = offset_path.with_extension(format!("tmp.{}", uuid::Uuid::now_v7()));
    fs::write(&tmp, offset.to_string())?;
    fs::rename(&tmp, offset_path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Take the exclusive lock on the file `offset_path` keys, without waiting.
/// `None` means another hook of the session holds it.
///
/// A hook holds it from reading the file to writing its new offset. Parallel
/// hooks of one session run concurrently, and without it two of them can read
/// from one offset, the one that read less can write its offset last, and the
/// next hook re-sends the rest from that offset: same lines, new chunk_indexes,
/// stored twice. A hook that finds the lock taken skips the file instead of
/// waiting (the holder may spend the whole delivery budget); a later hook, at
/// the latest that subagent's `SubagentStop`, sends what the holder did not
/// read. The OS drops the lock when its holder exits, even if killed.
pub fn try_lock(offset_path: &Path) -> io::Result<Option<fs::File>> {
    if let Some(dir) = offset_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(offset_path.with_extension("lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Read the lines appended to `transcript` since the offset stored in
/// `offset_path` (0 when absent). Returns `(lines, start_offset, end_offset)`
/// with the meaning of `commands::stream::read_new_transcript_lines`:
/// `start_offset` goes on the request as `transcript_offset`, `end_offset` is
/// the next read position.
///
/// Unlike that reader, it stops before a trailing line with no newline yet. A
/// subagent running in the background appends to its file while the parent's
/// hooks fire, so a hook can catch a line half-written; counting it as read
/// would skip the rest of that line for good. Offsets are exact byte counts.
///
/// A corrupt offset file is an error rather than a restart from 0: chunk
/// indexes derive from the start offset, so re-reading from elsewhere would
/// give the same lines new indexes and the server would store them twice.
pub fn read_new_lines(
    transcript: &Path,
    offset_path: &Path,
) -> io::Result<(Vec<serde_json::Value>, i64, i64)> {
    let start: i64 = match fs::read_to_string(offset_path) {
        Ok(content) => content
            .trim()
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e),
    };
    let mut file = fs::File::open(transcript)?;
    file.seek(SeekFrom::Start(start as u64))?;
    let mut reader = io::BufReader::new(file);

    let mut lines = Vec::new();
    let mut end = start;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            break;
        }
        end += n as i64;
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&buf) {
            lines.push(value);
        }
    }
    Ok((lines, start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn append(path: &Path, text: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn transcripts_lists_only_agent_jsonl_files_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("sess-1.jsonl");
        let sub = dir.path().join("sess-1").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        for name in [
            "agent-b2.jsonl",
            "agent-a1.jsonl",
            "agent-a1.meta.json",
            "agent-a1.forked-skill.json",
            "agent-.jsonl",
            "notes.jsonl",
        ] {
            fs::write(sub.join(name), "").unwrap();
        }
        fs::create_dir(sub.join("agent-dir.jsonl")).unwrap();

        assert_eq!(
            transcripts(&main),
            vec![sub.join("agent-a1.jsonl"), sub.join("agent-b2.jsonl")]
        );
    }

    #[test]
    fn transcripts_is_empty_without_a_subagents_dir_or_a_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(transcripts(&dir.path().join("sess-1.jsonl")).is_empty());
        assert!(transcripts(Path::new("")).is_empty());
    }

    #[test]
    fn offset_path_is_per_file_and_apart_from_the_main_offset() {
        let a = offset_path(Path::new("/s"), Path::new("/t/subagents/agent-a1.jsonl"));
        let b = offset_path(Path::new("/s"), Path::new("/t/subagents/agent-b2.jsonl"));
        assert_eq!(a, Path::new("/s/.subagent_offsets/agent-a1"));
        assert_ne!(a, b);
    }

    #[test]
    fn read_new_lines_resumes_at_the_stored_offset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent-a1.jsonl");
        let off = offset_path(dir.path(), &file);
        append(&file, "{\"n\":1}\n{\"n\":2}\n");

        let (lines, start, end) = read_new_lines(&file, &off).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!((start, end), (0, 16));
        write_offset(&off, end).unwrap();

        append(&file, "{\"n\":3}\n");
        let (lines, start, end) = read_new_lines(&file, &off).unwrap();
        assert_eq!(lines, vec![serde_json::json!({"n": 3})]);
        assert_eq!((start, end), (16, 24));
    }

    #[test]
    fn read_new_lines_leaves_a_half_written_line_for_the_next_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent-a1.jsonl");
        let off = offset_path(dir.path(), &file);
        append(&file, "{\"n\":1}\n{\"n\":");

        let (lines, _, end) = read_new_lines(&file, &off).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(end, 8);
        write_offset(&off, end).unwrap();

        append(&file, "2}\n");
        let (lines, start, end) = read_new_lines(&file, &off).unwrap();
        assert_eq!(lines, vec![serde_json::json!({"n": 2})]);
        assert_eq!((start, end), (8, 16));
    }

    #[test]
    fn read_new_lines_skips_blank_and_malformed_lines_but_counts_their_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent-a1.jsonl");
        append(&file, "\nnot json\r\n{\"n\":1}\r\n");

        let (lines, _, end) = read_new_lines(&file, &offset_path(dir.path(), &file)).unwrap();
        assert_eq!(lines, vec![serde_json::json!({"n": 1})]);
        assert_eq!(end, fs::metadata(&file).unwrap().len() as i64);
    }

    #[test]
    fn try_lock_is_exclusive_per_file_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let a1 = offset_path(dir.path(), Path::new("agent-a1.jsonl"));
        let b2 = offset_path(dir.path(), Path::new("agent-b2.jsonl"));

        let held = try_lock(&a1).unwrap().expect("free lock");
        assert!(try_lock(&a1).unwrap().is_none(), "a second holder");
        assert!(try_lock(&b2).unwrap().is_some(), "another file is free");
        drop(held);
        assert!(try_lock(&a1).unwrap().is_some());
        assert!(!a1.exists(), "the lock is not the offset file");
    }

    #[test]
    fn read_new_lines_rejects_a_corrupt_offset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent-a1.jsonl");
        let off = offset_path(dir.path(), &file);
        append(&file, "{\"n\":1}\n");
        write_offset(&off, 0).unwrap();
        fs::write(&off, "garbage").unwrap();

        let err = read_new_lines(&file, &off).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
