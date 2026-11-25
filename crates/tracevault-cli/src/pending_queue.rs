//! The offline queue of a session: `pending-<target>.jsonl` in its session
//! directory, one [`StreamEventRequest`] per line, holding what a hook could
//! not send.
//!
//! Every transcript line is in exactly one place: sent, in the queue, or
//! still unread in the agent's transcript file. A hook that cannot send lines
//! it read from a file queues them and moves the file's read offset past
//! them, so the next hook does not read them again. Earlier releases left the
//! offset where it was *and* queued the lines, so every failed hook added the
//! whole unsent end of the transcript to the queue once more, and the queue
//! (read whole by every hook) grew to gigabytes for a transcript of a few
//! megabytes.
//!
//! A queue is bounded ([`MAX_QUEUE_BYTES`]) and read one record at a time, so
//! a hook's memory does not depend on how long the server has been
//! unreachable. Lines that do not fit stay unread in the transcript file,
//! which is as good a place: the offset stops there until the queue drains.
//!
//! A session directory whose queues an earlier release wrote is brought up
//! to this once ([`upgrade`]): there the lines of a transcript file are both
//! in the queue, many times, and still ahead of the read offset, so the
//! queue's copies are dropped.

use crate::program::TOP;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

/// The most a queue file may hold. Transcript lines that do not fit are left
/// in the transcript; an event (or a record an agent handed over inline) that
/// does not fit can be kept nowhere and is dropped, with a warning. A queue
/// without a bound did that to the machine instead.
pub const MAX_QUEUE_BYTES: u64 = 32 * 1024 * 1024;

/// How long a claimed queue file must have gone untouched before another
/// reader takes it over as abandoned. A reader touches its claim with every
/// record it takes from it, and the longest it can then be busy with one is
/// a request's timeout (60 s, in `flush`; a hook is killed at 10 s).
const ABANDONED_AFTER: Duration = Duration::from_secs(300);

/// Whether the transcript lines of a queued record are read again from the
/// agent's transcript file, going by the `tool` the record names. An absent
/// `tool` is Claude Code (protocol v1 predates the field).
pub fn lines_are_reread(tool: Option<&str>) -> bool {
    use crate::agent::Agent;
    ![Agent::OpenCode, Agent::Amp]
        .iter()
        .any(|a| Some(a.tool_name()) == tool)
}

/// Put a record that was already in the queue back into it, whatever the
/// queue's size: a reader that claimed a queue and could not send a record
/// must not lose it to the bound, which a hook queueing meanwhile may have
/// used up. It can exceed the bound by no more than what was claimed.
pub fn requeue(pending_path: &Path, json: &str) -> io::Result<()> {
    let _lock = queue_lock(pending_path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pending_path)?;
    writeln!(file, "{json}")
}

/// The lock that orders writes to the queue at `pending_path` with its being
/// claimed. Without it a writer could open the file, have it claimed, read
/// and removed, and then write into the removed file: a record nobody will
/// ever read, whose lines the transcript's offset has moved past.
pub(crate) fn queue_lock(pending_path: &Path) -> io::Result<fs::File> {
    // A hidden name: whatever lists a session directory's `pending*` files
    // must not take the lock for a queue.
    let name = pending_path
        .file_name()
        .map(|n| format!(".{}.lock", n.to_string_lossy()))
        .unwrap_or_else(|| ".pending.lock".to_string());
    lock(&pending_path.with_file_name(name))
}

/// The queue file a directory entry belongs to: the entry itself when it is
/// a queue, and for a claimed queue (see [`claim`]) the file it was claimed
/// from, which may not exist while its records wait in the claim.
pub fn queue_of(entry: &Path) -> Option<PathBuf> {
    let name = entry.file_name()?.to_str()?;
    let base = match name.split_once(".draining-") {
        Some((base, _)) => base,
        None => name,
    };
    base.ends_with(".jsonl").then(|| entry.with_file_name(base))
}

/// Take the exclusive lock `lock_path` names, waiting for its holder. For
/// the few file operations a hook must not interleave with another's; the
/// OS drops the lock when its holder exits, even if killed.
pub fn lock(lock_path: &Path) -> io::Result<fs::File> {
    if let Some(dir) = lock_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    file.lock()?;
    Ok(file)
}

/// `part` without the transcript lines a file still holds: `None` when it is
/// nothing but lines.
pub fn without_lines(mut part: StreamEventRequest) -> Option<StreamEventRequest> {
    if part.event_type == StreamEventType::Transcript {
        return None;
    }
    part.transcript_lines = None;
    Some(part)
}

/// Append one record to the queue if it fits; `false` when the queue is full.
pub fn try_append(pending_path: &Path, json: &str) -> io::Result<bool> {
    let _lock = queue_lock(pending_path)?;
    // What readers hold claimed counts too: it may all come back, so a claim
    // must not make room for as much again.
    let size =
        fs::metadata(pending_path).map(|m| m.len()).unwrap_or(0) + claimed_bytes(pending_path);
    if size + json.len() as u64 + 1 > MAX_QUEUE_BYTES {
        return Ok(false);
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pending_path)?;
    writeln!(file, "{json}")?;
    Ok(true)
}

/// The size of the files claimed from the queue at `pending_path` and not
/// yet released.
fn claimed_bytes(pending_path: &Path) -> u64 {
    let (Some(dir), Some(prefix)) = (pending_path.parent(), claim_prefix(pending_path)) else {
        return 0;
    };
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with(&prefix))
                })
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Append one record to the queue. A full queue drops it and says so once
/// per process.
pub fn append(pending_path: &Path, json: &str) -> io::Result<()> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !try_append(pending_path, json)? && !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "{TOP}: warning: the queue of unsent events {} is full ({} MB); \
             events of this session are dropped until it is sent",
            pending_path.display(),
            MAX_QUEUE_BYTES / (1024 * 1024)
        );
    }
    Ok(())
}

/// A queue taken out of its place to be read: the file is renamed, so a hook
/// that queues meanwhile starts a new one, and what the reader does not send
/// it appends there. The claimed files are removed by [`Claimed::release`]; a
/// reader that dies first leaves them to be taken over (after
/// [`ABANDONED_AFTER`]) instead of losing what it held in memory.
#[derive(Default)]
pub struct Claimed {
    files: Vec<PathBuf>,
}

fn claim_prefix(pending_path: &Path) -> Option<String> {
    Some(format!("{}.draining-", pending_path.file_name()?.to_str()?))
}

/// Claim the queue at `pending_path`, together with claims other readers
/// abandoned (oldest first, as their records are older).
pub fn claim(pending_path: &Path) -> io::Result<Claimed> {
    claim_at(pending_path, SystemTime::now())
}

fn claim_at(pending_path: &Path, now: SystemTime) -> io::Result<Claimed> {
    let mut files = Vec::new();
    let (Some(dir), Some(prefix)) = (pending_path.parent(), claim_prefix(pending_path)) else {
        return Ok(Claimed { files });
    };

    let mut abandoned: Vec<(SystemTime, PathBuf)> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .filter(|(modified, _)| {
                now.duration_since(*modified)
                    .is_ok_and(|age| age >= ABANDONED_AFTER)
            })
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e),
    };
    abandoned.sort();
    for (_, path) in abandoned {
        // Taken over under a name of our own, so two hooks do not both read it.
        let mine = dir.join(format!("{prefix}{}", uuid::Uuid::now_v7()));
        if fs::rename(&path, &mine).is_ok() {
            touch(&mine);
            files.push(mine);
        }
    }

    let mine = dir.join(format!("{prefix}{}", uuid::Uuid::now_v7()));
    // Not while a writer is between opening the queue and writing to it.
    let _lock = queue_lock(pending_path)?;
    match fs::rename(pending_path, &mine) {
        Ok(()) => {
            touch(&mine);
            files.push(mine);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(Claimed { files })
}

/// A rename keeps the file's modification time, and the age of a claim is
/// counted from when it was claimed.
fn touch(path: &Path) {
    if let Ok(file) = OpenOptions::new().append(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

impl Claimed {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// This claim followed by `later`'s, read as one.
    pub fn then(mut self, later: Claimed) -> Claimed {
        self.files.extend(later.files);
        self
    }

    /// The claimed records in order, one at a time.
    pub fn records(&self) -> impl Iterator<Item = io::Result<String>> + Send + '_ {
        let mut reader = self.reader();
        std::iter::from_fn(move || reader.next_record().transpose())
    }

    /// A reader that knows how far into the claim it is (see [`Reader`]).
    pub fn reader(&self) -> Reader<'_> {
        Reader {
            files: &self.files,
            file: 0,
            open: None,
            offset: 0,
            last: Position { file: 0, offset: 0 },
        }
    }

    /// The same claim, as one on the queue at `pending_path`: files claimed
    /// from another queue are renamed to claims on this one, in order. For a
    /// queue that is adopted (a repo switch): from here on its records are
    /// sent as this queue's, and whoever takes the claim over after its
    /// reader died must send them there too, not to the target they were
    /// queued for.
    pub fn retarget(mut self, pending_path: &Path) -> io::Result<Claimed> {
        let (Some(dir), Some(prefix)) = (pending_path.parent(), claim_prefix(pending_path)) else {
            return Ok(self);
        };
        for path in &mut self.files {
            let named_for_it = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix));
            if !named_for_it {
                let renamed = dir.join(format!("{prefix}{}", uuid::Uuid::now_v7()));
                fs::rename(&*path, &renamed)?;
                *path = renamed;
            }
        }
        Ok(self)
    }

    /// Give the claim up. With `rest` (where a [`Reader`] stopped) the
    /// records from there on stay queued, in order and ahead of anything
    /// queued since; without it every record was dealt with and nothing
    /// stays.
    ///
    /// The rest is not copied back into the queue: each file is left as a
    /// claim that counts as abandoned at once, cut down to what was not read
    /// by replacing it in one step. A reader that dies anywhere in this
    /// leaves every record in exactly one file, so recovery neither loses nor
    /// repeats one. When nothing else was queued meanwhile, the one file
    /// left takes the queue's own name again.
    pub fn finish(self, rest: Option<Position>, pending_path: &Path) -> io::Result<()> {
        let Some(rest) = rest else {
            for path in &self.files {
                let _ = fs::remove_file(path);
            }
            return Ok(());
        };
        let mut left = Vec::new();
        for (i, path) in self.files.iter().enumerate() {
            if i < rest.file {
                let _ = fs::remove_file(path);
                continue;
            }
            let from = if i == rest.file { rest.offset } else { 0 };
            if cut(path, from)? {
                left.push(path);
            }
        }
        if let [only] = left[..] {
            let _lock = queue_lock(pending_path)?;
            if !pending_path.exists() && claimed_bytes(pending_path) == file_len(only) {
                fs::rename(only, pending_path)?;
            }
        }
        Ok(())
    }

    /// Remove the claimed files: every record was dealt with.
    pub fn release(self) {
        for path in &self.files {
            let _ = fs::remove_file(path);
        }
    }
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Leave of the claimed file `path` what starts at byte `from`, as a claim
/// that counts as abandoned at once (its modification time is the epoch).
/// `false` when nothing is left and the file is gone. The file is replaced
/// in one step, never partly written.
fn cut(path: &Path, from: u64) -> io::Result<bool> {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if from >= file.metadata()?.len() {
        fs::remove_file(path)?;
        return Ok(false);
    }
    if from == 0 {
        OpenOptions::new()
            .append(true)
            .open(path)?
            .set_modified(SystemTime::UNIX_EPOCH)?;
        return Ok(true);
    }
    // A hidden name, so the half-written file is no claim to anybody.
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.rest"));
    let mut write = || -> io::Result<()> {
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(from))?;
        let mut out = fs::File::create(&tmp)?;
        io::copy(&mut file, &mut out)?;
        out.set_modified(SystemTime::UNIX_EPOCH)?;
        out.sync_all()?;
        fs::rename(&tmp, path)
    };
    write()
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
        .map(|()| true)
}

/// Where a [`Reader`] is in a claim: a record's first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    file: usize,
    offset: u64,
}

/// Reads a claim's records in order and can tell where the last one it
/// returned began, which is where a reader that could not send it stops:
/// [`Claimed::finish`] keeps what is from there on.
pub struct Reader<'a> {
    files: &'a [PathBuf],
    file: usize,
    open: Option<io::BufReader<fs::File>>,
    offset: u64,
    last: Position,
}

impl Reader<'_> {
    /// The next record, or `None` at the claim's end. Blank lines are passed
    /// over. Reading a file marks it as in use (see [`ABANDONED_AFTER`]).
    pub fn next_record(&mut self) -> io::Result<Option<String>> {
        loop {
            let Some(path) = self.files.get(self.file) else {
                self.last = Position {
                    file: self.file,
                    offset: 0,
                };
                return Ok(None);
            };
            if self.open.is_none() {
                match fs::File::open(path) {
                    Ok(f) => self.open = Some(io::BufReader::new(f)),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        self.file += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
                self.offset = 0;
            }
            let mut line = String::new();
            let read = self
                .open
                .as_mut()
                .expect("opened above")
                .read_line(&mut line)?;
            if read == 0 {
                self.open = None;
                self.file += 1;
                continue;
            }
            self.last = Position {
                file: self.file,
                offset: self.offset,
            };
            self.offset += read as u64;
            touch(path);
            let record = line.trim();
            if !record.is_empty() {
                return Ok(Some(record.to_string()));
            }
        }
    }

    /// Where the record [`Self::next_record`] returned last began: with
    /// [`Claimed::finish`], that record and all after it stay queued.
    pub fn before_last(&self) -> Position {
        self.last
    }
}

/// The two fields that decide what happens to a queued record, read without
/// building its transcript lines.
#[derive(serde::Deserialize)]
struct Head {
    #[serde(default)]
    tool: Option<String>,
    event_type: StreamEventType,
}

/// One line of a queue an earlier release wrote, as it is kept: `None` for a
/// blank or unreadable line, and for a record that is only lines of a
/// transcript file (they are still ahead of the file's read offset); an event
/// that carries such lines comes back without them.
pub fn compacted(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let head: Head = serde_json::from_str(line).ok()?;
    if !lines_are_reread(head.tool.as_deref()) {
        return Some(line.to_string());
    }
    if head.event_type == StreamEventType::Transcript {
        return None;
    }
    let mut record: StreamEventRequest = serde_json::from_str(line).ok()?;
    if record.transcript_lines.take().is_none() {
        return Some(line.to_string());
    }
    serde_json::to_string(&record).ok()
}

/// Rewrite the queue at `pending_path` as [`compacted`] keeps it, one record
/// at a time.
///
/// Every claim on the queue is taken over, whatever its age: this runs only
/// under the directory's upgrade lock and before its mark exists, when no
/// reader of this release can hold one, so a claim is what a compaction that
/// was killed left behind, still uncompacted.
fn compact(pending_path: &Path) -> io::Result<()> {
    let every_claim = SystemTime::now() + ABANDONED_AFTER + ABANDONED_AFTER;
    let claimed = claim_at(pending_path, every_claim)?;
    // Each file is replaced by its compacted self in one step, so a
    // compaction that is killed leaves files that are either done or not,
    // and doing a done one again changes nothing.
    for path in &claimed.files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let tmp = path.with_file_name(format!(".{name}.compacting"));
        let rewrite = || -> io::Result<()> {
            let mut out = io::BufWriter::new(fs::File::create(&tmp)?);
            for line in io::BufReader::new(fs::File::open(path)?).lines() {
                if let Some(kept) = compacted(&line?) {
                    writeln!(out, "{kept}")?;
                }
            }
            out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            fs::rename(&tmp, path)
        };
        rewrite().inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
    }
    // Hand everything back, unread.
    claimed.finish(Some(Position { file: 0, offset: 0 }), pending_path)
}

/// Marks a session directory whose queues hold every line once (see the
/// module documentation).
const UPGRADED: &str = ".queue-v2";

/// Bring the queues of `session_dir` up to "every line in one place", once:
/// a directory without the mark may hold queues an earlier release wrote,
/// which are [`compact`]ed. Called before a queue of the directory is read
/// or written. A new directory has no queue and only gets the mark.
///
/// One at a time per directory: a second caller that compacted after the
/// first had finished would take the lines the first then queued, which are
/// nowhere else, for an earlier release's.
pub fn upgrade(session_dir: &Path) -> io::Result<()> {
    let mark = session_dir.join(UPGRADED);
    if mark.exists() || !session_dir.is_dir() {
        return Ok(());
    }
    let _lock = lock(&session_dir.join(".queue.lock"))?;
    if mark.exists() {
        return Ok(());
    }
    // Queues, and queues that exist only as the claim of a compaction that
    // was killed.
    let queues: std::collections::BTreeSet<PathBuf> = match fs::read_dir(session_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| queue_of(&e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("pending"))
            })
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for queue in queues {
        compact(&queue)?;
    }
    fs::write(mark, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tool: Option<&str>, event_type: StreamEventType, lines: usize) -> StreamEventRequest {
        StreamEventRequest {
            protocol_version: 1,
            tool: tool.map(str::to_string),
            event_type,
            session_id: "s".into(),
            timestamp: chrono::Utc::now(),
            hook_event_name: Some("PostToolUse".into()),
            tool_name: Some("Bash".into()),
            tool_use_id: Some("t1".into()),
            tool_input: Some(serde_json::json!({"command": "ls"})),
            tool_response: None,
            tool_is_error: None,
            event_index: None,
            event_uuid: Some(uuid::Uuid::now_v7()),
            transcript_lines: (lines > 0)
                .then(|| (0..lines).map(|i| serde_json::json!({"n": i})).collect()),
            transcript_offset: Some(10),
            model: None,
            cwd: None,
            final_stats: None,
            flow_id: None,
            labels: None,
            params: None,
            billing_mode: None,
        }
    }

    /// How many files `dir` holds, the lock files aside.
    fn files(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                !e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".lock")
            })
            .count()
    }

    fn json(r: &StreamEventRequest) -> String {
        serde_json::to_string(r).unwrap()
    }

    fn parsed(json: &str) -> StreamEventRequest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn file_agents_reread_their_lines_and_plugin_agents_do_not() {
        assert!(lines_are_reread(None));
        assert!(lines_are_reread(Some("claude-code")));
        assert!(lines_are_reread(Some("codex")));
        assert!(lines_are_reread(Some("gsd")));
        assert!(!lines_are_reread(Some("opencode")));
        assert!(!lines_are_reread(Some("amp")));
    }

    #[test]
    fn without_lines_keeps_the_event_and_drops_bare_lines() {
        let event = record(Some("claude-code"), StreamEventType::ToolUse, 3);
        let kept = without_lines(event.clone()).unwrap();
        assert_eq!(kept.transcript_lines, None);
        assert_eq!(kept.event_uuid, event.event_uuid);
        assert_eq!(kept.tool_input, event.tool_input);

        let lines = record(Some("claude-code"), StreamEventType::Transcript, 3);
        assert!(without_lines(lines).is_none());
    }

    #[test]
    fn compacted_drops_reread_lines_and_keeps_everything_else() {
        // Bare lines of a file agent: gone.
        let lines = record(Some("claude-code"), StreamEventType::Transcript, 5);
        assert_eq!(compacted(&json(&lines)), None);
        // Its event: kept, without the lines.
        let event = record(None, StreamEventType::ToolUse, 5);
        let kept = parsed(&compacted(&json(&event)).unwrap());
        assert_eq!(kept.transcript_lines, None);
        assert_eq!(kept.event_uuid, event.event_uuid);
        assert_eq!(kept.transcript_offset, event.transcript_offset);
        // An event without lines: the line as it is.
        let plain = json(&record(Some("codex"), StreamEventType::SessionEnd, 0));
        assert_eq!(compacted(&plain).as_deref(), Some(plain.as_str()));
        // Inline records: untouched, lines and all.
        let inline = json(&record(Some("amp"), StreamEventType::Transcript, 5));
        assert_eq!(compacted(&inline).as_deref(), Some(inline.as_str()));
        // Not a record.
        assert_eq!(compacted(""), None);
        assert_eq!(compacted("   "), None);
        assert_eq!(compacted("{not json"), None);
    }

    /// A directory an earlier release wrote: every queue in it is compacted,
    /// once. After that its queues hold each line once and are left alone.
    #[test]
    fn upgrade_compacts_every_queue_of_the_directory_once() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir
            .path()
            .join("pending-11111111-1111-1111-1111-111111111111.jsonl");
        let project = dir
            .path()
            .join("pending-project-22222222-2222-2222-2222-222222222222.jsonl");
        let legacy = dir.path().join("pending.jsonl");
        let lines = json(&record(Some("claude-code"), StreamEventType::Transcript, 4));
        let event = record(Some("claude-code"), StreamEventType::ToolUse, 4);
        for queue in [&repo, &project, &legacy] {
            fs::write(queue, format!("{lines}\n{lines}\n{}\n", json(&event))).unwrap();
        }
        fs::write(dir.path().join("metadata.json"), "{}").unwrap();

        upgrade(dir.path()).unwrap();

        for queue in [&repo, &project, &legacy] {
            let after = fs::read_to_string(queue).unwrap();
            let records: Vec<&str> = after.lines().collect();
            assert_eq!(records.len(), 1, "{}", queue.display());
            assert_eq!(parsed(records[0]).transcript_lines, None);
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("metadata.json")).unwrap(),
            "{}"
        );

        // From now on a record with lines is the only copy of them.
        fs::write(&repo, format!("{lines}\n")).unwrap();
        upgrade(dir.path()).unwrap();
        assert_eq!(fs::read_to_string(&repo).unwrap(), format!("{lines}\n"));
    }

    /// A claim waits for a writer that has the queue open, so nothing is
    /// written into a file that was already read and removed.
    #[test]
    fn a_claim_waits_for_a_write_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let first = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        let late = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        fs::write(&path, format!("{first}\n")).unwrap();

        // A writer: holds the lock, has the file open, has not written yet.
        let writing = queue_lock(&path).unwrap();
        let mut open = OpenOptions::new().append(true).open(&path).unwrap();
        let claimer = {
            let path = path.clone();
            std::thread::spawn(move || {
                let claimed = claim(&path).unwrap();
                let got: Vec<String> = claimed.records().map(Result::unwrap).collect();
                claimed.release();
                got
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        assert!(!claimer.is_finished(), "the claim waits");
        writeln!(open, "{late}").unwrap();
        drop(open);
        drop(writing);

        assert_eq!(claimer.join().unwrap(), vec![first, late]);
    }

    #[test]
    fn a_claim_belongs_to_the_queue_it_was_taken_from() {
        let dir = Path::new("/s");
        assert_eq!(
            queue_of(&dir.join("pending-x.jsonl")),
            Some(dir.join("pending-x.jsonl"))
        );
        assert_eq!(
            queue_of(&dir.join("pending-x.jsonl.draining-0199")),
            Some(dir.join("pending-x.jsonl"))
        );
        assert_eq!(queue_of(&dir.join(".pending-x.jsonl.lock")), None);
        assert_eq!(queue_of(&dir.join("metadata.json")), None);
    }

    /// A claim does not make room: what a reader holds may all come back.
    #[test]
    fn a_claimed_queue_still_counts_against_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_QUEUE_BYTES - 4)
            .unwrap();
        let claimed = claim(&path).unwrap();
        assert!(!path.exists());
        assert!(!try_append(&path, "0123456789").unwrap(), "still full");
        claimed.release();
        assert!(try_append(&path, "0123456789").unwrap());
    }

    #[test]
    fn claims_read_as_one_keep_their_order() {
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("pending-project-p.jsonl");
        let newer = dir.path().join("pending-r.jsonl");
        let a = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        let b = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        fs::write(&older, format!("{a}\n")).unwrap();
        fs::write(&newer, format!("{b}\n")).unwrap();
        let claimed = claim(&older).unwrap().then(claim(&newer).unwrap());
        let got: Vec<String> = claimed.records().map(Result::unwrap).collect();
        assert_eq!(got, vec![a, b]);
        claimed.release();
        assert_eq!(files(dir.path()), 0);
    }

    fn queue_of_three(path: &Path) -> Vec<String> {
        let records: Vec<String> = (0..3)
            .map(|_| json(&record(Some("opencode"), StreamEventType::ToolUse, 1)))
            .collect();
        fs::write(path, format!("{}\n", records.join("\n"))).unwrap();
        records
    }

    fn read_all(path: &Path) -> Vec<String> {
        let claimed = claim_at(path, SystemTime::now() + ABANDONED_AFTER * 2).unwrap();
        let got = claimed.records().map(Result::unwrap).collect();
        claimed
            .finish(Some(Position { file: 0, offset: 0 }), path)
            .unwrap();
        got
    }

    /// A reader that stops at a record leaves it and the ones after it
    /// queued, once, under the queue's own name when nothing else is there.
    #[test]
    fn finishing_part_way_keeps_exactly_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let records = queue_of_three(&path);

        let claimed = claim(&path).unwrap();
        let mut reader = claimed.reader();
        assert_eq!(reader.next_record().unwrap().as_ref(), Some(&records[0]));
        assert_eq!(reader.next_record().unwrap().as_ref(), Some(&records[1]));
        let stopped = reader.before_last();
        drop(reader);
        claimed.finish(Some(stopped), &path).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}\n{}\n", records[1], records[2])
        );
        assert_eq!(files(dir.path()), 1);
    }

    /// A reader that dies after reading part of its claim, or while giving
    /// it up, leaves each record in one file: whoever recovers it reads
    /// every record once, however often that happens.
    #[test]
    fn a_reader_that_dies_leaves_no_record_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let records = queue_of_three(&path);

        for _ in 0..3 {
            // Reads two records, puts nothing back, and is gone.
            let claimed = claim_at(&path, SystemTime::now() + ABANDONED_AFTER * 2).unwrap();
            let mut reader = claimed.reader();
            reader.next_record().unwrap();
            reader.next_record().unwrap();
            drop(reader);
            std::mem::forget(claimed);
        }
        assert_eq!(read_all(&path), records);
        assert_eq!(read_all(&path), records, "handing back repeats nothing");
        assert!(file_len(&path) > 0 && files(dir.path()) == 1);
    }

    /// While something else was queued meanwhile, the rest stays a claim
    /// that the next reader takes first, at once.
    #[test]
    fn the_rest_is_read_ahead_of_what_was_queued_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let records = queue_of_three(&path);
        let newer = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));

        let claimed = claim(&path).unwrap();
        let mut reader = claimed.reader();
        reader.next_record().unwrap();
        let stopped = reader.before_last();
        drop(reader);
        assert!(try_append(&path, &newer).unwrap());
        claimed.finish(Some(stopped), &path).unwrap();

        let next = claim(&path).unwrap();
        let got: Vec<String> = next.records().map(Result::unwrap).collect();
        let mut want = records.clone();
        want.push(newer);
        assert_eq!(got, want);
        next.release();
        assert_eq!(files(dir.path()), 0);
    }

    /// An adopted queue's files become claims on the queue that adopts it,
    /// so a recovery sends them where the adoption was sending them.
    #[test]
    fn an_adopted_claim_is_recovered_as_the_adopting_queue() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("pending-project-p.jsonl");
        let repo = dir.path().join("pending-r.jsonl");
        let records = queue_of_three(&project);

        let claimed = claim(&project).unwrap().retarget(&repo).unwrap();
        // The hook dies here.
        std::mem::forget(claimed);

        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with(".lock"))
            .collect();
        assert_eq!(names.len(), 1);
        assert!(
            names[0].starts_with("pending-r.jsonl.draining-"),
            "{names:?}"
        );
        assert_eq!(queue_of(&dir.path().join(&names[0])), Some(repo.clone()));
        assert_eq!(read_all(&repo), records);
        assert!(read_all(&project).is_empty());
    }

    #[test]
    fn a_claimed_record_goes_back_whatever_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_QUEUE_BYTES)
            .unwrap();
        assert!(!try_append(&path, "new").unwrap());
        requeue(&path, "claimed").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_QUEUE_BYTES + 8);
    }

    /// A second upgrade waits for the first and then finds the mark, so it
    /// does not compact what was queued in between.
    #[test]
    fn upgrade_waits_for_one_in_progress_and_then_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let lines = json(&record(Some("claude-code"), StreamEventType::Transcript, 4));
        let held = lock(&dir.path().join(".queue.lock")).unwrap();

        let session_dir = dir.path().to_path_buf();
        let waiting = std::thread::spawn(move || upgrade(&session_dir));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiting.is_finished(), "it waits for the lock");
        // The holder finishes: marks the directory and queues lines.
        fs::write(dir.path().join(UPGRADED), "").unwrap();
        fs::write(&path, format!("{lines}\n")).unwrap();
        drop(held);

        waiting.join().unwrap().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{lines}\n"));
    }

    /// A compaction that was killed leaves the old queue under a claim's
    /// name. The next upgrade finishes it, however recent the claim, instead
    /// of marking the directory with the old records still in it.
    #[test]
    fn upgrade_finishes_a_compaction_that_was_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let lines = json(&record(Some("claude-code"), StreamEventType::Transcript, 4));
        let event = record(Some("claude-code"), StreamEventType::ToolUse, 4);
        fs::write(
            dir.path().join("pending-x.jsonl.draining-killed"),
            format!("{lines}\n{lines}\n{}\n", json(&event)),
        )
        .unwrap();

        upgrade(dir.path()).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        let records: Vec<&str> = after.lines().collect();
        assert_eq!(records.len(), 1);
        assert_eq!(parsed(records[0]).event_uuid, event.event_uuid);
        assert_eq!(parsed(records[0]).transcript_lines, None);
        assert!(!dir.path().join("pending-x.jsonl.draining-killed").exists());
    }

    /// A claim that is still being read is not taken for abandoned.
    #[test]
    fn reading_a_claim_keeps_it_from_being_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let a = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        fs::write(&path, format!("{a}\n{a}\n")).unwrap();
        let claimed = claim(&path).unwrap();
        let file = claimed.files[0].clone();
        let long_ago = SystemTime::now() - ABANDONED_AFTER - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();

        let mut records = claimed.records();
        records.next().unwrap().unwrap();
        let other = claim(&path).unwrap();
        assert!(other.is_empty(), "the reader is alive");
        drop(records);
        claimed.release();
    }

    #[test]
    fn upgrade_of_a_new_or_missing_directory_only_marks_it() {
        let dir = tempfile::tempdir().unwrap();
        upgrade(dir.path()).unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != ".queue.lock")
            .collect();
        assert_eq!(names, [UPGRADED]);
        upgrade(&dir.path().join("absent")).unwrap();
        assert!(!dir.path().join("absent").exists());
    }

    #[test]
    fn compact_shrinks_a_queue_that_repeats_its_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let event = record(Some("claude-code"), StreamEventType::ToolUse, 2);
        let mut content = String::new();
        for _ in 0..50 {
            let lines = record(Some("claude-code"), StreamEventType::Transcript, 200);
            content.push_str(&json(&lines));
            content.push('\n');
        }
        content.push_str(&json(&event));
        content.push('\n');
        fs::write(&path, &content).unwrap();

        compact(&path).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        let records: Vec<&str> = after.lines().collect();
        assert_eq!(records.len(), 1);
        assert_eq!(parsed(records[0]).event_uuid, event.event_uuid);
        assert!(after.len() < content.len() / 50);
        // Nothing of the claim is left behind.
        assert_eq!(files(dir.path()), 1);
    }

    #[test]
    fn compact_of_a_missing_queue_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        compact(&dir.path().join("pending-x.jsonl")).unwrap();
        assert_eq!(files(dir.path()), 0);
    }

    #[test]
    fn a_claim_takes_the_queue_away_and_records_come_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let a = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        let b = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        fs::write(&path, format!("{a}\n\n{b}\n")).unwrap();

        let claimed = claim(&path).unwrap();
        assert!(!path.exists(), "a hook queueing now starts a new file");
        let got: Vec<String> = claimed.records().map(Result::unwrap).collect();
        assert_eq!(got, vec![a, b]);
        claimed.release();
        assert_eq!(files(dir.path()), 0);
    }

    #[test]
    fn an_abandoned_claim_is_taken_over_and_a_fresh_one_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let old = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        let new = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        let other = json(&record(Some("opencode"), StreamEventType::ToolUse, 1));
        let abandoned = dir.path().join("pending-x.jsonl.draining-dead");
        fs::write(&abandoned, format!("{old}\n")).unwrap();
        fs::write(&path, format!("{new}\n")).unwrap();
        // Another target's claim is not ours to take.
        let foreign = dir.path().join("pending-y.jsonl.draining-dead");
        fs::write(&foreign, format!("{other}\n")).unwrap();

        // Now: the other reader may still be at work.
        let claimed = claim(&path).unwrap();
        let got: Vec<String> = claimed.records().map(Result::unwrap).collect();
        assert_eq!(got, vec![new.clone()]);
        assert!(abandoned.exists());
        claimed.release();

        // Later: it is taken over, ahead of the queue itself.
        fs::write(&path, format!("{new}\n")).unwrap();
        let later = SystemTime::now() + ABANDONED_AFTER + Duration::from_secs(1);
        let claimed = claim_at(&path, later).unwrap();
        let got: Vec<String> = claimed.records().map(Result::unwrap).collect();
        assert_eq!(got, vec![old, new]);
        assert!(!abandoned.exists());
        claimed.release();
        assert!(foreign.exists());
        assert_eq!(files(dir.path()), 1);
    }

    #[test]
    fn a_full_queue_takes_no_more() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-x.jsonl");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_QUEUE_BYTES - 10).unwrap();
        assert!(!try_append(&path, "0123456789abcdef").unwrap());
        append(&path, "0123456789abcdef").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_QUEUE_BYTES - 10);
        assert!(try_append(&path, "01234567").unwrap());
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_QUEUE_BYTES - 1);
    }
}
