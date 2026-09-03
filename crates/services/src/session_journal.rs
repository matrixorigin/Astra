//! Session Journal — local JSONL persistence for observability & auditability.
//!
//! Writes one line per event to
//! `~/.astra/sessions/v1/users/<owner>/sessions/<session_id>.jsonl`.
//! Events include: turn completions, config changes, errors, compactions.
//!
//! The journal is append-only and survives process exits.
//! It can be replayed, exported, or analyzed by `/session` commands.
//!
//! **Test isolation:** use [`JournalDirGuard`] to redirect all `sessions`-rooted I/O on the
//! current thread (journal JSONL, workspace, step checkpoints) without mutating `HOME`. Use
//! [`ProcessJournalDirGuard`] only for integration tests that must observe journal writes from
//! background tasks running on different Tokio worker threads.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicU64, Ordering},
};

use astra_core::canonical_names::{normalize_name_list, normalize_optional_name};
use astra_turn_types::{
    ConversationCommitV1, InferencePurpose, SessionCursorV1, UserIntentDelivery, UserIntentStatus,
};

use crate::interaction_contract::{
    InteractionContract, InteractionIdentity, InteractionKind, InteractionStatus,
    approval_decision_status, ask_user_response_status,
};
use crate::{OwnerScope, SessionArtifactStore};

thread_local! {
    static LOCAL_SESSIONS_DIR_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    #[cfg(test)]
    static JOURNAL_DIRECTORY_SYNC_FAILURE_ONCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Debug, Clone)]
struct ProcessSessionsDirOverride {
    id: u64,
    dir: PathBuf,
}

static NEXT_PROCESS_SESSIONS_DIR_OVERRIDE_ID: AtomicU64 = AtomicU64::new(1);
static PROCESS_SESSIONS_DIR_OVERRIDE_POISONED_COUNT: AtomicU64 = AtomicU64::new(0);
static PROCESS_SESSIONS_DIR_OVERRIDES: LazyLock<Mutex<Vec<ProcessSessionsDirOverride>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

#[cfg(test)]
struct ApprovalAppendOpenHook {
    path: PathBuf,
    opened: std::sync::mpsc::SyncSender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static APPROVAL_APPEND_OPEN_HOOK: LazyLock<Mutex<Option<ApprovalAppendOpenHook>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
struct CanonicalCommitCasOpenHook {
    path: PathBuf,
    opened: std::sync::mpsc::SyncSender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static CANONICAL_COMMIT_CAS_OPEN_HOOK: LazyLock<Mutex<Option<CanonicalCommitCasOpenHook>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
fn run_approval_append_open_hook(path: &Path) {
    let hook = APPROVAL_APPEND_OPEN_HOOK.lock().ok().and_then(|mut slot| {
        slot.as_ref()
            .is_some_and(|hook| hook.path == path)
            .then(|| slot.take())
            .flatten()
    });
    if let Some(hook) = hook {
        let _ = hook.opened.send(());
        let _ = hook.resume.recv();
    }
}

#[cfg(test)]
fn run_canonical_commit_cas_open_hook(path: &Path) {
    let hook = CANONICAL_COMMIT_CAS_OPEN_HOOK
        .lock()
        .ok()
        .and_then(|mut slot| {
            slot.as_ref()
                .is_some_and(|hook| hook.path == path)
                .then(|| slot.take())
                .flatten()
        });
    if let Some(hook) = hook {
        let _ = hook.opened.send(());
        let _ = hook.resume.recv();
    }
}

/// Cargo places unit-, integration-, and benchmark-test executables under a
/// `target/{profile}/deps/<name>-<16 hex>` path.  A library dependency is not
/// compiled with `cfg(test)` for integration tests, so relying on that cfg (or
/// on every individual test remembering to install a guard) lets test journals
/// escape into the real `~/.astra` directory.  Resolve one process-scoped
/// fallback under `target/test-state` instead.  Explicit thread/process guards
/// still take precedence below.
static CARGO_TEST_PROCESS_SESSIONS_DIR: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    cargo_test_process_sessions_dir_for(&std::env::current_exe().ok()?, std::process::id())
});

fn cargo_test_process_sessions_dir_for(executable: &Path, process_id: u32) -> Option<PathBuf> {
    let deps_dir = executable.parent()?;
    if deps_dir.file_name().and_then(|name| name.to_str()) != Some("deps") {
        return None;
    }
    let executable_name = executable.file_name()?.to_str()?;
    let (_, hash) = executable_name.rsplit_once('-')?;
    if hash.len() != 16 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let target_dir = deps_dir.parent()?.parent()?;
    Some(
        target_dir
            .join("test-state")
            .join(format!("{executable_name}-{process_id}"))
            .join("sessions"),
    )
}

fn merge_execution_boundary_metadata(
    metadata: &mut serde_json::Value,
    execution_metadata: Option<&serde_json::Value>,
) {
    let Some(metadata) = metadata.as_object_mut() else {
        return;
    };
    let Some(execution_metadata) = execution_metadata.and_then(serde_json::Value::as_object) else {
        return;
    };
    for key in ["workspace", "executor", "transport"] {
        if let Some(value) = execution_metadata.get(key).cloned() {
            metadata.entry(key.to_string()).or_insert(value);
        }
    }
}

/// Bounded cache for session-start state to prevent unbounded memory growth
/// in long-running `astra serve` processes.  Uses FIFO eviction: when the
/// entry count reaches `MAX`, the oldest inserted entry is evicted before
/// inserting the new entry.  This bounds worst-case memory to ~N PathBuf entries
/// while preserving the most recently cached lookups.
struct BoundedSessionCache {
    entries: HashMap<PathBuf, bool>,
    /// Insertion-order queue for FIFO eviction.
    order: VecDeque<PathBuf>,
}

impl BoundedSessionCache {
    /// Maximum number of cached session paths before eviction triggers.
    const MAX: usize = 1024;

    fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(Self::MAX),
            order: VecDeque::with_capacity(Self::MAX),
        }
    }

    fn get(&self, path: &Path) -> Option<bool> {
        self.entries.get(path).copied()
    }

    fn insert(&mut self, path: PathBuf, value: bool) {
        let is_new = !self.entries.contains_key(&path);
        // Evict oldest entry (FIFO) only when inserting a net-new key at capacity.
        if is_new
            && self.entries.len() >= Self::MAX
            && let Some(evicted) = self.order.pop_front()
        {
            self.entries.remove(&evicted);
        }
        if is_new {
            self.order.push_back(path.clone());
        }
        self.entries.insert(path, value);
    }
}

static SESSION_START_STATE_CACHE: LazyLock<Mutex<BoundedSessionCache>> =
    LazyLock::new(|| Mutex::new(BoundedSessionCache::new()));

fn with_session_start_state_cache<R>(f: impl FnOnce(&mut BoundedSessionCache) -> R) -> R {
    match SESSION_START_STATE_CACHE.lock() {
        Ok(mut guard) => f(&mut guard),
        Err(mut poisoned) => {
            tracing::warn!(
                "session_start_state_cache mutex poisoned; clearing cached state before reuse"
            );
            poisoned.get_mut().order.clear();
            poisoned.get_mut().entries.clear();
            let mut guard = poisoned.into_inner();
            f(&mut guard)
        }
    }
}

/// Resolved local `sessions` directory (`~/.astra/sessions` or a test override).
///
/// Step checkpoints, workspace metadata, and session journal files all live under this root.
pub fn local_sessions_dir() -> PathBuf {
    LOCAL_SESSIONS_DIR_OVERRIDE.with(|c| {
        if let Some(ref p) = *c.borrow() {
            return p.clone();
        }
        let process_override = match PROCESS_SESSIONS_DIR_OVERRIDES.lock() {
            Ok(overrides) => overrides.last().map(|override_| override_.dir.clone()),
            Err(poisoned) => {
                PROCESS_SESSIONS_DIR_OVERRIDE_POISONED_COUNT.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "process_sessions_dir_overrides mutex poisoned; using last stored override"
                );
                poisoned
                    .into_inner()
                    .last()
                    .map(|override_| override_.dir.clone())
            }
        };
        if let Some(p) = process_override {
            return p;
        }
        if let Some(p) = CARGO_TEST_PROCESS_SESSIONS_DIR.as_ref() {
            return p.clone();
        }
        astra_runtime_env::local_state_root().join("sessions")
    })
}

fn cached_session_start_state(path: &Path) -> Option<bool> {
    with_session_start_state_cache(|cache| cache.get(path))
}

fn set_cached_session_start_state(path: &Path, has_open_session_start: bool) {
    with_session_start_state_cache(|cache| {
        cache.insert(path.to_path_buf(), has_open_session_start);
    });
}

fn update_cached_session_start_state_from_event(path: &Path, event_type: &JournalEventType) {
    match event_type {
        JournalEventType::SessionStart => set_cached_session_start_state(path, true),
        JournalEventType::SessionEnd => set_cached_session_start_state(path, false),
        _ => {}
    }
}

fn update_cached_session_start_state_from_events(path: &Path, events: &[JournalEvent]) {
    if let Some(last_type) = events
        .iter()
        .rev()
        .map(|event| &event.event_type)
        .find(|event_type| {
            matches!(
                event_type,
                JournalEventType::SessionStart | JournalEventType::SessionEnd
            )
        })
    {
        update_cached_session_start_state_from_event(path, last_type);
    }
}

/// Tiebreaker rank for events that share a timestamp.
///
/// Lower rank sorts earlier.  Only `SessionStart` and `SessionEnd` carry
/// meaningful tiebreaks (the boundary events must envelope same-timestamp
/// inner events); everything else stays in insertion order via the stable
/// sort.  This is **explicit on purpose** — relying on `#[derive(Ord)]`
/// over the variant list silently inverts boundary semantics if a future
/// refactor reorders the enum.
fn event_type_tiebreak_rank(event_type: &JournalEventType) -> u8 {
    match event_type {
        JournalEventType::SessionStart => 0,
        JournalEventType::SessionEnd => 2,
        _ => 1,
    }
}

fn stabilize_event_order(events: &mut [JournalEvent]) {
    // Fast path: already chronological by file layout, no same-timestamp
    // boundary tiebreak inversion, no leading SessionStart that needs to be
    // lifted.  `read_journal` is in the steady-state hot path — re-sorting
    // thousands of events on every read is wasteful when the writer
    // guarantees append-time chronological order.
    //
    // Detection must consider the same compound key the slow path uses,
    // otherwise same-timestamp boundary misorders silently pass through:
    //   - `pair[0].ts > pair[1].ts`             — chronological inversion
    //   - `pair[0].ts == pair[1].ts && rank>` — boundary tiebreak inversion
    let needs_resort = events
        .windows(2)
        .any(|pair| match pair[0].ts.cmp(&pair[1].ts) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => {
                event_type_tiebreak_rank(&pair[0].event_type)
                    > event_type_tiebreak_rank(&pair[1].event_type)
            }
            std::cmp::Ordering::Less => false,
        });
    let needs_session_start_lift = events
        .iter()
        .position(|event| event.event_type == JournalEventType::SessionStart)
        .is_some_and(|idx| idx > 0);
    if !needs_resort && !needs_session_start_lift {
        return;
    }
    // Stable sort by (timestamp, explicit tiebreak rank). Boundary events
    // (SessionStart/SessionEnd) take their natural envelope position; all
    // other events keep insertion order via stable sort.
    events.sort_by(|left, right| {
        left.ts.cmp(&right.ts).then_with(|| {
            event_type_tiebreak_rank(&left.event_type)
                .cmp(&event_type_tiebreak_rank(&right.event_type))
        })
    });
    if let Some(first_session_start) = events
        .iter()
        .position(|event| event.event_type == JournalEventType::SessionStart)
        && first_session_start > 0
    {
        events[..=first_session_start].rotate_right(1);
    }
}

fn journal_needs_session_start_for_path(path: &Path) -> std::io::Result<bool> {
    journal_needs_session_start_impl(path, /*skip_cache=*/ false)
}

/// Core implementation shared by cached and uncached callers.
///
/// When `skip_cache` is true, the process-local `SESSION_START_STATE_CACHE` is
/// not consulted for early return.  This is required when the caller already
/// holds the file lock: between two lock acquisitions in the same process,
/// another process (e.g. edge-cloud sync) may have written to the file and
/// updated *its* cache — but our process-local cache still holds the old value.
/// See `ensure_session_start_event` for the lock-protected call site.
fn journal_needs_session_start_impl(path: &Path, skip_cache: bool) -> std::io::Result<bool> {
    let is_missing_or_empty = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len() == 0,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => return Err(err),
    };
    if is_missing_or_empty {
        set_cached_session_start_state(path, false);
        return Ok(true);
    }
    if !skip_cache && let Some(has_open_session_start) = cached_session_start_state(path) {
        return Ok(!has_open_session_start);
    }
    // Hot-path: the legacy logic only depends on the *last* event.  If the
    // last event is `SessionEnd` (or the file has no parseable events),
    // we need a new `SessionStart`; otherwise an open session exists and
    // we don't.  The position of older boundary events does not change
    // this answer because the journal is append-only chronological in
    // steady state — once a `SessionStart` is followed by *any*
    // non-`SessionEnd` event, that session is open.
    //
    // Reading the entire file just to find one event is O(file size) on
    // every invocation; for long-running multi-megabyte journals it
    // dominates per-append latency.  We instead read only the tail.
    let needs_session_start = read_last_event_type(path)?
        .is_none_or(|event_type| event_type == JournalEventType::SessionEnd);
    set_cached_session_start_state(path, !needs_session_start);
    Ok(needs_session_start)
}

/// Outcome of [`read_last_event_type_with_bytes`].  The byte count is
/// exposed for tests that need to assert algorithmic complexity (bounded
/// I/O regardless of file size) without relying on wall-clock timing.
#[derive(Debug, Clone)]
struct LastEventScan {
    event_type: Option<JournalEventType>,
    /// Bytes actually read from the file.  Always ≤ `RECOVERY_TAIL_MAX_BYTES`.
    /// Independent of file size by construction.
    bytes_read: u64,
}

/// Read **only the last parseable event's `event_type`** from the journal.
///
/// Walks backwards from EOF in `RECOVERY_TAIL_CHUNK_BYTES`-sized chunks,
/// returning the first event we successfully decode.  Stops as soon as a
/// match is found — typically one chunk read for any non-empty journal.
/// Returns `None` if the file is empty or no line in the tail window is a
/// parseable JournalEvent.
fn read_last_event_type(path: &Path) -> std::io::Result<Option<JournalEventType>> {
    let scan = read_last_event_type_with_bytes(path)?;
    debug_assert!(scan.bytes_read <= RECOVERY_TAIL_MAX_BYTES as u64);
    record_journal_read(
        astra_core::history_work::HistoryWorkSite::SessionJournalTailRead,
        scan.bytes_read.try_into().unwrap_or(usize::MAX),
        usize::from(scan.event_type.is_some()),
    );
    Ok(scan.event_type)
}

fn read_last_event_type_with_bytes(path: &Path) -> std::io::Result<LastEventScan> {
    use std::io::{Read as _, Seek as _};
    let mut file = std::fs::File::open(path)?;
    let total = file.seek(std::io::SeekFrom::End(0))?;
    if total == 0 {
        return Ok(LastEventScan {
            event_type: None,
            bytes_read: 0,
        });
    }
    let mut pos = total;
    let mut bytes_read: u64 = 0;
    let mut carry: Vec<u8> = Vec::new();
    while pos > 0 && bytes_read < RECOVERY_TAIL_MAX_BYTES as u64 {
        let read_len = u64::min(RECOVERY_TAIL_CHUNK_BYTES as u64, pos);
        pos -= read_len;
        file.seek(std::io::SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; read_len as usize];
        file.read_exact(&mut chunk)?;
        bytes_read += read_len;
        // `chunk` now holds bytes [pos, pos+read_len).  Append `carry`
        // (the partial leading line from the *previous* iteration, which
        // covers bytes that were positionally *after* this chunk) so each
        // chunk's complete lines can be parsed in isolation.
        chunk.extend_from_slice(&carry);
        let mut iter = chunk.split(|&b| b == b'\n').collect::<Vec<_>>();
        // First element is a partial leading line iff we did not read
        // from offset 0; carry forward to merge with the previous chunk.
        let leading = if pos > 0 {
            iter.remove(0).to_vec()
        } else {
            Vec::new()
        };
        // Walk lines from newest to oldest (reverse order: the chunk
        // ends with the newest event).  Return the first parseable
        // event_type we find — it is by construction the last event in
        // the journal.
        for line in iter.iter().rev() {
            let line = std::str::from_utf8(line).unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(event_type) = peek_event_type(line) {
                return Ok(LastEventScan {
                    event_type: Some(event_type),
                    bytes_read,
                });
            }
        }
        carry = leading;
    }
    // Tail window exhausted without finding a parseable event.  The
    // remaining `carry` bytes are the partial leading line of the OLDEST
    // chunk we processed — its missing prefix lies in unread bytes
    // earlier in the file.  Without that prefix we cannot reconstruct
    // the line, so it MUST be discarded.  Returning `None` lets the
    // caller fall back to the safe default ("needs SessionStart").  The
    // budget cap is intentional: a journal whose entire tail window is
    // a single mega-event is pathological and must not pin one fd's
    // tail-scan to unbounded I/O.
    let _ = carry;
    Ok(LastEventScan {
        event_type: None,
        bytes_read,
    })
}

/// Cheap event-type peek: extracts the `event_type` field without
/// deserializing the entire JournalEvent.  Avoids the overhead of
/// constructing all the optional fields/metadata during the hot path.
///
/// Returns `None` for malformed JSON or unknown event_type values
/// (forward compatibility — a future variant we don't recognize must not
/// crash the writer).
fn peek_event_type(line: &str) -> Option<JournalEventType> {
    #[derive(serde::Deserialize)]
    struct Peek {
        // Wire format uses `type` (matches `JournalEvent#[serde(rename =
        // "type")] pub event_type`).  Past mistakes proved this is the
        // sort of mismatch that fails silently — the field name is part
        // of the journal's public contract.
        #[serde(rename = "type")]
        event_type: JournalEventType,
    }
    serde_json::from_str::<Peek>(line)
        .ok()
        .map(|p| p.event_type)
}

fn prepend_session_start_if_needed<'a>(
    path: &Path,
    events: &'a [JournalEvent],
) -> std::io::Result<Cow<'a, [JournalEvent]>> {
    let needs_session_start = journal_needs_session_start_for_path(path)?;
    Ok(prepend_session_start_for_known_state(
        events,
        needs_session_start,
    ))
}

fn prepend_session_start_for_known_state(
    events: &[JournalEvent],
    needs_session_start: bool,
) -> Cow<'_, [JournalEvent]> {
    if events.is_empty()
        || events
            .iter()
            .any(|event| event.event_type == JournalEventType::SessionStart)
        || !needs_session_start
    {
        return Cow::Borrowed(events);
    }
    let Some(seed) = events
        .iter()
        .find(|event| event.event_type != JournalEventType::SessionStart)
    else {
        return Cow::Borrowed(events);
    };
    let mut session_start =
        JournalEvent::session_start(seed.session_id.as_deref(), seed.model.as_deref());
    // Set ts to 1µs before the seed event so SessionStart is guaranteed
    // chronologically first.  On a parse failure we fall back to cloning the
    // seed timestamp as-is rather than Utc::now(), so the prepended event
    // never ends up *after* the seed.
    if let Ok(seed_ts) = chrono::DateTime::parse_from_rfc3339(&seed.ts) {
        session_start.ts = (seed_ts - chrono::Duration::microseconds(1)).to_rfc3339();
    } else {
        session_start.ts = seed.ts.clone();
    }
    let mut prefixed = Vec::with_capacity(events.len() + 1);
    prefixed.push(session_start);
    prefixed.extend(events.iter().cloned());
    Cow::Owned(prefixed)
}

/// Opens a journal file with an exclusive file lock.
///
/// On Linux, `flock()` locks are associated with open file descriptions, and on
/// kernels ≥ 2.6.12 same-process conflicts are detected (the second `lock_exclusive()`
/// either blocks on the same fd or fails with `EAGAIN` on a different fd).  This
/// guarantees that concurrent writes — including from within the same process —
/// are serialized.  The `concurrent_first_writes_prepend_single_session_start`
/// test validates this invariant.
///
/// Permission handling: `0o600` is applied only on the path that actually
/// creates the file.  This rules out the TOCTOU window where
/// `path.exists()` says false but a concurrent process creates the file
/// before our `open()` call — under the previous "stat then create" pattern
/// we would chmod a file we did not create, silently retitling its
/// permissions.  We use `create_new(true)` first; if it fails because the
/// file already exists, fall through to a plain append-open without chmod.
fn open_locked_journal_file(path: &Path) -> std::io::Result<std::fs::File> {
    use fs2::FileExt;
    let file = open_unlocked_journal_file(path)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn missing_directories_before_create(path: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        if current.exists() {
            break;
        }
        missing.push(current.to_path_buf());
        candidate = current.parent();
    }
    missing
}

fn open_unlocked_journal_file(path: &Path) -> std::io::Result<std::fs::File> {
    Ok(open_unlocked_journal_file_with_creation(path)?.file)
}

struct OpenedJournalFile {
    file: std::fs::File,
    created: bool,
}

fn open_unlocked_journal_file_with_creation(path: &Path) -> std::io::Result<OpenedJournalFile> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let create_attempt = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .append(true)
        .open(path);
    let (file, we_created_it) = match create_attempt {
        Ok(file) => (file, true),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .append(true)
                .open(path)?;
            (file, false)
        }
        Err(err) => return Err(err),
    };
    #[cfg(unix)]
    if we_created_it {
        use std::os::unix::fs::PermissionsExt;
        // Restrict to owner-only.  We just created the file, so this is
        // the *only* moment we will ever chmod it — once flock is taken
        // any subsequent appender will skip the chmod path entirely.
        // Failing to chmod a file we own is unexpected; surface the
        // error so misconfigured filesystems (e.g. read-only mounts) are
        // not silently masked.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = we_created_it;
    }
    Ok(OpenedJournalFile {
        file,
        created: we_created_it,
    })
}

fn serialize_journal_events(events: &[JournalEvent]) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    for event in events {
        let mut line = serde_json::to_vec(event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        if matches!(
            event.event_type,
            JournalEventType::LlmRequestFull | JournalEventType::ContextAssemblyRecorded
        ) && astra_core::history_work::instrumentation_enabled()
        {
            astra_core::history_work::record_operation(
                astra_core::history_work::HistoryWorkSite::SessionJournalHistorySerialization,
                line.len().try_into().unwrap_or(u64::MAX),
                1,
                0,
            );
        }
        buf.extend(line);
    }
    Ok(buf)
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if JOURNAL_DIRECTORY_SYNC_FAILURE_ONCE.with(|fail| fail.replace(false)) {
        return Err(std::io::Error::other(
            "injected journal directory sync failure",
        ));
    }
    std::fs::File::open(path)?.sync_all()
}

#[cfg(test)]
fn fail_next_journal_directory_sync() {
    JOURNAL_DIRECTORY_SYNC_FAILURE_ONCE.with(|fail| fail.set(true));
}

fn record_journal_read(site: astra_core::history_work::HistoryWorkSite, bytes: usize, rows: usize) {
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_operation(
            site,
            bytes.try_into().unwrap_or(u64::MAX),
            rows.try_into().unwrap_or(u64::MAX),
            0,
        );
    }
}

/// Validate that a session ID is safe for use as a filesystem path component.
///
/// Delegates to [`astra_core::session_id::validate`] — the single source of truth.
pub fn validate_session_id(session_id: &str) -> Result<(), String> {
    astra_core::session_id::validate(session_id)
}

/// Redirect session journal + workspace + step checkpoint paths on **this thread** to `dir`.
///
/// `dir` must be the `sessions` folder (the directory that will contain `<session_id>.jsonl`
/// files and `<session_id>/` subdirectories). Nestable: dropping restores the previous override.
#[must_use = "drop restores the previous sessions-dir override for this thread"]
pub struct JournalDirGuard {
    previous: Option<PathBuf>,
}

impl JournalDirGuard {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        let previous = LOCAL_SESSIONS_DIR_OVERRIDE.with(|c| (*c.borrow_mut()).replace(dir));
        Self { previous }
    }
}

impl Drop for JournalDirGuard {
    fn drop(&mut self) {
        let prev = self.previous.take();
        LOCAL_SESSIONS_DIR_OVERRIDE.with(|c| {
            *c.borrow_mut() = prev;
        });
    }
}

/// Returns the current thread's test/local sessions-root override, if one is
/// installed. Callers that offload journal work to a blocking thread can carry
/// this explicit scope across the thread boundary; thread-local state is not
/// inherited by Tokio workers.
#[must_use]
pub fn current_journal_dir_override() -> Option<PathBuf> {
    LOCAL_SESSIONS_DIR_OVERRIDE.with(|cell| cell.borrow().clone())
}

/// Redirect session journal + workspace + step checkpoint paths process-wide.
///
/// Use this only in tests that intentionally exercise cross-thread async
/// background work. Prefer [`JournalDirGuard`] for ordinary single-threaded
/// unit tests, because this guard affects every thread in the current process.
#[must_use = "drop restores the previous process-wide sessions-dir override"]
pub struct ProcessJournalDirGuard {
    id: u64,
}

impl ProcessJournalDirGuard {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        let id = NEXT_PROCESS_SESSIONS_DIR_OVERRIDE_ID.fetch_add(1, Ordering::Relaxed);
        PROCESS_SESSIONS_DIR_OVERRIDES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(ProcessSessionsDirOverride {
                id,
                dir: dir.clone(),
            });
        Self { id }
    }
}

impl Drop for ProcessJournalDirGuard {
    fn drop(&mut self) {
        let mut overrides = PROCESS_SESSIONS_DIR_OVERRIDES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if overrides
            .last()
            .is_some_and(|override_| override_.id == self.id)
        {
            overrides.pop();
        } else if let Some(index) = overrides
            .iter()
            .rposition(|override_| override_.id == self.id)
        {
            overrides.remove(index);
        }
    }
}

/// Session state change tracking for edge-cloud sync.
/// Records mutations as deltas instead of overwriting full state.
pub mod state_delta {
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    /// Change operation type for session state mutations.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum StateChangeOp {
        Create,
        Update,
        Delete,
    }

    /// A single state change entry for session mutations.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct StateChange {
        /// Monotonic version number within the session.
        pub version: u64,
        /// Timestamp in milliseconds.
        pub timestamp_ms: u64,
        /// The state key being mutated.
        pub key: String,
        /// The operation type.
        pub op: StateChangeOp,
        /// The value (None for Delete).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub value: Option<serde_json::Value>,
        /// Turn number when the change occurred.
        pub turn: u32,
    }

    /// Accumulates session state changes for delta sync.
    pub struct SessionStateAccumulator {
        version_counter: u64,
        entries: Vec<StateChange>,
        current_state: HashMap<String, serde_json::Value>,
    }

    impl Default for SessionStateAccumulator {
        fn default() -> Self {
            Self::new()
        }
    }

    impl SessionStateAccumulator {
        /// Create a new state accumulator starting at version 1.
        pub fn new() -> Self {
            Self {
                version_counter: 1,
                entries: Vec::new(),
                current_state: HashMap::new(),
            }
        }

        fn next_version(&mut self) -> u64 {
            let v = self.version_counter;
            self.version_counter += 1;
            v
        }

        fn now_ms() -> u64 {
            use std::time::SystemTime;
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        }

        /// Record a state creation.
        pub fn create(
            &mut self,
            key: impl Into<String>,
            value: impl Serialize,
            turn: u32,
        ) -> Result<u64, String> {
            let key = key.into();
            if self.current_state.contains_key(&key) {
                return Err(format!("Key '{}' already exists", key));
            }

            let version = self.next_version();
            let json_value = serde_json::to_value(value).map_err(|e| e.to_string())?;

            self.current_state.insert(key.clone(), json_value.clone());
            self.entries.push(StateChange {
                version,
                timestamp_ms: Self::now_ms(),
                key,
                op: StateChangeOp::Create,
                value: Some(json_value),
                turn,
            });

            Ok(version)
        }

        /// Record a state update.
        pub fn update(
            &mut self,
            key: impl Into<String>,
            value: impl Serialize,
            turn: u32,
        ) -> Result<u64, String> {
            let key = key.into();
            if !self.current_state.contains_key(&key) {
                return Err(format!("Key '{}' not found", key));
            }

            let version = self.next_version();
            let json_value = serde_json::to_value(value).map_err(|e| e.to_string())?;

            self.current_state.insert(key.clone(), json_value.clone());
            self.entries.push(StateChange {
                version,
                timestamp_ms: Self::now_ms(),
                key,
                op: StateChangeOp::Update,
                value: Some(json_value),
                turn,
            });

            Ok(version)
        }

        /// Record a state deletion.
        pub fn delete(&mut self, key: impl Into<String>, turn: u32) -> Result<u64, String> {
            let key = key.into();
            if !self.current_state.contains_key(&key) {
                return Err(format!("Key '{}' not found", key));
            }

            let version = self.next_version();
            self.current_state.remove(&key);
            self.entries.push(StateChange {
                version,
                timestamp_ms: Self::now_ms(),
                key,
                op: StateChangeOp::Delete,
                value: None,
                turn,
            });

            Ok(version)
        }

        /// Get the current value for a key.
        pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
            self.current_state.get(key)
        }

        /// Get all changes since a version (exclusive).
        pub fn changes_since(&self, since_version: u64) -> Vec<&StateChange> {
            self.entries
                .iter()
                .filter(|e| e.version > since_version)
                .collect()
        }
        /// Get current state snapshot.
        pub fn snapshot(&self) -> &HashMap<String, serde_json::Value> {
            &self.current_state
        }

        /// Get the latest version number.
        pub fn latest_version(&self) -> u64 {
            self.version_counter.saturating_sub(1)
        }

        /// Get the number of change entries.
        pub fn change_count(&self) -> usize {
            self.entries.len()
        }
        /// Compact by keeping only latest change per key.
        pub fn compact(&mut self) {
            let mut latest: HashMap<String, StateChange> = HashMap::new();

            for entry in &self.entries {
                if entry.op == StateChangeOp::Delete {
                    latest.remove(&entry.key);
                } else {
                    latest.insert(entry.key.clone(), entry.clone());
                }
            }

            let mut new_entries: Vec<StateChange> = latest.into_values().collect();
            new_entries.sort_by_key(|e| e.version);
            self.entries = new_entries;
        }

        /// Calculate memory overhead of changes vs full state.
        pub fn overhead_percentage(&self) -> f64 {
            let changes_bytes: usize = self
                .entries
                .iter()
                .map(|e| {
                    let base = e.key.len() + std::mem::size_of::<StateChange>();
                    let val_bytes = e
                        .value
                        .as_ref()
                        .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
                        .unwrap_or(0);
                    base + val_bytes
                })
                .sum();

            let state_bytes: usize = self
                .current_state
                .iter()
                .map(|(k, v)| k.len() + serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
                .sum();

            if state_bytes == 0 {
                0.0
            } else {
                (changes_bytes as f64 / state_bytes as f64) * 100.0
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_create_and_get() {
            let mut acc = SessionStateAccumulator::new();
            let v = acc.create("key1", "value1", 1).unwrap();

            assert_eq!(v, 1);
            assert_eq!(acc.get("key1"), Some(&serde_json::json!("value1")));
            assert_eq!(acc.change_count(), 1);
        }

        #[test]
        fn test_update_existing() {
            let mut acc = SessionStateAccumulator::new();
            acc.create("key1", "value1", 1).unwrap();
            let v = acc.update("key1", "value2", 2).unwrap();

            assert_eq!(v, 2);
            assert_eq!(acc.get("key1"), Some(&serde_json::json!("value2")));
            assert_eq!(acc.change_count(), 2);
        }

        #[test]
        fn test_delete_existing() {
            let mut acc = SessionStateAccumulator::new();
            acc.create("key1", "value1", 1).unwrap();
            let v = acc.delete("key1", 2).unwrap();

            assert_eq!(v, 2);
            assert_eq!(acc.get("key1"), None);
            assert_eq!(acc.change_count(), 2);
        }

        #[test]
        fn test_changes_since_version() {
            let mut acc = SessionStateAccumulator::new();
            acc.create("a", 1, 1).unwrap();
            acc.create("b", 2, 1).unwrap();
            acc.update("a", 3, 2).unwrap();

            let changes = acc.changes_since(1);
            assert_eq!(changes.len(), 2); // b, a-update
        }

        #[test]
        fn test_compact_reduces_entries() {
            let mut acc = SessionStateAccumulator::new();
            acc.create("key", "v1", 1).unwrap();
            acc.update("key", "v2", 1).unwrap();
            acc.update("key", "v3", 1).unwrap();

            assert_eq!(acc.change_count(), 3);
            acc.compact();
            assert_eq!(acc.change_count(), 1);
            assert_eq!(acc.get("key"), Some(&serde_json::json!("v3")));
        }

        #[test]
        fn test_overhead_with_updates() {
            let mut acc = SessionStateAccumulator::new();

            // Create many entries
            for i in 0..100 {
                acc.create(format!("key{}", i), "x".repeat(100), 1).unwrap();
            }

            // Update many times
            for _ in 0..5 {
                for i in 0..50 {
                    acc.update(format!("key{}", i), "y".repeat(100), 1).unwrap();
                }
            }

            let overhead = acc.overhead_percentage();
            // After many updates, overhead will be high until compaction
            // This verifies the measurement works
            assert!(overhead > 0.0, "Should have overhead after updates");

            // After compaction, overhead should be reduced
            acc.compact();
            let after = acc.overhead_percentage();
            assert!(after < overhead, "Compaction should reduce overhead");
        }
    }
}

/// Parent session linkage when forking or branching a session (edge-local audit + cloud sync).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLineage {
    pub parent_session_id: String,
    /// Last turn number included from the parent at fork time (for replay boundaries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_after_turn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Correlates this session or event with multi-agent / handoff workflows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinationMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    /// Shared id across forked sessions, sub-agents, or cloud-orchestrated steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Optional upstream event ids when this event is caused by multiple parents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_event_ids: Option<Vec<String>>,
}

/// Canonical prompt-history item emitted by a local root or delegated run.
///
/// The owning session journal keeps one item stream per execution run.
/// `run_id` and `agent_id` identify that execution, while `item_seq` is
/// monotonic within it. `source_event_id` is immutable across retries, replay
/// and pagination; consumers must never use message text as an identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalTranscriptItem {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_event_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub item_seq: u64,
    pub message: serde_json::Value,
}

/// Edge permission / cloud policy fingerprint at a point in time (for cloud–edge audit alignment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgePolicySnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_policy_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_fingerprint: Option<String>,
}

/// Per-tool-call audit record, embedded in turn events for granular tracking.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Provider/model tool call id. Stable linkage between assistant tool call,
    /// tool result, DB trace row, and any child-agent lifecycle event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool name.
    pub name: String,
    /// Whether the observed outcome is successful. For execution accounting,
    /// consult `disposition`: reused/deferred/suppressed calls did not execute.
    pub ok: bool,
    /// Execution time in milliseconds.
    pub ms: u64,
    /// Error message if the call failed (first 500 chars).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Input size in bytes (arguments/parameters).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_bytes: Option<u32>,
    /// Output size in bytes (result/response).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u32>,
    /// Preview of tool arguments (truncated to ~80 chars).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_preview: Option<String>,
    /// Preview of tool result (truncated to 500 chars) for cloud audit trail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_preview: Option<String>,
    /// File path extracted from full tool arguments at execution time.
    /// More reliable than parsing `args_preview` (which is truncated to ~80 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Explicit flag for surgically removed tool calls. When `true`, this record
    /// is an audit-only placeholder — the parallel tool call was removed from
    /// context because a skill covered the work. Prefer this over checking
    /// `name == SURGICAL_REMOVAL_TOOL_NAME`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surgically_removed: Option<bool>,
    /// Original tool name before surgical removal replaced it with the sentinel.
    /// Only set when `surgically_removed == Some(true)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_tool_name: Option<String>,
    // ── Observability fields (Phase 1) ───────────────────────────────────
    /// Offset from turn start when this tool began executing (milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_offset_ms: Option<u64>,
    /// Batch ID shared by tools executed in parallel within the same round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// Whether this tool was executed in parallel with others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<bool>,
    /// LLM round index within the turn (0-based). Identifies which LLM→tool
    /// cycle this call belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    /// Full tool arguments as JSON string (untruncated).
    /// This is the display-safe durable projection. Runtime authority that
    /// still needs the exact invocation uses [`Self::authoritative_args_full`]
    /// while the call is live; raw arguments must never cross a serde boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_full: Option<String>,
    /// Lossless arguments retained only in the live process for typed
    /// lifecycle/evidence decisions. This field is deliberately skipped by
    /// serde so checkpoints, events, journals, and cross-process payloads
    /// cannot persist model-supplied credentials.
    #[serde(skip)]
    pub runtime_args_full: Option<String>,
    /// Full tool result text (untruncated, after per-tool output limit).
    /// Enables debugging tool failures without re-execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_full: Option<String>,
    /// Dedicated ask_user prompt/response audit payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_user: Option<serde_json::Value>,
    /// When set, this record represents a short-circuited `skill(name=X)`
    /// re-invocation. The value is the re-entry index (1 = first repeat call,
    /// 2 = second, ...). Surfaces skill-loop inefficiencies in journal digests
    /// without needing to grep `result_preview`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_reentry_count: Option<u32>,
    /// When `true`, this short-circuited skill call was blocked by the per-turn
    /// re-entry lockout (reentry_count ≥ 3). The executor refused to even
    /// produce a follow-the-instructions stub and returned a BLOCKED result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_locked_out: Option<bool>,
    /// Exit-code semantic classification for this tool call.
    /// Serialized from `ExitSemantics` enum (snake_case). When present,
    /// downstream exit-code logic uses this to distinguish real errors
    /// from domain-negative outcomes (grep no-match, diff differences,
    /// test failures).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_semantics: Option<String>,
    /// Output-aware command result classification for trace/harness use.
    /// This catches failures that raw exit status can hide, such as a
    /// build/test pipeline whose final `tail` command exits successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_class: Option<String>,
    /// Executor-owned post-execution fact that the bound workspace changed.
    /// This is intentionally separate from lexical mutation classification:
    /// an opaque shell command may change files without containing a known
    /// redirect/verb, while a command that merely may mutate is not proof that
    /// it did. Only a trusted workspace executor may set this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mutation_observed: Option<bool>,
    /// Binding scope for `workspace_mutation_observed`; only
    /// `bound_workspace` may satisfy a workspace completion obligation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mutation_scope: Option<String>,
    /// Compact executor receipt retained for audit/debugging.  The boolean
    /// projection above is used by hot-path scheduling; this value preserves
    /// the receipt schema/source without carrying file contents across the
    /// journal boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mutation_receipt: Option<serde_json::Value>,
    /// Executor-owned observation that explicitly declared state outside the
    /// bound workspace changed during this invocation. This is separate from
    /// workspace evidence so neither scope can satisfy the other's contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_effect_observed: Option<bool>,
    /// Scope bound into `external_effect_receipt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_effect_scope: Option<String>,
    /// Compact, validated executor receipt for the declared external targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_effect_receipt: Option<serde_json::Value>,
    /// A multi-target writer committed a strict prefix before failing. This
    /// is a quarantine/barrier fact, never successful mutation evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mutation_partial: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mutation_partial_paths: Option<Vec<String>>,
    /// Validated executor-authored fact that a best-effort inferred source was
    /// modified or deleted.  This is advisory recovery state, not proof that
    /// the tool failed and not authority to roll the change back implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_preimage_recovery: Option<serde_json::Value>,
    /// Source-authored error classification. This remains typed across
    /// runtime, journal, ingestion, and reflection boundaries so downstream
    /// systems never need to infer control semantics from error prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<astra_core::ErrorKind>,
    /// What happened to the requested call at the execution boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<ToolCallDisposition>,
}

impl fmt::Debug for ToolCallRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Serialize the durable projection instead of deriving Debug over the
        // live-only runtime_args_full field. This keeps accidental tracing or
        // panic formatting from turning a model-supplied secret into a log
        // record while preserving a useful, complete audit shape.
        let durable = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        formatter
            .debug_struct("ToolCallRecord")
            .field("durable", &durable)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallDisposition {
    /// The executor ran and produced a success or failure outcome.
    Executed,
    /// Admission, permission, binding, or argument validation rejected the
    /// request before execution.
    Rejected,
    /// A previously computed observation was reused without execution.
    Reused,
    /// A duplicate/synthetic request was intentionally omitted.
    Suppressed,
    /// The request was intentionally postponed pending activation or a later
    /// retry opportunity.
    Deferred,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcomeSummary {
    pub requested: u32,
    pub executed: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub rejected: u32,
    pub reused: u32,
    pub suppressed: u32,
    pub deferred: u32,
}

impl ToolOutcomeSummary {
    pub fn from_records(records: &[ToolCallRecord]) -> Self {
        let mut summary = Self::default();
        for record in records {
            summary.requested = summary.requested.saturating_add(1);
            match record.effective_disposition() {
                ToolCallDisposition::Executed => {
                    summary.executed = summary.executed.saturating_add(1);
                    if record.ok {
                        summary.succeeded = summary.succeeded.saturating_add(1);
                    } else {
                        summary.failed = summary.failed.saturating_add(1);
                    }
                }
                ToolCallDisposition::Rejected => {
                    summary.rejected = summary.rejected.saturating_add(1)
                }
                ToolCallDisposition::Reused => summary.reused = summary.reused.saturating_add(1),
                ToolCallDisposition::Suppressed => {
                    summary.suppressed = summary.suppressed.saturating_add(1)
                }
                ToolCallDisposition::Deferred => {
                    summary.deferred = summary.deferred.saturating_add(1)
                }
            }
        }
        summary
    }

    pub fn is_consistent(&self) -> bool {
        self.executed == self.succeeded.saturating_add(self.failed)
            && self.requested
                == self
                    .executed
                    .saturating_add(self.rejected)
                    .saturating_add(self.reused)
                    .saturating_add(self.suppressed)
                    .saturating_add(self.deferred)
    }
}

/// Tool call name sentinel used for assistant messages that had parallel tool
/// calls surgically removed from context (see
/// `runtime::turn::agentic_tool_interception`). These are intentional context
/// optimizations — **not** tool failures — and are filtered out of
/// evaluation/analytics by [`ToolCallRecord::is_synthetic_placeholder`].
pub const SURGICAL_REMOVAL_TOOL_NAME: &str = "(surgically_removed)";
pub const NOOP_OR_CACHED_RESULT_CLASS: &str = "noop_or_cached";
pub const BLOCKED_TOOL_RESULT_CLASS: &str = "blocked_tool";

impl ToolCallRecord {
    /// Return the lossless arguments for live authority decisions. Restored
    /// records from before the runtime-only lane remain usable when their
    /// durable projection is exact; a redacted projection is intentionally
    /// not parsed as an authority input because its marker is not the command
    /// the executor ran.
    pub fn authoritative_args_full(&self) -> Option<&str> {
        if let Some(runtime_args) = self.runtime_args_full.as_deref() {
            return Some(runtime_args);
        }
        let durable_args = self.args_full.as_deref()?;
        (!durable_args.contains("[REDACTED:")).then_some(durable_args)
    }

    pub fn was_executed(&self) -> bool {
        self.effective_disposition() == ToolCallDisposition::Executed
    }

    /// Canonical terminal lifecycle event for this record.
    ///
    /// Keeping this mapping on the record prevents journal ingestion, live
    /// trace persistence, transcripts, and audit analytics from inventing
    /// incompatible interpretations of the same disposition.
    pub fn canonical_terminal_event_type(&self) -> &'static str {
        match self.effective_disposition() {
            ToolCallDisposition::Executed if self.ok => "tool_call_completed",
            ToolCallDisposition::Executed => "tool_call_failed",
            ToolCallDisposition::Rejected => "tool_call_rejected",
            ToolCallDisposition::Reused => "tool_call_reused",
            ToolCallDisposition::Suppressed => "tool_call_suppressed",
            ToolCallDisposition::Deferred => "tool_call_deferred",
        }
    }

    pub fn effective_disposition(&self) -> ToolCallDisposition {
        if let Some(disposition) = self.disposition {
            return disposition;
        }
        if self.was_blocked_by_policy() || self.skill_locked_out == Some(true) {
            return ToolCallDisposition::Rejected;
        }
        if self.surgically_removed == Some(true) || self.skill_reentry_count.is_some() {
            return ToolCallDisposition::Suppressed;
        }
        if self.result_class.as_deref() == Some(NOOP_OR_CACHED_RESULT_CLASS) {
            return ToolCallDisposition::Reused;
        }
        ToolCallDisposition::Executed
    }

    /// Synthetic placeholders are audit-only records emitted when skill routing
    /// suppresses a tool call without actually executing it, **or** when a
    /// parallel tool call was surgically removed from context after a skill
    /// took over its work. Neither case represents real tool execution or
    /// failure, so these records must be filtered out before computing
    /// analytics (tool_error_rate, repeat_tool_call, failed_tool_calls, …).
    pub fn is_synthetic_placeholder(&self) -> bool {
        self.is_noop_or_cached_result()
    }

    /// True when this tool call was rejected by the pipeline before execution
    /// (e.g. restricted_tools policy).  These calls should not count toward
    /// `tools_used` for pattern learning — they never ran, so attributing
    /// turn-level success/failure to them creates a self-reinforcing block loop.
    pub fn was_blocked_by_policy(&self) -> bool {
        !self.ok && self.result_class.as_deref() == Some(BLOCKED_TOOL_RESULT_CLASS)
    }

    /// True when the tool call did not produce new observations because the
    /// runtime served or pointed back to an already-known result.
    ///
    /// This is intentionally a result-semantics predicate, not a read_file
    /// special case. Evaluation and loop guards can use it to distinguish
    /// "successful execution with fresh evidence" from cache hits, duplicate
    /// suppressions, and unchanged-result stubs.
    pub fn is_noop_or_cached_result(&self) -> bool {
        self.result_class.as_deref() == Some(NOOP_OR_CACHED_RESULT_CLASS)
            || self.surgically_removed == Some(true)
            || self.skill_reentry_count.is_some()
            || self.skill_locked_out == Some(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalJournalDecision {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_kind: Option<String>,
}

impl ApprovalJournalDecision {
    pub fn interaction_contract(
        &self,
        session_id: &str,
        user_id: Option<&str>,
    ) -> Option<InteractionContract> {
        let run_id = self.run_id.as_deref()?;
        let identity =
            InteractionIdentity::new(user_id, session_id, run_id, self.request_id.as_str());
        if !identity.is_run_scoped() {
            return None;
        }
        Some(InteractionContract::new(
            InteractionKind::Approval,
            identity,
            approval_decision_status(&self.decision),
            Some("session_journal.approval_decision".to_string()),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalJournalRequest {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_kind: Option<String>,
}

fn normalize_optional_str(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn normalize_tool_call_records(records: Vec<ToolCallRecord>) -> Vec<ToolCallRecord> {
    records
        .into_iter()
        .filter_map(|mut record| {
            let name = record.name.trim();
            if name.is_empty() {
                return None;
            }
            record.name = name.to_string();
            record.original_tool_name = normalize_optional_name(record.original_tool_name);
            Some(record)
        })
        .collect()
}

#[inline]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Identity owned by the runtime that produced an event.
///
/// `JournalEvent::turn` is reserved for the root session turn. Child-local
/// counters live here so consumers cannot join independent turn namespaces by
/// comparing bare integers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalProducerScope {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_turn: Option<u32>,
}

/// A single journal event (one line in the JSONL file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEvent {
    /// Event type discriminator.
    #[serde(rename = "type")]
    pub event_type: JournalEventType,
    /// ISO 8601 timestamp.
    pub ts: String,
    /// Session ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Run-scoped producer identity, independent from the root session turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_scope: Option<JournalProducerScope>,
    /// Turn number (1-based, for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// Internal agentic step within the outer session turn (0-based).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentic_step: Option<u32>,
    /// LLM model used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// User input text (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_input: Option<String>,
    /// Assistant response text (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_output: Option<String>,
    /// Number of calls that actually reached an executor in this turn.
    /// `tool_outcomes.requested` includes rejected/reused/suppressed/deferred
    /// calls and is therefore the correct denominator for admission analysis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<u32>,
    /// Fresh input tokens used. Prompt-cache read/write buckets are carried in
    /// `cache_read_tokens` / `cache_creation_tokens`; add all three for the
    /// billable input total.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    /// Completion tokens used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    /// Turn duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Error message (for error events or failed turns).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Config key (for config_change events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_key: Option<String>,
    /// Config value (for config_change events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_value: Option<String>,
    /// Number of turns compacted (for compact events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns_compacted: Option<usize>,
    /// Number of facts stored (for compact events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts_stored: Option<usize>,
    /// Tool names selected for the LLM request (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible_tools: Option<Vec<String>>,
    /// Skill names selected for the LLM request (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_skills: Option<Vec<String>>,
    /// Tool names actually called by the LLM (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_used: Option<Vec<String>>,
    /// Per-tool-call detail: [{name, ok, ms, error?}] for granular audit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    /// Mutually exclusive execution-boundary counts derived from `tool_calls`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_outcomes: Option<ToolOutcomeSummary>,
    /// Token budget used by selected dynamic tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_used: Option<u32>,
    /// Token budget pressure (0.0 = normal, 0.3 = trim, 0.6 = compact, 0.9 = aggressive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_pressure: Option<f64>,
    /// Stall type (for stall_detected events): "sig_stall", "name_stall", "divergence".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_type: Option<String>,
    /// Flexible metadata for event-specific data (JSON object).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Plan subtask ID — set when this turn was executed as part of plan mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_subtask_id: Option<String>,
    /// Time to first token in milliseconds (streaming latency).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Context assembly time in milliseconds (prompt building).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_ms: Option<u64>,
    /// Cache read tokens (prompt cache hits).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Cache creation tokens (prompt cache writes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// Memoria search time in milliseconds (subset of context_ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memoria_ms: Option<u64>,
    /// Fork / branch lineage (also set on `session_fork` events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_lineage: Option<SessionLineage>,
    /// Multi-agent or handoff correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination: Option<CoordinationMeta>,
    /// Canonical conversation item for a local root or delegated run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_item: Option<JournalTranscriptItem>,
    /// Canonical conversation delta committed atomically with a completed
    /// root turn. This is the durability source for prompt continuation;
    /// CSL and display history are projections of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_commit: Option<ConversationCommitV1>,
    /// Edge policy snapshot for cloud–edge audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_policy: Option<EdgePolicySnapshot>,
    /// Full context assembly trace for deep observability (M1 telemetry).
    /// Stores the serialized ContextAssemblyTrace from runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_assembly_trace: Option<serde_json::Value>,
    /// Routing domain hint label for this REPL turn (e.g. `github`); omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_domain_hint: Option<String>,
    /// True when the turn succeeded with tool calls but routing had no domain — entity graph learn was skipped.
    #[serde(default, skip_serializing_if = "is_false")]
    pub entity_learn_skipped_no_domain: bool,
    // ── Observability fields (Phase 1) ───────────────────────────────────
    /// LLM round index within a turn (0-based, for llm_round events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    /// Number of tool_calls returned by LLM in this round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls_returned: Option<u32>,
    /// Offset from turn start in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_ms: Option<u64>,
    /// Total LLM rounds in this turn (set on turn_completed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_rounds: Option<u32>,
    /// Total LLM time in this turn excluding tool execution (set on turn_completed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_llm_ms: Option<u64>,
    /// Total tool execution time in this turn (set on turn_completed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tool_ms: Option<u64>,
    // ── Causal lineage (P5) ──────────────────────────────────────────────
    /// Parent event ID for causal tree construction.
    /// Turn → SessionStart, LlmRound → Turn, DelegationSubRunStarted → DelegationStarted, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_event_id: Option<String>,
    // ── Git snapshot (P0) ────────────────────────────────────────────────
    /// Git HEAD commit hash at the time of this event (short or full SHA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    /// Git branch name at the time of this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
}

/// Event type discriminator.
///
/// Note: deliberately does NOT derive `Ord`/`PartialOrd`.  Same-timestamp
/// boundary semantics live in [`event_type_tiebreak_rank`] so adding,
/// removing, or reordering variants cannot silently change journal
/// envelope ordering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JournalEventType {
    /// Session started.
    SessionStart,
    /// A conversation turn completed.
    Turn,
    /// A turn failed with an error.
    TurnError,
    /// A tool call failed with an error (non-zero exit, crash, signal, timeout).
    ToolCallError,
    /// Manual or auto compact.
    Compact,
    /// Configuration changed (model, explain, skill toggle).
    ConfigChange,
    /// An error occurred (non-turn).
    Error,
    /// Session ended.
    SessionEnd,
    /// Stall or divergence detected (non-happy path).
    StallDetected,
    /// Session checkpoint saved.
    Checkpoint,
    /// TurnGuard verdict emitted (unified non-happy-path audit).
    TurnGuardVerdict,
    /// Turn quality evaluation recorded for audit and replay surfaces.
    TurnEvaluation,
    /// Plan execution progress (subtask started, completed, plan done).
    PlanProgress,
    /// Forked from another session — records lineage for audit and sync.
    SessionFork,
    /// Cloud–edge policy ack, agent handoff, or other sync metadata (lightweight).
    SyncMarker,
    /// Delegation group started (sub-run group spawned).
    DelegationStarted,
    /// A single sub-run within a delegation started running.
    DelegationSubRunStarted,
    /// A single sub-run within a delegation completed.
    DelegationSubRunCompleted,
    /// A sub-run was retried, linking the original run to the new retry run.
    DelegationRetry,
    /// Delegation completed (all sub-runs done, results aggregated).
    DelegationCompleted,
    /// A child agent was spawned (via spawn_agent tool or delegation).
    AgentSpawned,
    /// A spawned agent terminated (completed, failed, or cancelled).
    AgentTerminated,
    /// One canonical prompt-history item from a local root or delegated run.
    TranscriptItem,
    /// Subtask or plan verification completed (acceptance-criteria gate result).
    VerificationCompleted,
    /// Plan was edited (subtask added/removed/reordered, goal changed).
    PlanEdit,
    /// Plan lifecycle event (created, completed, abandoned, replanned).
    PlanLifecycle,
    /// Durable task lifecycle event (created, updated, completed, failed, cancelled).
    TaskLifecycle,
    /// Effective goal steering changed (manual goal set, active plan goal took over).
    GoalSteered,
    /// An approval prompt was emitted for a tool call.
    ApprovalRequired,
    /// An approval decision was received for a tool call.
    ApprovalDecision,
    /// An approval prompt timed out before a decision arrived.
    ApprovalTimeout,
    /// An ask_user prompt was emitted and is waiting for a response.
    AskUserPrompted,
    /// An ask_user response was received.
    AskUserResponse,
    /// Permission evaluation / approval / persistence audit event.
    PermissionAudit,
    /// A rollback-capable execution boundary started tracking side effects.
    ExecutionBoundaryOpened,
    /// A rollback-capable execution boundary finished successfully.
    ExecutionBoundaryCommitted,
    /// A rollback-capable execution boundary aborted and may have rolled back prior work.
    ExecutionBoundaryAborted,
    /// Context assembly trace recorded (observability: prompt building details).
    ContextAssemblyRecorded,
    /// Focus drift detected during a turn (severity, cause, evidence).
    DriftDetected,
    /// Scenario detected and adaptive profile applied for this session.
    AdaptiveScenarioApplied,
    /// Per-turn micro-adaptation adjusted config values.
    AdaptivePerTurnApplied,
    /// A structured interruption was recorded (budget exhaustion, rate limit, cancel, etc.).
    InterruptionRecorded,
    /// Compaction retry completed — records tier, tokens freed, and per-layer breakdown.
    CompactionRetry,
    /// One LLM→tools round within a turn (observability Phase 1).
    LlmRound,
    /// Full LLM request payload for a single attempt within a round.
    LlmRequestFull,
    /// Full LLM response payload for a single attempt within a round.
    LlmResponseFull,
    /// Background session-memory (session-memory.md) extraction completed.
    /// Describes a single atomic rewrite of the session-memory L1
    /// artifact.
    SessionMemoryExtraction,
    /// A durable, structured diagnostic from asynchronous subsystem work.
    ///
    /// This is intentionally separate from free-form logs: product verification
    /// and operators can act on severity/subsystem/operation/code without
    /// matching human-facing error text.
    SubsystemDiagnostic,
    /// An asynchronous subsystem reached its durable observation boundary.
    SubsystemSettled,
    /// Context pipeline per-turn feedback (cache ratio, tokens, tier).
    PipelineFeedback,
    /// Context pipeline trace alert fired (cache break, recovery loop, etc.).
    PipelineAlert,
    /// Context pipeline compaction audit (what was dropped/cleared, why).
    PipelineCompactionAudit,
    /// Startup bootstrap phases completed (per-phase timestamps in metadata).
    Bootstrap,
    /// Lightweight trace span for cross-boundary observability (edge ↔ cloud).
    ///
    /// Stored in `metadata` as `{span_id, parent_span_id, name, start_us, end_us, attrs}`.
    /// Turn-level spans use `turn` + `parent_event_id` for causal tree construction
    /// without requiring a dedicated graph database.
    TraceSpan,
}

/// Why the gate rejected a session-memory extraction attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMemoryExtractionSkipReason {
    NoSessionId,
    NoGrowth,
    /// The selector returned a structurally valid empty sparse patch. This is
    /// a successful semantic decision: retrying other models cannot create
    /// evidence that the conversation did not contain.
    NoSemanticChange,
    /// A durable snapshot already covers this turn. This closes the
    /// cross-process/restart gap that an in-memory debounce cannot observe.
    AlreadyCurrent,
    InFlight,
    /// A newer canonical turn already owns extraction for this session. The
    /// older snapshot is intentionally discarded instead of being allowed to
    /// overwrite or delay newer memory.
    Superseded,
    SelectorCooldown,
    /// Memoria endpoint tripped the circuit breaker after consecutive
    /// failures. Emitted synchronously — no spawn, no retry attempted
    /// until the cooldown TTL elapses.
    MemoriaUnhealthy,
}

/// Why an attempt errored during the LLM/write phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMemoryExtractionErrorReason {
    /// The caller supplied a non-session inference owner to session-memory
    /// extraction. No provider or Memoria request was attempted.
    InvalidScope,
    /// The complete background extraction operation exceeded its bounded
    /// deadline before it reached a durable terminal outcome. This is
    /// deliberately distinct from [`Self::LlmTimeout`]: the deadline covers
    /// provider selection, snapshot I/O, persistence, and cleanup together.
    DeadlineExceeded,
    LlmTimeout,
    LlmError,
    EmptyResponse,
    /// Pre-store purge of previous L1 failed after retries — aborting
    /// the store avoids leaving two L1 rows in Memoria for one session,
    /// which would make prefix-based retrieval non-deterministic.
    PurgeFailed,
    WriteFailed,
}

/// Which code path produced the written content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMemoryExtractionSource {
    Llm,
    RuleFallback,
}

/// Operational impact of a structured subsystem diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubsystemDiagnosticSeverity {
    Warning,
    Error,
}

/// Full outcome of a single extraction attempt. Serialized flat into the
/// event metadata: `{"outcome": "extracted", "source": "llm",
/// "bytes_written": 4021}` or `{"outcome": "skipped", "reason": "in_flight"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMemoryExtractionOutcome {
    Extracted {
        source: SessionMemoryExtractionSource,
        bytes_written: u64,
    },
    Skipped {
        reason: SessionMemoryExtractionSkipReason,
    },
    Errored {
        reason: SessionMemoryExtractionErrorReason,
    },
}

/// Operational breadcrumbs merged into the session-memory extraction
/// event's metadata. None-valued fields are omitted from the JSON so
/// skip events (which carry no LLM state) don't emit nonsense keys.
///
/// These fields exist for operator debugging (`SELECT ... FROM
/// agent_events WHERE event_type='session_memory_extraction'` on a
/// puzzling session) and must not drive any runtime behaviour — the
/// logic-bearing fields are `outcome` / `reason` / `source`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMemoryExtractionBreadcrumbs {
    /// Number of messages fed into the extractor. Helps triage
    /// "why does this session have no L1" (possibly 0 messages).
    pub messages_count: Option<u32>,
    /// Selector model that was actually used (or would have been, if
    /// the call failed before dispatch). Absent on pure-gate skips and
    /// on pure rule-based runs with no selector. Present on degraded
    /// fallback writes when a selector existed but was unhealthy.
    pub selector_model: Option<String>,
    /// Final attempt count (1 = succeeded first try, 2 = recovered
    /// after one retry). Absent when no persist attempt occurred.
    pub attempt: Option<u32>,
    /// When the LLM fails before the final persist error, keep that
    /// upstream reason so the journal reflects the full failure chain.
    pub llm_reason: Option<SessionMemoryExtractionErrorReason>,
    /// Short human-readable detail for the upstream LLM failure (for
    /// example an HTTP status or provider error message snippet).
    pub llm_detail: Option<String>,
    /// Short human-readable detail for the final persist failure (for
    /// example a backend validation or proxy error snippet).
    pub persist_detail: Option<String>,
}

impl SessionMemoryExtractionOutcome {
    /// Short tag matching the top-level `outcome` field. Cheap enough to
    /// call from log lines and UX bridges that don't want to match the
    /// full enum.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Extracted { .. } => "extracted",
            Self::Skipped { .. } => "skipped",
            Self::Errored { .. } => "errored",
        }
    }

    fn to_json(&self, bc: &SessionMemoryExtractionBreadcrumbs) -> serde_json::Value {
        let mut obj = match self {
            Self::Extracted {
                source,
                bytes_written,
            } => serde_json::json!({
                "outcome": "extracted",
                "source": source,
                "bytes_written": bytes_written,
            }),
            Self::Skipped { reason } => serde_json::json!({
                "outcome": "skipped",
                "reason": reason,
            }),
            Self::Errored { reason } => serde_json::json!({
                "outcome": "errored",
                "reason": reason,
            }),
        };
        if let Some(map) = obj.as_object_mut() {
            if let Some(n) = bc.messages_count {
                map.insert("messages_count".into(), serde_json::json!(n));
            }
            if let Some(ref m) = bc.selector_model {
                map.insert("selector_model".into(), serde_json::json!(m));
            }
            if let Some(a) = bc.attempt {
                map.insert("attempt".into(), serde_json::json!(a));
            }
            if let Some(llm_reason) = bc.llm_reason {
                map.insert("llm_reason".into(), serde_json::json!(llm_reason));
            }
            if let Some(ref llm_detail) = bc.llm_detail {
                map.insert("llm_detail".into(), serde_json::json!(llm_detail));
            }
            if let Some(ref persist_detail) = bc.persist_detail {
                map.insert("persist_detail".into(), serde_json::json!(persist_detail));
            }
        }
        obj
    }
}

/// Writer that appends events to a session journal file.
#[derive(Debug, thiserror::Error)]
pub enum SessionExecutionLeaseError {
    #[error("session `{session_id}` already has an active execution")]
    Conflict { session_id: String },
    #[error("failed to acquire execution lease for session `{session_id}`: {source}")]
    Io {
        session_id: String,
        #[source]
        source: std::io::Error,
    },
}

/// Process-independent admission token for one session execution.
///
/// On Linux, a kernel-named abstract Unix socket is the primary admission
/// authority. It cannot be renamed or unlinked, so replacing the advisory
/// lock-file inode cannot create two simultaneous owners. The lock file is a
/// second fence for cooperating processes in other network namespaces and on
/// shared filesystems. It is deliberately independent from the rotatable
/// journal inode and is never removed. Dropping the token releases both locks.
#[derive(Debug)]
pub struct SessionExecutionLease {
    #[cfg(target_os = "linux")]
    _kernel_authority: std::os::unix::net::UnixDatagram,
    _file: std::fs::File,
    session_id: String,
    lock_path: PathBuf,
    created_parent_dirs: Vec<PathBuf>,
}

impl SessionExecutionLease {
    pub fn try_acquire(session_id: &str) -> Result<Self, SessionExecutionLeaseError> {
        let owner_scope = OwnerScope::local_user();
        let journal_path =
            journal_file_path_for_owner(&owner_scope, session_id).map_err(|source| {
                SessionExecutionLeaseError::Io {
                    session_id: session_id.to_string(),
                    source,
                }
            })?;
        let lock_path = execution_lease_path(&journal_path, session_id);
        // On Linux this binds a kernel-held guard that must live for the whole
        // lease; the non-Linux fallback returns unit, which trips the lint.
        #[allow(clippy::let_unit_value)]
        let _kernel_authority =
            acquire_execution_kernel_authority(&owner_scope, &journal_path, session_id)?;
        let created_parent_dirs = journal_path
            .parent()
            .map(missing_directories_before_create)
            .unwrap_or_default();
        if let Some(parent) = lock_path.parent()
            && let Err(source) = std::fs::create_dir_all(parent)
        {
            return Err(SessionExecutionLeaseError::Io {
                session_id: session_id.to_string(),
                source,
            });
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|source| SessionExecutionLeaseError::Io {
                session_id: session_id.to_string(),
                source,
            })?;
        use fs2::FileExt;
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(SessionExecutionLeaseError::Conflict {
                    session_id: session_id.to_string(),
                });
            }
            Err(source) => {
                return Err(SessionExecutionLeaseError::Io {
                    session_id: session_id.to_string(),
                    source,
                });
            }
        }
        match open_journal_file_is_current(&file, &lock_path) {
            Ok(true) => Ok(Self {
                #[cfg(target_os = "linux")]
                _kernel_authority,
                _file: file,
                session_id: session_id.to_string(),
                lock_path,
                created_parent_dirs,
            }),
            Ok(false) => Err(SessionExecutionLeaseError::Io {
                session_id: session_id.to_string(),
                source: std::io::Error::other(
                    "execution lease inode was replaced during acquisition",
                ),
            }),
            Err(source) => Err(SessionExecutionLeaseError::Io {
                session_id: session_id.to_string(),
                source,
            }),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

#[cfg(target_os = "linux")]
fn acquire_execution_kernel_authority(
    owner_scope: &OwnerScope,
    journal_path: &Path,
    session_id: &str,
) -> Result<std::os::unix::net::UnixDatagram, SessionExecutionLeaseError> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let absolute_journal_path = if journal_path.is_absolute() {
        journal_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(journal_path))
            .map_err(|source| SessionExecutionLeaseError::Io {
                session_id: session_id.to_string(),
                source,
            })?
    };
    let mut identity = Sha256::new();
    identity.update(b"astra-session-execution-authority-v1\0");
    identity.update(owner_scope.id().as_bytes());
    identity.update(b"\0");
    identity.update(absolute_journal_path.as_os_str().as_encoded_bytes());
    identity.update(b"\0");
    identity.update(session_id.as_bytes());
    let name = format!("astra-exec-v1-{:x}", identity.finalize());
    let address = SocketAddr::from_abstract_name(name.as_bytes()).map_err(|source| {
        SessionExecutionLeaseError::Io {
            session_id: session_id.to_string(),
            source,
        }
    })?;
    UnixDatagram::bind_addr(&address).map_err(|source| {
        if source.kind() == std::io::ErrorKind::AddrInUse {
            SessionExecutionLeaseError::Conflict {
                session_id: session_id.to_string(),
            }
        } else {
            SessionExecutionLeaseError::Io {
                session_id: session_id.to_string(),
                source,
            }
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn acquire_execution_kernel_authority(
    _owner_scope: &OwnerScope,
    _journal_path: &Path,
    session_id: &str,
) -> Result<(), SessionExecutionLeaseError> {
    Err(SessionExecutionLeaseError::Io {
        session_id: session_id.to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this platform has no rename-resistant session execution authority",
        ),
    })
}

fn execution_lease_path(journal_path: &Path, session_id: &str) -> PathBuf {
    journal_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".execution-locks")
        .join(format!("{session_id}.lock"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalCommitCasOutcome {
    Committed { persistence_warning: Option<String> },
    NotCommitted(String),
    Conflict(String),
    Unknown(String),
}

#[derive(Debug, PartialEq, Eq)]
enum CanonicalCommitPresence {
    Exact,
    Absent,
    Conflict(String),
    Unknown(String),
}

fn classify_canonical_commit_presence(
    events: &[JournalEvent],
    malformed_lines: usize,
    turn: u32,
    intended: &ConversationCommitV1,
) -> CanonicalCommitPresence {
    if malformed_lines > 0 {
        return CanonicalCommitPresence::Unknown(format!(
            "journal contains {malformed_lines} malformed record(s)"
        ));
    }

    let intended_cursor = &intended.cursor;
    let mut exact_matches = 0usize;
    let mut same_turn_conflict = false;
    let mut same_cursor_conflict = false;
    for event in events {
        if let Some(commit) = event.conversation_commit.as_ref() {
            if commit == intended {
                exact_matches += 1;
                continue;
            }
            let cursor = &commit.cursor;
            if cursor.owner_id == intended_cursor.owner_id
                && cursor.session_id == intended_cursor.session_id
                && cursor.branch_id == intended_cursor.branch_id
                && cursor.completed_turn == intended_cursor.completed_turn
                && cursor.journal_event_seq == intended_cursor.journal_event_seq
                && cursor.conversation_seq == intended_cursor.conversation_seq
            {
                same_cursor_conflict = true;
            }
        }
        if matches!(
            event.event_type,
            JournalEventType::Turn | JournalEventType::TurnError
        ) && event.turn == Some(turn)
        {
            same_turn_conflict = true;
        }
    }

    if exact_matches == 1 && !same_turn_conflict && !same_cursor_conflict {
        CanonicalCommitPresence::Exact
    } else if exact_matches > 1
        || (exact_matches == 1 && (same_turn_conflict || same_cursor_conflict))
    {
        CanonicalCommitPresence::Unknown(
            "journal contains duplicate or conflicting copies of the intended canonical commit"
                .to_string(),
        )
    } else if same_cursor_conflict {
        CanonicalCommitPresence::Conflict(
            "journal contains a different commit at the intended canonical cursor".to_string(),
        )
    } else if same_turn_conflict {
        CanonicalCommitPresence::Conflict(format!(
            "journal turn identity {turn} is already committed"
        ))
    } else {
        CanonicalCommitPresence::Absent
    }
}

pub struct JournalWriter {
    path: PathBuf,
    owner_scope: OwnerScope,
    session_id: String,
    created_parent_dirs: Vec<PathBuf>,
}

impl JournalWriter {
    /// Create a writer in the process-local owner scope.
    ///
    /// Authenticated runtimes must use [`Self::for_user`]; this constructor is
    /// reserved for genuinely local CLI sessions and tests.
    pub fn new(session_id: &str) -> std::io::Result<Self> {
        let owner_scope = OwnerScope::local_user();
        let path = journal_file_path_for_owner(&owner_scope, session_id)?;
        Self::from_path(owner_scope, session_id, path)
    }

    /// Create a writer isolated to an authenticated user owner scope.
    pub fn for_user(user_id: &str, session_id: &str) -> std::io::Result<Self> {
        let owner_scope = OwnerScope::user(user_id)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let path = journal_file_path_for_owner(&owner_scope, session_id)?;
        Self::from_path(owner_scope, session_id, path)
    }

    /// Create a writer isolated to an explicit owner scope.
    pub fn for_owner(owner_scope: &OwnerScope, session_id: &str) -> std::io::Result<Self> {
        let path = journal_file_path_for_owner(owner_scope, session_id)?;
        Self::from_path(owner_scope.clone(), session_id, path)
    }

    fn from_path(
        owner_scope: OwnerScope,
        session_id: &str,
        path: PathBuf,
    ) -> std::io::Result<Self> {
        validate_session_id(session_id)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        if path.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("journal path is a directory: {}", path.display()),
            ));
        }
        let mut created_parent_dirs = Vec::new();
        if let Some(dir) = path.parent() {
            created_parent_dirs = missing_directories_before_create(dir);
            std::fs::create_dir_all(dir)?;
        }
        Ok(Self {
            path,
            owner_scope,
            session_id: session_id.to_string(),
            created_parent_dirs,
        })
    }

    /// Append a single event to the journal file.
    ///
    /// **Concurrency:** the line + trailing `\n` are written via a single
    /// `write_all` call so concurrent appenders cannot interleave the newline
    /// with another writer's payload. On Linux, writes to a regular file
    /// opened with `O_APPEND` of size <= `PIPE_BUF` (4096 bytes) are atomic;
    /// `writeln!` would issue the `\n` as a separate syscall and lose
    /// atomicity, producing concatenated records like `{a}{b}\n\n` that the
    /// reader cannot parse. See `JournalWriter::append` test
    /// `concurrent_appends_remain_record_separated`.
    pub fn append(&self, event: &JournalEvent) -> std::io::Result<()> {
        use std::io::Write;
        let mut file = open_locked_journal_file(&self.path)?;
        let events = prepend_session_start_if_needed(&self.path, std::slice::from_ref(event))?;
        let buf = serialize_journal_events(events.as_ref())?;
        if let Err(e) = file.write_all(&buf) {
            if e.kind() == std::io::ErrorKind::Other
                || e.raw_os_error() == Some(28) // ENOSPC
                || e.to_string().contains("No space")
            {
                astra_core::agent_error!("journal", "disk full, journal event lost");
            }
            return Err(e);
        }
        // Ensure durability: flush to disk so a crash doesn't lose the event.
        file.sync_data()?;
        update_cached_session_start_state_from_events(&self.path, events.as_ref());
        Ok(())
    }

    /// Get the path to this journal file.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Immutable owner captured when this writer was opened.
    ///
    /// Keeping ownership on the handle prevents a later process-local profile
    /// switch from projecting this journal into another account's outbox.
    pub fn owner_scope(&self) -> &OwnerScope {
        &self.owner_scope
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Batch-append multiple events in a single write + fsync.
    pub fn append_bulk(&self, events: &[JournalEvent]) -> std::io::Result<()> {
        self.append_bulk_inner(events, true)
    }

    /// Batch-append multiple events without fsync (best-effort, for interrupted turns).
    pub fn append_bulk_no_sync(&self, events: &[JournalEvent]) -> std::io::Result<()> {
        self.append_bulk_inner(events, false)
    }

    fn sync_creation_metadata_if_needed(
        &self,
        journal_created: bool,
        first_canonical_commit: bool,
        lease_created_dirs: &[PathBuf],
    ) -> std::io::Result<()> {
        if !journal_created
            && !first_canonical_commit
            && self.created_parent_dirs.is_empty()
            && lease_created_dirs.is_empty()
        {
            return Ok(());
        }
        let mut directories = Vec::new();
        if let Some(parent) = self.path.parent() {
            directories.push(parent.to_path_buf());
        }
        for created in self.created_parent_dirs.iter().chain(lease_created_dirs) {
            if !directories.iter().any(|existing| existing == created) {
                directories.push(created.clone());
            }
            if let Some(parent) = created.parent()
                && !directories.iter().any(|existing| existing == parent)
            {
                directories.push(parent.to_path_buf());
            }
        }
        directories.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
        for directory in directories {
            sync_directory(&directory)?;
        }
        Ok(())
    }

    /// Atomically compare and append one canonical conversation commit.
    ///
    /// The compare, append, durability fence, generation confirmation, and
    /// exact-commit verification all happen while holding one exclusive file
    /// lock. A stale caller can therefore never append a second version of the
    /// same turn or report success for a detached journal inode.
    pub fn append_canonical_commit_cas(
        &self,
        lease: &SessionExecutionLease,
        expected_base_cursor: Option<&SessionCursorV1>,
        turn: u32,
        intended: &ConversationCommitV1,
        events: &[JournalEvent],
    ) -> CanonicalCommitCasOutcome {
        if lease.session_id != self.session_id
            || lease.lock_path != execution_lease_path(&self.path, &self.session_id)
        {
            return CanonicalCommitCasOutcome::NotCommitted(
                "canonical CAS requires the matching active session execution lease".to_string(),
            );
        }
        self.append_canonical_commit_cas_locked(
            expected_base_cursor,
            turn,
            intended,
            events,
            &lease.created_parent_dirs,
        )
    }

    fn append_canonical_commit_cas_locked(
        &self,
        expected_base_cursor: Option<&SessionCursorV1>,
        turn: u32,
        intended: &ConversationCommitV1,
        events: &[JournalEvent],
        lease_created_dirs: &[PathBuf],
    ) -> CanonicalCommitCasOutcome {
        use fs2::FileExt;
        use std::io::{Read, Seek, SeekFrom, Write};

        if intended.cursor.session_id != self.session_id || intended.cursor.completed_turn != turn {
            return CanonicalCommitCasOutcome::NotCommitted(
                "intended canonical commit does not match the writer session/turn".to_string(),
            );
        }
        let intended_turns = events
            .iter()
            .filter(|event| {
                event.event_type == JournalEventType::Turn
                    && event.turn == Some(turn)
                    && event.conversation_commit.as_ref() == Some(intended)
            })
            .count();
        let terminal_turn_events = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    JournalEventType::Turn | JournalEventType::TurnError
                )
            })
            .count();
        let other_commits = events.iter().any(|event| {
            event
                .conversation_commit
                .as_ref()
                .is_some_and(|commit| commit != intended)
        });
        if intended_turns != 1 || terminal_turn_events != 1 || other_commits {
            return CanonicalCommitCasOutcome::NotCommitted(
                "canonical CAS batch must contain exactly one intended turn commit".to_string(),
            );
        }

        let opened = match open_unlocked_journal_file_with_creation(&self.path) {
            Ok(opened) => opened,
            Err(error) => {
                return CanonicalCommitCasOutcome::Unknown(format!(
                    "failed to open canonical journal CAS: {error}"
                ));
            }
        };
        let journal_created = opened.created;
        let mut file = opened.file;
        #[cfg(test)]
        run_canonical_commit_cas_open_hook(&self.path);
        if let Err(error) = file.lock_exclusive() {
            return CanonicalCommitCasOutcome::Unknown(format!(
                "failed to lock canonical journal CAS: {error}"
            ));
        }
        match open_journal_file_is_current(&file, &self.path) {
            Ok(true) => {}
            Ok(false) => {
                return CanonicalCommitCasOutcome::Unknown(
                    "canonical journal generation rotated before CAS validation".to_string(),
                );
            }
            Err(error) => {
                return CanonicalCommitCasOutcome::Unknown(format!(
                    "failed to verify canonical journal generation: {error}"
                ));
            }
        }

        if let Err(error) = file.seek(SeekFrom::Start(0)) {
            return CanonicalCommitCasOutcome::Unknown(format!(
                "failed to seek canonical journal CAS: {error}"
            ));
        }
        let mut content = String::new();
        if let Err(error) = file.read_to_string(&mut content) {
            return CanonicalCommitCasOutcome::Unknown(format!(
                "failed to read canonical journal CAS: {error}"
            ));
        }
        let (current_events, _, malformed_lines) =
            match parse_complete_journal_text_in_append_order(&content) {
                Ok(parsed) => parsed,
                Err(error) => {
                    return CanonicalCommitCasOutcome::Unknown(format!(
                        "canonical journal CAS readback is incomplete: {error}"
                    ));
                }
            };
        match classify_canonical_commit_presence(&current_events, malformed_lines, turn, intended) {
            CanonicalCommitPresence::Exact => {
                if let Err(error) = file.sync_data() {
                    return CanonicalCommitCasOutcome::Unknown(format!(
                        "failed canonical journal CAS durability fence: {error}"
                    ));
                }
                return match open_journal_file_is_current(&file, &self.path) {
                    Ok(true) => {
                        if let Err(error) = self.sync_creation_metadata_if_needed(
                            journal_created,
                            false,
                            lease_created_dirs,
                        ) {
                            CanonicalCommitCasOutcome::Unknown(format!(
                                "failed to sync canonical journal creation metadata: {error}"
                            ))
                        } else {
                            CanonicalCommitCasOutcome::Committed {
                                persistence_warning: None,
                            }
                        }
                    }
                    Ok(false) => CanonicalCommitCasOutcome::Unknown(
                        "canonical commit exists only on a rotated journal generation".to_string(),
                    ),
                    Err(error) => CanonicalCommitCasOutcome::Unknown(format!(
                        "failed to confirm canonical journal generation: {error}"
                    )),
                };
            }
            CanonicalCommitPresence::Conflict(reason) => {
                return CanonicalCommitCasOutcome::Conflict(reason);
            }
            CanonicalCommitPresence::Unknown(reason) => {
                return CanonicalCommitCasOutcome::Unknown(reason);
            }
            CanonicalCommitPresence::Absent => {}
        }

        let current_base_cursor = current_events
            .iter()
            .filter_map(|event| event.conversation_commit.as_ref())
            .next_back()
            .map(|commit| &commit.cursor);
        let first_canonical_commit = current_base_cursor.is_none();
        if current_base_cursor != expected_base_cursor {
            return CanonicalCommitCasOutcome::Conflict(
                "canonical journal base changed before turn settlement".to_string(),
            );
        }

        let needs_session_start = current_events
            .last()
            .is_none_or(|event| event.event_type == JournalEventType::SessionEnd);
        let append_events = prepend_session_start_for_known_state(events, needs_session_start);
        let serialized = match serialize_journal_events(append_events.as_ref()) {
            Ok(serialized) => serialized,
            Err(error) => {
                return CanonicalCommitCasOutcome::NotCommitted(format!(
                    "failed to serialize canonical journal CAS: {error}"
                ));
            }
        };
        let write_error = file.write_all(&serialized).err();
        if let Err(error) = file.sync_data() {
            return CanonicalCommitCasOutcome::Unknown(format!(
                "failed canonical journal CAS durability fence: {error}"
            ));
        }
        match open_journal_file_is_current(&file, &self.path) {
            Ok(true) => {}
            Ok(false) => {
                return CanonicalCommitCasOutcome::Unknown(
                    "canonical journal generation rotated after CAS write".to_string(),
                );
            }
            Err(error) => {
                return CanonicalCommitCasOutcome::Unknown(format!(
                    "failed to confirm canonical journal generation: {error}"
                ));
            }
        }

        if let Err(error) = file.seek(SeekFrom::Start(0)) {
            return CanonicalCommitCasOutcome::Unknown(format!(
                "failed to seek canonical journal CAS verification: {error}"
            ));
        }
        let mut verified_content = String::new();
        if let Err(error) = file.read_to_string(&mut verified_content) {
            return CanonicalCommitCasOutcome::Unknown(format!(
                "failed to read canonical journal CAS verification: {error}"
            ));
        }
        let (verified_events, _, verified_malformed_lines) =
            match parse_complete_journal_text_in_append_order(&verified_content) {
                Ok(parsed) => parsed,
                Err(error) => {
                    return CanonicalCommitCasOutcome::Unknown(format!(
                        "canonical journal CAS verification is incomplete: {error}"
                    ));
                }
            };
        match classify_canonical_commit_presence(
            &verified_events,
            verified_malformed_lines,
            turn,
            intended,
        ) {
            CanonicalCommitPresence::Exact => {
                match open_journal_file_is_current(&file, &self.path) {
                    Ok(true) => {}
                    Ok(false) => {
                        return CanonicalCommitCasOutcome::Unknown(
                            "canonical journal generation rotated during CAS verification"
                                .to_string(),
                        );
                    }
                    Err(error) => {
                        return CanonicalCommitCasOutcome::Unknown(format!(
                            "failed final canonical journal generation check: {error}"
                        ));
                    }
                }
                if let Err(error) = self.sync_creation_metadata_if_needed(
                    journal_created,
                    first_canonical_commit,
                    lease_created_dirs,
                ) {
                    return CanonicalCommitCasOutcome::Unknown(format!(
                        "failed to sync canonical journal creation metadata: {error}"
                    ));
                }
                update_cached_session_start_state_from_events(&self.path, append_events.as_ref());
                CanonicalCommitCasOutcome::Committed {
                        persistence_warning: write_error.map(|error| {
                            format!(
                                "canonical journal CAS write reported an error but exact durable verification succeeded: {error}"
                            )
                        }),
                    }
            }
            CanonicalCommitPresence::Absent => {
                if let Some(error) = write_error {
                    CanonicalCommitCasOutcome::NotCommitted(format!(
                        "canonical journal CAS write failed and exact commit is absent: {error}"
                    ))
                } else {
                    CanonicalCommitCasOutcome::Unknown(
                        "canonical journal CAS write succeeded but exact commit is absent"
                            .to_string(),
                    )
                }
            }
            CanonicalCommitPresence::Conflict(reason)
            | CanonicalCommitPresence::Unknown(reason) => CanonicalCommitCasOutcome::Unknown(
                format!("canonical journal CAS verification failed: {reason}"),
            ),
        }
    }

    /// Commit prior no-sync appends to the local durable boundary without
    /// manufacturing another journal event. Long-running child agents can
    /// expose live pages after each round, then pay one fsync at terminal
    /// settlement before advertising transcript reconciliation to the UI.
    pub fn sync_data(&self) -> std::io::Result<()> {
        let file = open_locked_journal_file(&self.path)?;
        file.sync_data()
    }

    /// Read one complete current-generation snapshot in physical append
    /// order. Canonical replay must use this order rather than timestamps,
    /// which are diagnostic and may move backwards across processes.
    pub fn complete_append_order_snapshot(&self) -> std::io::Result<Vec<JournalEvent>> {
        use fs2::FileExt;
        use std::io::{Read, Seek, SeekFrom};

        const MAX_GENERATION_RETRIES: usize = 8;
        for _ in 0..MAX_GENERATION_RETRIES {
            let mut file = match std::fs::OpenOptions::new().read(true).open(&self.path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(error) => return Err(error),
            };
            FileExt::lock_shared(&file)?;
            if !open_journal_file_is_current(&file, &self.path)? {
                continue;
            }
            file.seek(SeekFrom::Start(0))?;
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            if !open_journal_file_is_current(&file, &self.path)? {
                continue;
            }
            let (events, _, malformed_lines) =
                parse_complete_journal_text_in_append_order(&content)?;
            if malformed_lines > 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("journal contains {malformed_lines} malformed record(s)"),
                ));
            }
            record_journal_read(
                astra_core::history_work::HistoryWorkSite::SessionJournalDigestRead,
                content.len(),
                events.len(),
            );
            return Ok(events);
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "journal {} kept rotating during append-order snapshot",
                self.session_id
            ),
        ))
    }

    /// Establish a durability fence and read the exact journal generation
    /// protected by that fence.
    ///
    /// This is intended for reconciling an ambiguous append result: callers
    /// must not treat an ordinary pathname read as proof that page-cache
    /// bytes survived the failed durability boundary.
    pub fn durable_readback(&self) -> std::io::Result<(Vec<JournalEvent>, usize, usize)> {
        self.durable_readback_with(|file| file.sync_data())
    }

    fn durable_readback_with<F>(
        &self,
        sync_data: F,
    ) -> std::io::Result<(Vec<JournalEvent>, usize, usize)>
    where
        F: Fn(&std::fs::File) -> std::io::Result<()>,
    {
        use fs2::FileExt;
        use std::io::{Read, Seek, SeekFrom};

        const MAX_GENERATION_RETRIES: usize = 8;
        for _ in 0..MAX_GENERATION_RETRIES {
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .append(true)
                .open(&self.path)?;
            file.lock_exclusive()?;
            if !open_journal_file_is_current(&file, &self.path)? {
                continue;
            }

            sync_data(&file)?;
            file.seek(SeekFrom::Start(0))?;
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            if !open_journal_file_is_current(&file, &self.path)? {
                continue;
            }

            let parsed = parse_complete_journal_text(&content)?;
            record_journal_read(
                astra_core::history_work::HistoryWorkSite::SessionJournalDigestRead,
                content.len(),
                parsed.0.len(),
            );
            return Ok(parsed);
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "journal {} kept rotating during durable readback",
                self.session_id
            ),
        ))
    }

    fn append_bulk_inner(&self, events: &[JournalEvent], sync: bool) -> std::io::Result<()> {
        use std::io::Write;
        if events.is_empty() {
            return Ok(());
        }
        let mut file = open_locked_journal_file(&self.path)?;
        let events = prepend_session_start_if_needed(&self.path, events)?;
        let buf = serialize_journal_events(events.as_ref())?;
        file.write_all(&buf)?;
        if sync {
            file.sync_data()?;
        }
        update_cached_session_start_state_from_events(&self.path, events.as_ref());
        Ok(())
    }
}

// ─── Turn Event Buffer ───────────────────────────────────────────────────────

/// Data for one LLM→tools round within a turn.
pub struct LlmRoundRecord {
    pub purpose: InferencePurpose,
    pub ttft_ms: Option<u64>,
    pub duration_ms: u64,
    /// Fresh input tokens for this provider call. Cache buckets are disjoint.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub tool_calls_returned: u32,
    pub tool_call_names: Vec<String>,
    pub finish_reason: Option<String>,
    pub agentic_step: Option<u32>,
    pub source: Option<String>,
    pub run_id: Option<String>,
    /// Durable parent run for a child producer. Root rounds leave this unset.
    pub parent_run_id: Option<String>,
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    /// When set, this round belongs to a child agent (not the parent).
    /// Written into the typed producer scope so consumers cannot confuse the
    /// child's local loop counter with a root session turn.
    pub agent_id: Option<String>,
}

impl LlmRoundRecord {
    /// Start an LLM-round record with an explicit policy/usage purpose.
    /// Optional observability fields may be filled by the caller, but purpose
    /// can never be inherited from a default or guessed from source text.
    pub fn new(purpose: InferencePurpose) -> Self {
        Self {
            purpose,
            ttft_ms: None,
            duration_ms: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: Vec::new(),
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            parent_run_id: None,
            tool_calls: None,
            agent_id: None,
        }
    }
}

/// In-memory collector for fine-grained turn events.
///
/// Events are accumulated during a turn and flushed to the journal in a single
/// IO operation when the turn completes. On interruption, `flush_interrupted`
/// writes partial data without fsync.
/// Max events before oldest are evicted (ring-buffer semantics).
const TURN_EVENT_BUFFER_CAP: usize = 1000;
const TURN_EVENT_DROPPED_META_KEY: &str = "dropped_events_before";

pub struct TurnEventBuffer {
    events: std::collections::VecDeque<JournalEvent>,
    dropped_events: u64,
    turn_start: std::time::Instant,
    turn_started_at: chrono::DateTime<chrono::Utc>,
    session_id: Option<String>,
    /// Canonical user-visible session turn. Child producers deliberately have
    /// no session turn; their local turn belongs in `producer_turn` instead.
    session_turn: Option<u32>,
    /// Producer-local turn for child/sub-run telemetry. This must never be
    /// joined to a root turn without an explicit run lineage relation.
    producer_turn: Option<u32>,
    round: u32,
    batch_counter: u32,
    /// Action union from the exact tool schemas sent on the current provider
    /// boundary. Tool names alone cannot prove whether a sensitive action
    /// (for example `agent_fanout.start`) was actually visible.
    visible_tool_actions: Option<std::collections::BTreeMap<String, Vec<String>>>,
    /// Exact provider-visible tool names for the current round. This is a
    /// bounded schema projection, not the schemas themselves, and lets
    /// lifecycle diagnostics prove that an assigned run was actually given
    /// its required continuation operation.
    visible_tool_names: Option<Vec<String>>,
}

impl TurnEventBuffer {
    /// Start collecting events for a new turn.
    pub fn begin_turn(session_id: Option<&str>, turn: u32) -> Self {
        Self::begin_turn_with_round(session_id, turn, 0)
    }

    /// Start collecting events for a new turn at a specific round offset.
    pub fn begin_turn_with_round(session_id: Option<&str>, turn: u32, round: u32) -> Self {
        Self {
            events: std::collections::VecDeque::new(),
            dropped_events: 0,
            turn_start: std::time::Instant::now(),
            turn_started_at: chrono::Utc::now(),
            session_id: session_id.map(ToString::to_string),
            session_turn: Some(turn),
            producer_turn: None,
            round,
            batch_counter: 0,
            visible_tool_actions: None,
            visible_tool_names: None,
        }
    }

    /// Start a buffer for a child/sub-run producer.
    ///
    /// Unlike [`Self::begin_turn`], this constructor does not fabricate a
    /// session turn from the child's local loop index. The producer-local turn
    /// is retained in metadata while `JournalEvent::turn` remains unset.
    pub fn begin_producer_turn(session_id: Option<&str>, producer_turn: u32) -> Self {
        Self {
            events: std::collections::VecDeque::new(),
            dropped_events: 0,
            turn_start: std::time::Instant::now(),
            turn_started_at: chrono::Utc::now(),
            session_id: session_id.map(ToString::to_string),
            session_turn: None,
            producer_turn: Some(producer_turn),
            round: 0,
            batch_counter: 0,
            visible_tool_actions: None,
            visible_tool_names: None,
        }
    }

    /// Bind a server-assigned session identity after the first streamed LLM
    /// response reveals it. Events captured earlier in that same turn are
    /// retrofitted atomically so first-round telemetry is not orphaned.
    pub fn bind_session_id(&mut self, session_id: &str) -> Result<(), String> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err("cannot bind an empty session_id to turn events".to_string());
        }
        if let Some(existing) = self.session_id.as_deref()
            && existing != session_id
        {
            return Err(format!(
                "turn event buffer session mismatch: existing={existing}, incoming={session_id}"
            ));
        }
        if let Some(conflicting) = self.events.iter().find_map(|event| {
            event
                .session_id
                .as_deref()
                .filter(|existing| *existing != session_id)
        }) {
            return Err(format!(
                "buffered event session mismatch: existing={conflicting}, incoming={session_id}"
            ));
        }
        self.session_id = Some(session_id.to_string());
        for event in &mut self.events {
            if event.session_id.is_none() {
                event.session_id = Some(session_id.to_string());
            }
        }
        Ok(())
    }

    /// Push event, evicting oldest if buffer is full (ring-buffer semantics).
    fn push_event(&mut self, event: JournalEvent) {
        self.events.push_back(event);
        while self.events.len() > TURN_EVENT_BUFFER_CAP {
            self.events.pop_front();
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
    }

    /// Elapsed milliseconds since turn start.
    pub fn offset_ms(&self) -> u64 {
        self.turn_start.elapsed().as_millis() as u64
    }

    /// The instant when this turn started (for passing to sub-contexts).
    pub fn turn_start_instant(&self) -> std::time::Instant {
        self.turn_start
    }

    /// Wall-clock anchor paired with [`Self::turn_start_instant`]. Trace
    /// projections use this to preserve the observed event timeline instead
    /// of assigning post-loop import time to every round/tool.
    pub fn turn_started_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.turn_started_at
    }

    /// Current LLM round index (0-based).
    pub fn current_round(&self) -> u32 {
        self.round
    }

    /// Generate a batch ID for a group of parallel tool executions.
    pub fn next_batch_id(&mut self) -> String {
        let id = format!("b-{}-{}", self.round, self.batch_counter);
        self.batch_counter += 1;
        id
    }

    /// Persist a bounded projection of the exact action surface for the next
    /// LLM round. The full schemas remain out of the journal; this map is the
    /// minimum evidence needed to distinguish "tool name visible" from
    /// "requested action branch visible" during incident review.
    pub fn set_visible_tool_actions(
        &mut self,
        actions: std::collections::BTreeMap<String, Vec<String>>,
    ) {
        self.visible_tool_actions = (!actions.is_empty()).then_some(actions);
    }

    /// Persist the bounded name projection of the exact provider wire
    /// surface for the next LLM round.
    pub fn set_visible_tool_names(&mut self, names: Vec<String>) {
        let mut seen = std::collections::HashSet::new();
        let mut names = names;
        names.retain(|name| !name.trim().is_empty() && seen.insert(name.clone()));
        self.visible_tool_names = Some(names);
    }

    /// Record an LLM round completion (one LLM→tools cycle).
    pub fn record_llm_round(&mut self, r: LlmRoundRecord) {
        let mut evt = JournalEvent::base(JournalEventType::LlmRound, self.session_id.as_deref());
        evt.turn = self.session_turn;
        if let Some(run_id) = r.run_id.as_ref().filter(|run_id| !run_id.trim().is_empty()) {
            evt.producer_scope = Some(JournalProducerScope {
                run_id: run_id.clone(),
                parent_run_id: r.parent_run_id.clone(),
                agent_id: r.agent_id.clone(),
                local_turn: self.producer_turn,
            });
        }
        evt.agentic_step = r.agentic_step;
        evt.round = Some(self.round);
        evt.offset_ms = Some(self.offset_ms().saturating_sub(r.duration_ms));
        evt.ttft_ms = r.ttft_ms;
        evt.duration_ms = Some(r.duration_ms);
        evt.tokens_in = Some(r.prompt_tokens);
        evt.tokens_out = Some(r.completion_tokens);
        if r.cache_read_tokens > 0 {
            evt.cache_read_tokens = Some(r.cache_read_tokens);
        }
        if r.cache_creation_tokens > 0 {
            evt.cache_creation_tokens = Some(r.cache_creation_tokens);
        }
        evt.tool_calls_returned = Some(r.tool_calls_returned);
        if let Some(tool_calls) = r.tool_calls {
            evt = evt.with_tool_calls(tool_calls);
        }
        let mut meta = serde_json::Map::new();
        meta.insert("purpose".into(), serde_json::json!(r.purpose));
        // Keep the observability facts alongside the canonical event fields
        // when this journal is uploaded to `agent_events`. The cloud event
        // schema stores generic metadata plus token counters, so without
        // these bounded scalar fields a normal `llm_round` loses its duration
        // and TTFT and reflection cannot explain where a session spent time.
        if r.duration_ms > 0 {
            meta.insert("duration_ms".into(), serde_json::json!(r.duration_ms));
        }
        if let Some(ttft_ms) = r.ttft_ms {
            meta.insert("ttft_ms".into(), serde_json::json!(ttft_ms));
        }
        if r.prompt_tokens > 0 {
            meta.insert("prompt_tokens".into(), serde_json::json!(r.prompt_tokens));
        }
        if r.completion_tokens > 0 {
            meta.insert(
                "completion_tokens".into(),
                serde_json::json!(r.completion_tokens),
            );
        }
        if r.cache_read_tokens > 0 {
            meta.insert(
                "cache_read_tokens".into(),
                serde_json::json!(r.cache_read_tokens),
            );
        }
        if r.cache_creation_tokens > 0 {
            meta.insert(
                "cache_creation_tokens".into(),
                serde_json::json!(r.cache_creation_tokens),
            );
        }
        if !r.tool_call_names.is_empty() {
            meta.insert(
                "tool_call_names".into(),
                serde_json::json!(r.tool_call_names),
            );
        }
        if let Some(actions) = self.visible_tool_actions.as_ref() {
            meta.insert("visible_tool_actions".into(), serde_json::json!(actions));
        }
        if let Some(names) = self.visible_tool_names.as_ref() {
            meta.insert("visible_tools".into(), serde_json::json!(names));
        }
        if let Some(finish_reason) = r.finish_reason {
            meta.insert("finish_reason".into(), serde_json::json!(finish_reason));
        }
        if let Some(source) = r.source {
            meta.insert("source".into(), serde_json::json!(source));
        }
        evt.metadata = Some(serde_json::Value::Object(meta));
        self.push_event(evt);
        self.round += 1;
        self.batch_counter = 0;
    }

    /// Record a single event (generic).
    pub fn record(&mut self, event: JournalEvent) {
        self.push_event(event);
    }

    /// Record a trace span via builder — preferred API.
    pub fn record_trace_span_v2(&mut self, builder: TraceSpanBuilder) {
        let mut evt = builder
            .session_id(self.session_id.as_deref())
            .turn(self.session_turn)
            .build();
        // base() may have already set session_id; let the builder override win
        evt.session_id = self.session_id.clone();
        evt.turn = self.session_turn;
        self.push_event(evt);
    }

    /// Number of events collected so far.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Number of oldest events evicted from the in-memory ring buffer for the
    /// current turn. When non-zero, any flush/drain result is necessarily
    /// partial and the first surviving event is annotated with the count.
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    /// Whether no events have been collected.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Flush all collected events to the journal (one IO, with fsync).
    pub fn flush(&mut self, writer: &JournalWriter) -> std::io::Result<()> {
        if self.events.is_empty() {
            return Ok(());
        }
        let events = self.events.make_contiguous();
        annotate_dropped_turn_events(events, self.dropped_events);
        writer.append_bulk(events)?;
        self.events.clear();
        self.dropped_events = 0;
        Ok(())
    }

    /// Best-effort flush on interruption: no fsync, marks events as partial.
    pub fn flush_interrupted(&mut self, writer: &JournalWriter) -> std::io::Result<()> {
        if self.events.is_empty() {
            return Ok(());
        }
        for event in &mut self.events {
            let meta = event.metadata.get_or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = meta.as_object_mut() {
                obj.insert("partial".into(), serde_json::json!(true));
            }
        }
        let events = self.events.make_contiguous();
        annotate_dropped_turn_events(events, self.dropped_events);
        writer.append_bulk_no_sync(events)?;
        self.events.clear();
        self.dropped_events = 0;
        Ok(())
    }

    /// Drain collected events (for callers that persist elsewhere, e.g. DB).
    pub fn drain(&mut self) -> Vec<JournalEvent> {
        let mut events: Vec<JournalEvent> = std::mem::take(&mut self.events).into();
        annotate_dropped_turn_events(&mut events, self.dropped_events);
        self.dropped_events = 0;
        events
    }
}

fn annotate_dropped_turn_events(events: &mut [JournalEvent], dropped_events: u64) {
    if dropped_events == 0 || events.is_empty() {
        return;
    }
    let meta = events[0]
        .metadata
        .get_or_insert_with(|| serde_json::json!({}));
    if let Some(obj) = meta.as_object_mut() {
        obj.insert(
            TURN_EVENT_DROPPED_META_KEY.into(),
            serde_json::json!(dropped_events),
        );
    }
}

fn parse_journal_text(content: &str) -> (Vec<JournalEvent>, usize, usize) {
    let (mut events, non_empty_lines, malformed_lines) =
        parse_journal_text_in_append_order(content);
    stabilize_event_order(&mut events);
    (events, non_empty_lines, malformed_lines)
}

fn parse_complete_journal_text(
    content: &str,
) -> std::io::Result<(Vec<JournalEvent>, usize, usize)> {
    if !content.is_empty() && !content.ends_with('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "journal has a non-empty unterminated tail",
        ));
    }
    Ok(parse_journal_text(content))
}

fn parse_complete_journal_text_in_append_order(
    content: &str,
) -> std::io::Result<(Vec<JournalEvent>, usize, usize)> {
    if !content.is_empty() && !content.ends_with('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "journal has a non-empty unterminated tail",
        ));
    }
    Ok(parse_journal_text_in_append_order(content))
}

/// Parse physical append order without timestamp repair. This is the cursor
/// order for append-only streams such as a root conversation: a timestamp can
/// drift, but a completed durable write has one unambiguous position in the
/// journal.
fn parse_journal_text_in_append_order(content: &str) -> (Vec<JournalEvent>, usize, usize) {
    let mut events = Vec::new();
    let mut non_empty_lines = 0usize;
    let mut malformed_lines = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        non_empty_lines += 1;
        match serde_json::from_str::<JournalEvent>(line) {
            Ok(evt) => events.push(evt),
            Err(_) => malformed_lines += 1,
        }
    }
    (events, non_empty_lines, malformed_lines)
}

/// Read all events from a session journal file.
pub fn read_journal(session_id: &str) -> std::io::Result<Vec<JournalEvent>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    read_journal_from_path(&journal_file_path(session_id))
}

pub fn read_journal_for_owner(
    owner_scope: &OwnerScope,
    session_id: &str,
) -> std::io::Result<Vec<JournalEvent>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    read_journal_from_path(&journal_file_path_for_owner(owner_scope, session_id)?)
}

pub fn read_journal_for_user(
    user_id: &str,
    session_id: &str,
) -> std::io::Result<Vec<JournalEvent>> {
    let owner_scope = OwnerScope::user(user_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    read_journal_for_owner(&owner_scope, session_id)
}

fn read_journal_from_path(path: &Path) -> std::io::Result<Vec<JournalEvent>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    let events = parse_journal_text(&content).0;
    record_journal_read(
        astra_core::history_work::HistoryWorkSite::SessionJournalFullRead,
        content.len(),
        events.len(),
    );
    Ok(events)
}

/// Read a session journal in physical append order.
///
/// This is intentionally distinct from [`read_journal`], whose chronological
/// projection repairs clock drift for timeline consumers. Transcript cursors
/// need durable append order instead: a cursor must not move when a host clock
/// is corrected after a record has been written.
pub fn read_journal_append_order(session_id: &str) -> std::io::Result<Vec<JournalEvent>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let path = journal_file_path(session_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)?;
    let events = parse_journal_text_in_append_order(&content).0;
    record_journal_read(
        astra_core::history_work::HistoryWorkSite::SessionJournalFullRead,
        content.len(),
        events.len(),
    );
    Ok(events)
}

/// Complete append-only journal records available after a durable byte cursor.
///
/// The cursor advances only over newline-terminated JSONL records. A concurrent
/// writer may have opened or partially written its final line; leaving that
/// suffix for the next pass prevents a derived projection from recording a
/// malformed or half-written event and then advancing past it.
#[derive(Debug, Clone)]
pub struct JournalAppendDelta {
    pub events: Vec<JournalEvent>,
    pub next_offset: u64,
}

pub fn read_durable_journal_append_delta(
    session_id: &str,
    offset: u64,
) -> std::io::Result<JournalAppendDelta> {
    read_durable_journal_append_delta_for_owner(&OwnerScope::local_user(), session_id, offset)
}

pub fn read_durable_journal_append_delta_for_owner(
    owner_scope: &OwnerScope,
    session_id: &str,
    offset: u64,
) -> std::io::Result<JournalAppendDelta> {
    use std::io::{Read, Seek, SeekFrom};

    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let path = journal_file_path_for_owner(owner_scope, session_id)?;
    if offset == 0 && !path.exists() {
        return Ok(JournalAppendDelta {
            events: Vec::new(),
            next_offset: 0,
        });
    }
    // Hold the same exclusive file lock used by appenders while establishing
    // the durability barrier and reading the suffix. Without this, an appender
    // could add no-sync bytes after sync_data but before read_to_end, allowing
    // a derived outbox cursor to advance beyond the crash-durable journal.
    let mut file = open_locked_journal_file(&path)?;
    file.sync_data()?;
    let len = file.metadata()?.len();
    if offset > len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "journal {session_id} shrank below durable outbox cursor: cursor={offset} bytes={len}"
            ),
        ));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut suffix = Vec::new();
    file.read_to_end(&mut suffix)?;
    let complete_len = suffix
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    if complete_len == 0 {
        record_journal_read(
            astra_core::history_work::HistoryWorkSite::SessionJournalAppendDeltaRead,
            suffix.len(),
            0,
        );
        return Ok(JournalAppendDelta {
            events: Vec::new(),
            next_offset: offset,
        });
    }
    let complete = std::str::from_utf8(&suffix[..complete_len]).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("journal {session_id} contains non-UTF-8 JSONL: {error}"),
        )
    })?;
    let mut events = Vec::new();
    for (line_index, line) in complete.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event = serde_json::from_str::<JournalEvent>(line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "journal {session_id} has invalid JSONL at append line {}: {error}",
                    line_index + 1
                ),
            )
        })?;
        events.push(event);
    }
    record_journal_read(
        astra_core::history_work::HistoryWorkSite::SessionJournalAppendDeltaRead,
        suffix.len(),
        events.len(),
    );
    Ok(JournalAppendDelta {
        events,
        next_offset: offset.saturating_add(complete_len as u64),
    })
}

/// Rotation-aware cursor used by approval waiters.
///
/// A byte offset alone is not an identity: after atomic replacement the same
/// offset may point into unrelated JSONL. Keep file generation with the byte
/// position so replacement is detected even when the new file has the exact
/// same length. The generic sync-outbox cursor remains monotonic and separate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalJournalCursor {
    offset: u64,
    identity: Option<JournalFileIdentity>,
    version: Option<JournalFileVersion>,
    prefix_fingerprint: Option<[u8; 32]>,
}

impl ApprovalJournalCursor {
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct JournalFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn journal_file_identity(metadata: &std::fs::Metadata) -> Option<JournalFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(JournalFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct JournalFileIdentity;

#[cfg(not(unix))]
fn journal_file_identity(_metadata: &std::fs::Metadata) -> Option<JournalFileIdentity> {
    // Correctness wins over a speculative timestamp fingerprint: without a
    // stable platform file id, callers rescan rather than risk missing a
    // same-size replacement.
    None
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct JournalFileVersion {
    len: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
fn journal_file_version(metadata: &std::fs::Metadata) -> Option<JournalFileVersion> {
    use std::os::unix::fs::MetadataExt;
    Some(JournalFileVersion {
        len: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(not(unix))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct JournalFileVersion {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

#[cfg(not(unix))]
fn journal_file_version(metadata: &std::fs::Metadata) -> Option<JournalFileVersion> {
    Some(JournalFileVersion {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// Hash the complete consumed prefix whenever the file version changes.
///
/// File identity catches atomic replacement. Metadata makes the unchanged
/// polling path O(1). A complete prefix digest, rather than sampled boundary
/// windows, proves that an apparent append did not actually truncate/regrow or
/// rewrite the middle of a large same-inode journal between polls.
fn approval_cursor_prefix_fingerprint(
    file: &mut std::fs::File,
    offset: u64,
) -> std::io::Result<Option<[u8; 32]>> {
    use std::io::{Read, Seek, SeekFrom};

    if offset > file.metadata()?.len() {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    hasher.update(b"astra-approval-cursor-v2");
    hasher.update(offset.to_le_bytes());
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = offset;
    let mut buffer = [0_u8; 16 * 1024];
    while remaining > 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..take])?;
        hasher.update(&buffer[..take]);
        remaining -= take as u64;
    }
    Ok(Some(hasher.finalize().into()))
}

fn read_approval_journal_append_delta_for_user(
    user_id: &str,
    session_id: &str,
    cursor: Option<&ApprovalJournalCursor>,
) -> std::io::Result<(Vec<JournalEvent>, ApprovalJournalCursor)> {
    use fs2::FileExt;
    use std::io::{Read, Seek, SeekFrom};

    validate_session_id(session_id)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let path = journal_file_path_for_user(user_id, session_id)?;
    let mut file = match std::fs::OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), ApprovalJournalCursor::default()));
        }
        Err(error) => return Err(error),
    };
    let unlocked_metadata = file.metadata()?;
    let unlocked_identity = journal_file_identity(&unlocked_metadata);
    let unlocked_version = journal_file_version(&unlocked_metadata);
    if let Some(cursor) = cursor
        && cursor.identity.is_some()
        && cursor.identity == unlocked_identity
        && cursor.version == unlocked_version
        && cursor.offset == unlocked_metadata.len()
    {
        // Same inode, length and nanosecond change metadata: the generation is
        // unchanged. This high-frequency path is O(1), takes no lock and does
        // no I/O proportional to session size. When metadata changes, the slow
        // path verifies the complete old prefix before trusting the offset.
        // A racing append is harmless: it is observed on the next polling tick.
        return Ok((Vec::new(), cursor.clone()));
    }

    if let Err(error) = FileExt::try_lock_shared(&file) {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            // An appender owns the file. Never block an async approval waiter
            // or consume a blocking-pool thread behind another session; the
            // next polling tick retries after the writer has committed.
            return Ok((
                Vec::new(),
                cursor
                    .cloned()
                    .unwrap_or_else(ApprovalJournalCursor::default),
            ));
        }
        return Err(error);
    }
    let metadata = file.metadata()?;
    let identity = journal_file_identity(&metadata);
    let version = journal_file_version(&metadata);
    let prefix_matches = if let Some(cursor) = cursor
        && cursor.identity.is_some()
        && cursor.identity == identity
        && cursor.offset <= metadata.len()
    {
        approval_cursor_prefix_fingerprint(&mut file, cursor.offset)? == cursor.prefix_fingerprint
    } else {
        false
    };
    let offset = cursor
        .filter(|_| prefix_matches)
        .map(|cursor| cursor.offset)
        .unwrap_or(0);
    file.seek(SeekFrom::Start(offset))?;
    let mut suffix = Vec::new();
    file.read_to_end(&mut suffix)?;
    let complete_len = suffix
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let next_offset = offset.saturating_add(complete_len as u64);
    let next_cursor = ApprovalJournalCursor {
        offset: next_offset,
        identity,
        version,
        prefix_fingerprint: approval_cursor_prefix_fingerprint(&mut file, next_offset)?,
    };
    if complete_len == 0 {
        record_journal_read(
            astra_core::history_work::HistoryWorkSite::SessionJournalAppendDeltaRead,
            suffix.len(),
            0,
        );
        return Ok((Vec::new(), next_cursor));
    }
    let complete = std::str::from_utf8(&suffix[..complete_len]).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("journal {session_id} contains non-UTF-8 JSONL: {error}"),
        )
    })?;
    let mut events = Vec::new();
    for (line_index, line) in complete.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        events.push(serde_json::from_str::<JournalEvent>(line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "journal {session_id} has invalid JSONL at append line {}: {error}",
                    line_index + 1
                ),
            )
        })?);
    }
    record_journal_read(
        astra_core::history_work::HistoryWorkSite::SessionJournalAppendDeltaRead,
        suffix.len(),
        events.len(),
    );
    Ok((events, next_cursor))
}

/// Read the last `limit` events from a session journal file.
///
/// This avoids loading the entire journal into memory for long-running sessions
/// where only recent events are relevant (e.g. cache-hit diagnostics).
pub fn read_journal_tail(session_id: &str, limit: usize) -> std::io::Result<Vec<JournalEvent>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    read_journal_tail_from_path(&journal_file_path(session_id), limit)
}

pub fn read_journal_tail_for_user(
    user_id: &str,
    session_id: &str,
    limit: usize,
) -> std::io::Result<Vec<JournalEvent>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    read_journal_tail_from_path(&journal_file_path_for_user(user_id, session_id)?, limit)
}

fn read_journal_tail_from_path(path: &Path, limit: usize) -> std::io::Result<Vec<JournalEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    if !path.exists() {
        return Ok(Vec::new());
    }

    // Seek backwards from EOF instead of scanning the complete JSONL file.
    // Long-lived CLI sessions can accumulate hundreds of thousands of
    // events; a bounded tail read must bound I/O as well as retained memory.
    let tail_lines = read_journal_tail_lines_exact(path, limit)?;
    let mut events = Vec::with_capacity(tail_lines.len());
    for line in tail_lines {
        if let Ok(event) = serde_json::from_str::<JournalEvent>(&line) {
            events.push(event);
        }
    }
    stabilize_event_order(&mut events);
    Ok(events)
}

pub fn journal_needs_session_start(session_id: &str) -> std::io::Result<bool> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let path = journal_file_path(session_id);
    journal_needs_session_start_for_path(&path)
}

pub fn ensure_session_start_event(session_id: &str, model: Option<&str>) -> std::io::Result<()> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let path = journal_file_path(session_id);

    // Acquire the lock first — concurrent writers (including external
    // processes or edge-cloud sync replays) may have modified the file
    // since the last cache write; we must not return early based on a
    // stale cache entry without holding the lock.
    let mut file = open_locked_journal_file(&path)?;
    // Under lock, bypass the process-local cache to avoid TOCTOU:
    // another process may have written SessionEnd between our last
    // cache update and this lock acquisition.
    if journal_needs_session_start_impl(&path, /*skip_cache=*/ true)? {
        use std::io::Write;
        let events = vec![JournalEvent::session_start(Some(session_id), model)];
        let buf = serialize_journal_events(&events)?;
        file.write_all(&buf)?;
        file.sync_data()?;
        update_cached_session_start_state_from_events(&path, &events);
    }
    Ok(())
}

fn approval_metadata_str(metadata: &serde_json::Value, field: &str) -> Option<String> {
    metadata
        .get("approval")
        .and_then(|approval| approval.get(field))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
}

pub fn find_latest_approval_decision(
    session_id: &str,
    request_id: &str,
) -> std::io::Result<Option<ApprovalJournalDecision>> {
    find_latest_approval_decision_impl(session_id, request_id, None)
}

pub fn find_latest_approval_decision_for_run(
    session_id: &str,
    request_id: &str,
    run_id: &str,
) -> std::io::Result<Option<ApprovalJournalDecision>> {
    find_latest_approval_decision_impl(session_id, request_id, Some(run_id))
}

/// Find a durable approval decision inside one authenticated user's journal.
///
/// Cloud/server callers must use this instead of the process-local lookup so
/// replay receipts cannot escape the session owner's audit partition.
pub fn find_latest_approval_decision_for_user_run(
    user_id: &str,
    session_id: &str,
    request_id: &str,
    run_id: &str,
) -> std::io::Result<Option<ApprovalJournalDecision>> {
    let events = read_journal_for_user(user_id, session_id)?;
    Ok(find_latest_approval_decision_in_events(
        events.iter(),
        request_id,
        Some(run_id),
    ))
}

/// Read only approval events appended after `cursor`, returning the next
/// durable generation-aware cursor. A new file generation is scanned from
/// zero, while an unchanged generation is a metadata-only fast path.
pub fn find_latest_approval_decision_for_user_run_after(
    user_id: &str,
    session_id: &str,
    request_id: &str,
    run_id: &str,
    cursor: Option<&ApprovalJournalCursor>,
) -> std::io::Result<(Option<ApprovalJournalDecision>, ApprovalJournalCursor)> {
    let (events, next_cursor) =
        read_approval_journal_append_delta_for_user(user_id, session_id, cursor)?;
    let decision = find_latest_approval_decision_in_events(events.iter(), request_id, Some(run_id));
    Ok((decision, next_cursor))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecisionAppendOutcome {
    Appended,
    Idempotent,
    Conflict(ApprovalJournalDecision),
}

fn approval_decision_from_event(
    event: &JournalEvent,
    request_id: &str,
    run_id: Option<&str>,
) -> Option<ApprovalJournalDecision> {
    if event.event_type != JournalEventType::ApprovalDecision {
        return None;
    }
    let metadata = event.metadata.as_ref()?;
    let found_request_id = approval_metadata_str(metadata, "request_id")?;
    if found_request_id != request_id {
        return None;
    }
    let found_run_id = approval_metadata_str(metadata, "run_id");
    if let Some(expected_run_id) = run_id
        && found_run_id.as_deref() != Some(expected_run_id)
    {
        return None;
    }
    let decision = approval_metadata_str(metadata, "decision")?;
    Some(ApprovalJournalDecision {
        request_id: found_request_id,
        run_id: found_run_id,
        decision,
        reason: approval_metadata_str(metadata, "reason"),
        tool_name: approval_metadata_str(metadata, "tool_name"),
        approval_kind: approval_metadata_str(metadata, "approval_kind"),
    })
}

fn approval_decision_matches(
    existing: &ApprovalJournalDecision,
    decision: &str,
    reason: Option<&str>,
    tool_name: Option<&str>,
    approval_kind: Option<&str>,
) -> bool {
    existing.decision == decision
        && existing.reason.as_deref() == reason
        && existing.tool_name.as_deref() == tool_name
        && existing.approval_kind.as_deref() == approval_kind
}

#[allow(clippy::too_many_arguments)]
pub fn append_approval_decision_for_run_if_absent(
    session_id: &str,
    turn: Option<u32>,
    request_id: &str,
    run_id: &str,
    tool_name: Option<&str>,
    approval_kind: Option<&str>,
    decision: &str,
    reason: Option<&str>,
) -> std::io::Result<ApprovalDecisionAppendOutcome> {
    let path = journal_file_path(session_id);
    append_approval_decision_for_run_if_absent_at_path(
        &path,
        session_id,
        turn,
        request_id,
        run_id,
        tool_name,
        approval_kind,
        decision,
        reason,
    )
}

/// Atomically append an approval decision to one authenticated user's journal.
#[allow(clippy::too_many_arguments)]
pub fn append_approval_decision_for_user_run_if_absent(
    user_id: &str,
    session_id: &str,
    turn: Option<u32>,
    request_id: &str,
    run_id: &str,
    tool_name: Option<&str>,
    approval_kind: Option<&str>,
    decision: &str,
    reason: Option<&str>,
) -> std::io::Result<ApprovalDecisionAppendOutcome> {
    let path = journal_file_path_for_user(user_id, session_id)?;
    append_approval_decision_for_run_if_absent_at_path(
        &path,
        session_id,
        turn,
        request_id,
        run_id,
        tool_name,
        approval_kind,
        decision,
        reason,
    )
}

#[allow(clippy::too_many_arguments)]
fn append_approval_decision_for_run_if_absent_at_path(
    path: &Path,
    session_id: &str,
    turn: Option<u32>,
    request_id: &str,
    run_id: &str,
    tool_name: Option<&str>,
    approval_kind: Option<&str>,
    decision: &str,
    reason: Option<&str>,
) -> std::io::Result<ApprovalDecisionAppendOutcome> {
    use fs2::FileExt;
    use std::io::{Read, Seek, SeekFrom, Write};

    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    if run_id.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "run_id required",
        ));
    }

    const MAX_GENERATION_RETRIES: usize = 8;
    for _ in 0..MAX_GENERATION_RETRIES {
        let mut file = open_unlocked_journal_file(path)?;
        #[cfg(test)]
        run_approval_append_open_hook(path);
        file.lock_exclusive()?;
        // `flock` belongs to the inode, not the directory entry. An operator
        // may rotate the pathname while this writer waits for the old inode's
        // lock. Never inspect or acknowledge a detached generation.
        if !open_journal_file_is_current(&file, path)? {
            continue;
        }

        let mut content = String::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_string(&mut content)?;
        let events = parse_journal_text(&content).0;
        if let Some(existing) = events
            .iter()
            .rev()
            .find_map(|event| approval_decision_from_event(event, request_id, Some(run_id)))
        {
            if !open_journal_file_is_current(&file, path)? {
                continue;
            }
            return if approval_decision_matches(
                &existing,
                decision,
                reason,
                tool_name,
                approval_kind,
            ) {
                Ok(ApprovalDecisionAppendOutcome::Idempotent)
            } else {
                Ok(ApprovalDecisionAppendOutcome::Conflict(existing))
            };
        }

        let event = JournalEvent::approval_decision_for_run(
            Some(session_id),
            turn,
            request_id,
            Some(run_id),
            tool_name,
            approval_kind,
            decision,
            reason,
        );
        let events = prepend_session_start_if_needed(path, std::slice::from_ref(&event))?;
        let buf = serialize_journal_events(events.as_ref())?;
        file.write_all(&buf)?;
        file.sync_data()?;
        if !open_journal_file_is_current(&file, path)? {
            // The durable bytes landed in a rotated generation. Retry the
            // idempotent operation against the pathname that readers use.
            continue;
        }
        update_cached_session_start_state_from_events(path, events.as_ref());
        return Ok(ApprovalDecisionAppendOutcome::Appended);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!("journal {session_id} kept rotating while persisting approval receipt"),
    ))
}

#[cfg(unix)]
fn open_journal_file_is_current(file: &std::fs::File, path: &Path) -> std::io::Result<bool> {
    let open_identity = journal_file_identity(&file.metadata()?);
    let path_metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let path_identity = journal_file_identity(&path_metadata);
    Ok(open_identity.is_some() && open_identity == path_identity)
}

#[cfg(not(unix))]
fn open_journal_file_is_current(_file: &std::fs::File, path: &Path) -> std::io::Result<bool> {
    // Platforms without a stable file id cannot prove pathname continuity
    // from std metadata alone. Preserve existing behavior rather than making
    // every approval append fail; the directory-level generation protocol is
    // enforced on Unix deployments where server-side rotation is supported.
    Ok(path.exists())
}

fn find_latest_approval_decision_impl(
    session_id: &str,
    request_id: &str,
    run_id: Option<&str>,
) -> std::io::Result<Option<ApprovalJournalDecision>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let events = read_journal(session_id)?;
    Ok(find_latest_approval_decision_in_events(
        events.iter(),
        request_id,
        run_id,
    ))
}

fn find_latest_approval_decision_in_events<'a>(
    events: impl DoubleEndedIterator<Item = &'a JournalEvent>,
    request_id: &str,
    run_id: Option<&str>,
) -> Option<ApprovalJournalDecision> {
    events
        .rev()
        .find_map(|event| approval_decision_from_event(event, request_id, run_id))
}

pub fn find_latest_approval_required(
    session_id: &str,
    request_id: &str,
) -> std::io::Result<Option<ApprovalJournalRequest>> {
    find_latest_approval_required_impl(session_id, request_id, None)
}

pub fn find_latest_approval_required_for_run(
    session_id: &str,
    request_id: &str,
    run_id: &str,
) -> std::io::Result<Option<ApprovalJournalRequest>> {
    find_latest_approval_required_impl(session_id, request_id, Some(run_id))
}

fn find_latest_approval_required_impl(
    session_id: &str,
    request_id: &str,
    run_id: Option<&str>,
) -> std::io::Result<Option<ApprovalJournalRequest>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let events = read_journal(session_id)?;
    for event in events.into_iter().rev() {
        if event.event_type != JournalEventType::ApprovalRequired {
            continue;
        }
        let Some(metadata) = event.metadata.as_ref() else {
            continue;
        };
        let Some(found_request_id) = approval_metadata_str(metadata, "request_id") else {
            continue;
        };
        if found_request_id != request_id {
            continue;
        }
        let found_run_id = approval_metadata_str(metadata, "run_id");
        if let Some(expected_run_id) = run_id
            && found_run_id.as_deref() != Some(expected_run_id)
        {
            continue;
        }
        return Ok(Some(ApprovalJournalRequest {
            request_id: found_request_id,
            run_id: found_run_id,
            turn: event.turn,
            tool_name: approval_metadata_str(metadata, "tool_name"),
            approval_kind: approval_metadata_str(metadata, "approval_kind"),
        }));
    }
    Ok(None)
}

fn ask_user_metadata_str(metadata: &serde_json::Value, field: &str) -> Option<String> {
    metadata
        .get("ask_user")
        .and_then(|ask_user| ask_user.get(field))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
}

#[derive(Debug, Clone, PartialEq)]
pub struct AskUserJournalResponse {
    pub request_id: String,
    pub run_id: Option<String>,
    pub status: String,
    pub answers: Option<serde_json::Value>,
}

/// Canonical durable ask-user request.  Responses must be bound to one of
/// these records before they can affect a run; a bearer token alone does not
/// authorize a caller to invent a questionnaire request id.
#[derive(Debug, Clone, PartialEq)]
pub struct AskUserJournalRequest {
    pub request_id: String,
    pub run_id: Option<String>,
    pub turn: Option<u32>,
    pub prompt: serde_json::Value,
}

/// Result of atomically recording the terminal answer for an ask-user
/// request.  This mirrors approval decisions: retrying the exact callback is
/// safe, but a conflicting late answer never overwrites the first outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum AskUserResponseAppendOutcome {
    Appended,
    Idempotent,
    Conflict(AskUserJournalResponse),
}

impl AskUserJournalResponse {
    pub fn interaction_contract(
        &self,
        session_id: &str,
        user_id: Option<&str>,
    ) -> Option<InteractionContract> {
        let run_id = self.run_id.as_deref()?;
        let identity =
            InteractionIdentity::new(user_id, session_id, run_id, self.request_id.as_str());
        if !identity.is_run_scoped() {
            return None;
        }
        Some(InteractionContract::new(
            InteractionKind::UserPrompt,
            identity,
            ask_user_response_status(&self.status),
            Some("session_journal.ask_user_response".to_string()),
        ))
    }
}

pub fn find_latest_ask_user_response(
    session_id: &str,
    request_id: &str,
) -> std::io::Result<Option<AskUserJournalResponse>> {
    find_latest_ask_user_response_impl(session_id, request_id, None)
}

pub fn find_latest_ask_user_response_for_run(
    session_id: &str,
    request_id: &str,
    run_id: &str,
) -> std::io::Result<Option<AskUserJournalResponse>> {
    find_latest_ask_user_response_impl(session_id, request_id, Some(run_id))
}

pub fn find_latest_ask_user_prompted_for_run(
    session_id: &str,
    request_id: &str,
    run_id: &str,
) -> std::io::Result<Option<AskUserJournalRequest>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let events = read_journal(session_id)?;
    for event in events.into_iter().rev() {
        if event.event_type != JournalEventType::AskUserPrompted {
            continue;
        }
        let Some(metadata) = event.metadata.as_ref() else {
            continue;
        };
        let Some(found_request_id) = ask_user_metadata_str(metadata, "request_id") else {
            continue;
        };
        if found_request_id != request_id {
            continue;
        }
        let found_run_id = ask_user_metadata_str(metadata, "run_id");
        if found_run_id.as_deref() != Some(run_id) {
            continue;
        }
        let Some(prompt) = metadata
            .get("ask_user")
            .and_then(|ask_user| ask_user.get("prompt"))
            .cloned()
        else {
            continue;
        };
        return Ok(Some(AskUserJournalRequest {
            request_id: found_request_id,
            run_id: found_run_id,
            turn: event.turn,
            prompt,
        }));
    }
    Ok(None)
}

fn ask_user_response_from_event(
    event: &JournalEvent,
    request_id: &str,
    run_id: &str,
) -> Option<AskUserJournalResponse> {
    if event.event_type != JournalEventType::AskUserResponse {
        return None;
    }
    let metadata = event.metadata.as_ref()?;
    let found_request_id = ask_user_metadata_str(metadata, "request_id")?;
    if found_request_id != request_id {
        return None;
    }
    let found_run_id = ask_user_metadata_str(metadata, "run_id");
    if found_run_id.as_deref() != Some(run_id) {
        return None;
    }
    let status = ask_user_metadata_str(metadata, "status")?;
    let answers = metadata
        .get("ask_user")
        .and_then(|ask_user| ask_user.get("answers"))
        .filter(|answers| !answers.is_null())
        .cloned();
    Some(AskUserJournalResponse {
        request_id: found_request_id,
        run_id: found_run_id,
        status,
        answers,
    })
}

/// Append a terminal ask-user response while holding the journal lock.
///
/// This is intentionally not `find + JournalWriter::append`: responses may
/// be retried across pods or race the timeout closer, and that split protocol
/// would leave two contradictory answers in durable history.
pub fn append_ask_user_response_for_run_if_absent(
    session_id: &str,
    turn: Option<u32>,
    request_id: &str,
    run_id: &str,
    status: &str,
    answers: Option<serde_json::Value>,
) -> std::io::Result<AskUserResponseAppendOutcome> {
    use std::io::{Read, Seek, SeekFrom, Write};

    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    if run_id.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "run_id required",
        ));
    }

    let path = journal_file_path(session_id);
    let mut file = open_locked_journal_file(&path)?;
    let mut content = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut content)?;
    let events = parse_journal_text(&content).0;
    for event in events.iter().rev() {
        let Some(existing) = ask_user_response_from_event(event, request_id, run_id) else {
            continue;
        };
        if ask_user_response_status(&existing.status) == InteractionStatus::Pending {
            continue;
        }
        return if existing.status == status && existing.answers == answers {
            Ok(AskUserResponseAppendOutcome::Idempotent)
        } else {
            Ok(AskUserResponseAppendOutcome::Conflict(existing))
        };
    }

    let event = JournalEvent::ask_user_response(
        Some(session_id),
        turn,
        request_id,
        Some(run_id),
        status,
        answers,
    );
    let events = prepend_session_start_if_needed(&path, std::slice::from_ref(&event))?;
    let buf = serialize_journal_events(events.as_ref())?;
    file.write_all(&buf)?;
    file.sync_data()?;
    update_cached_session_start_state_from_events(&path, events.as_ref());
    Ok(AskUserResponseAppendOutcome::Appended)
}

fn find_latest_ask_user_response_impl(
    session_id: &str,
    request_id: &str,
    run_id: Option<&str>,
) -> std::io::Result<Option<AskUserJournalResponse>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let events = read_journal(session_id)?;
    for event in events.into_iter().rev() {
        if event.event_type != JournalEventType::AskUserResponse {
            continue;
        }
        let Some(metadata) = event.metadata.as_ref() else {
            continue;
        };
        let Some(found_request_id) = ask_user_metadata_str(metadata, "request_id") else {
            continue;
        };
        if found_request_id != request_id {
            continue;
        }
        let found_run_id = ask_user_metadata_str(metadata, "run_id");
        if let Some(expected_run_id) = run_id
            && found_run_id.as_deref() != Some(expected_run_id)
        {
            continue;
        }
        let Some(status) = ask_user_metadata_str(metadata, "status") else {
            continue;
        };
        let answers = metadata
            .get("ask_user")
            .and_then(|ask_user| ask_user.get("answers"))
            .filter(|answers| !answers.is_null())
            .cloned();
        return Ok(Some(AskUserJournalResponse {
            request_id: found_request_id,
            run_id: found_run_id,
            status,
            answers,
        }));
    }
    Ok(None)
}

/// Read journal for offline analysis tools. Returns an error if the JSONL file is missing.
///
/// Second element: non-empty physical lines; third: lines that failed JSON parse.
pub fn read_journal_for_digest(
    session_id: &str,
) -> std::io::Result<(Vec<JournalEvent>, usize, usize)> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    read_journal_digest_from_path(&journal_file_path(session_id))
}

pub fn read_journal_for_digest_for_user(
    user_id: &str,
    session_id: &str,
) -> std::io::Result<(Vec<JournalEvent>, usize, usize)> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    read_journal_digest_from_path(&journal_file_path_for_user(user_id, session_id)?)
}

fn read_journal_digest_from_path(
    path: &Path,
) -> std::io::Result<(Vec<JournalEvent>, usize, usize)> {
    if !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session journal not found: {}", path.display()),
        ));
    }
    let content = std::fs::read_to_string(path)?;
    let parsed = parse_journal_text(&content);
    record_journal_read(
        astra_core::history_work::HistoryWorkSite::SessionJournalDigestRead,
        content.len(),
        parsed.0.len(),
    );
    Ok(parsed)
}

/// List all session IDs that have journal files.
pub fn list_sessions() -> std::io::Result<Vec<String>> {
    list_sessions_for_owner(&OwnerScope::local_user())
}

pub fn list_sessions_for_user(user_id: &str) -> std::io::Result<Vec<String>> {
    let owner_scope = OwnerScope::user(user_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    list_sessions_for_owner(&owner_scope)
}

pub fn list_sessions_for_owner(owner_scope: &OwnerScope) -> std::io::Result<Vec<String>> {
    let dir = journal_dir_for_owner(owner_scope)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(sid) = name.strip_suffix(".jsonl") {
            sessions.push(sid.to_string());
        }
    }
    sessions.sort();
    Ok(sessions)
}

#[must_use]
pub fn local_owner_sessions_dir() -> PathBuf {
    journal_dir()
}

/// Path to the JSONL journal file for a session.
///
/// # Panics
/// Panics if `session_id` contains path traversal characters. Use [`validate_session_id`]
/// to pre-validate untrusted input.
#[must_use]
pub fn journal_file_path(session_id: &str) -> PathBuf {
    assert!(
        validate_session_id(session_id).is_ok(),
        "unsafe session ID passed to journal_file_path: {session_id}"
    );
    crate::local_session_artifact_store()
        .journal_path(session_id)
        .expect("validated session_id must resolve journal path")
}

pub fn journal_file_path_for_owner(
    owner_scope: &OwnerScope,
    session_id: &str,
) -> std::io::Result<PathBuf> {
    crate::local_session_artifact_store()
        .journal_path_for_owner(owner_scope, session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

pub fn journal_file_path_for_user(user_id: &str, session_id: &str) -> std::io::Result<PathBuf> {
    let owner_scope = OwnerScope::user(user_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    journal_file_path_for_owner(&owner_scope, session_id)
}

/// List local session IDs sorted by file modification time (most recent first).
/// Only returns the `limit` most recent sessions to avoid scanning all files.
pub fn list_sessions_by_time(limit: usize) -> std::io::Result<Vec<String>> {
    list_sessions_by_time_for_owner(&OwnerScope::local_user(), limit)
}

pub fn list_sessions_by_time_for_user(user_id: &str, limit: usize) -> std::io::Result<Vec<String>> {
    let owner_scope = OwnerScope::user(user_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    list_sessions_by_time_for_owner(&owner_scope, limit)
}

pub fn list_sessions_by_time_for_owner(
    owner_scope: &OwnerScope,
    limit: usize,
) -> std::io::Result<Vec<String>> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let dir = journal_dir_for_owner(owner_scope)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // Min-heap of (mtime, sid) — keeps only the `limit` newest entries
    let mut heap: BinaryHeap<Reverse<(std::time::SystemTime, String)>> = BinaryHeap::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(sid) = name.strip_suffix(".jsonl") {
            // Skip test-generated sessions
            if sid.starts_with("test-") || sid.starts_with("new-sess-") {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if heap.len() < limit {
                heap.push(Reverse((mtime, sid.to_string())));
            } else if let Some(&Reverse((min_time, _))) = heap.peek()
                && mtime > min_time
            {
                heap.pop();
                heap.push(Reverse((mtime, sid.to_string())));
            }
        }
    }
    let mut items: Vec<_> = heap.into_iter().map(|Reverse(item)| item).collect();
    items.sort_by_key(|b| std::cmp::Reverse(b.0)); // newest first by mtime
    Ok(items.into_iter().map(|(_, sid)| sid).collect())
}

/// Count turn events in a journal without fully parsing all events.
pub fn count_turns(session_id: &str) -> u32 {
    if validate_session_id(session_id).is_err() {
        return 0;
    }
    use std::io::BufRead;
    let path = journal_file_path(session_id);
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(|l| l.ok())
        .filter(|l| l.contains("\"type\":\"turn\""))
        .count() as u32
}

/// Quick metadata peek from a journal file — reads only the first few lines.
///
/// Returns `(first_user_input, model, timestamp)` without parsing the entire JSONL.
/// Designed for fast session listing via partial journal reads.
pub fn peek_session_meta(session_id: &str) -> Option<SessionPeek> {
    validate_session_id(session_id).ok()?;
    use std::io::BufRead;
    let path = journal_file_path(session_id);
    let file = std::fs::File::open(&path).ok()?;
    let reader = std::io::BufReader::new(file);

    let mut model: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut created_at: Option<String> = None;

    // Read at most 20 lines — enough to find session_start + first turn
    for line in reader.lines().take(20).map_while(|l| l.ok()) {
        if created_at.is_none() {
            // Extract timestamp from first line (any event type)
            if let Some(ts) = extract_json_str(&line, "\"ts\":\"") {
                created_at = Some(ts);
            }
        }
        if model.is_none() && line.contains("\"type\":\"session_start\"") {
            model = extract_json_str(&line, "\"model\":\"");
        }
        if first_prompt.is_none() && line.contains("\"type\":\"turn\"") {
            first_prompt = extract_json_str(&line, "\"user_input\":\"");
        }
        if model.is_some() && first_prompt.is_some() {
            break;
        }
    }

    Some(SessionPeek {
        first_prompt,
        model,
        created_at,
    })
}

/// Lightweight session metadata from journal head.
#[derive(Debug, Clone, Default)]
pub struct SessionPeek {
    /// First user message (truncated, from first Turn event).
    pub first_prompt: Option<String>,
    /// Model from SessionStart event.
    pub model: Option<String>,
    /// Timestamp of first event.
    pub created_at: Option<String>,
}

const RECOVERY_TAIL_LINE_LIMIT: usize = 32;
const RECOVERY_TAIL_CHUNK_BYTES: usize = 4096;
const RECOVERY_TAIL_MAX_BYTES: usize = 64 * 1024;

/// Lightweight terminal state for crash-recovery decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEndState {
    /// The last recoverability marker in the latest session segment was `session_end`.
    Completed,
    /// The session stopped with a structured interruption record.
    Interrupted { kind: String, resumable: bool },
    /// The session had activity after the latest `session_start` but never ended cleanly.
    Zombie,
}

impl SessionEndState {}

#[derive(Debug, Deserialize)]
struct JournalTailEntry {
    #[serde(rename = "type")]
    event_type: JournalEventType,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

fn read_journal_tail_lines(path: &Path, max_lines: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek};

    if max_lines == 0 {
        return Ok(Vec::new());
    }

    let mut file = std::fs::File::open(path)?;
    let mut pos = file.seek(std::io::SeekFrom::End(0))?;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut bytes_read = 0usize;
    let mut newline_count = 0usize;

    while pos > 0 && newline_count <= max_lines && bytes_read < RECOVERY_TAIL_MAX_BYTES {
        let read_len = usize::min(RECOVERY_TAIL_CHUNK_BYTES, pos as usize);
        pos -= read_len as u64;
        file.seek(std::io::SeekFrom::Start(pos))?;
        let mut chunk = vec![0; read_len];
        file.read_exact(&mut chunk)?;
        newline_count += chunk.iter().filter(|&&b| b == b'\n').count();
        bytes_read += read_len;
        chunks.push(chunk);
    }

    chunks.reverse();
    let mut bytes = Vec::with_capacity(bytes_read);
    for chunk in chunks {
        bytes.extend_from_slice(&chunk);
    }

    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(max_lines)
        .map(ToString::to_string)
        .collect();
    lines.reverse();
    record_journal_read(
        astra_core::history_work::HistoryWorkSite::SessionJournalTailRead,
        bytes.len(),
        lines.len(),
    );
    Ok(lines)
}

/// Read an exact logical tail without scanning from the beginning of the
/// journal. Unlike recovery's defensive tail reader, this public API promises
/// up to `max_lines` events and therefore cannot silently apply the recovery
/// byte cap.
fn read_journal_tail_lines_exact(path: &Path, max_lines: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek};

    if max_lines == 0 {
        return Ok(Vec::new());
    }

    let mut file = std::fs::File::open(path)?;
    let mut pos = file.seek(std::io::SeekFrom::End(0))?;
    let mut chunks = Vec::new();
    let mut newline_count = 0usize;
    while pos > 0 && newline_count <= max_lines {
        let read_len = usize::min(RECOVERY_TAIL_CHUNK_BYTES, pos as usize);
        pos -= read_len as u64;
        file.seek(std::io::SeekFrom::Start(pos))?;
        let mut chunk = vec![0; read_len];
        file.read_exact(&mut chunk)?;
        newline_count += chunk.iter().filter(|&&byte| byte == b'\n').count();
        chunks.push(chunk);
    }

    chunks.reverse();
    let bytes = chunks.into_iter().flatten().collect::<Vec<_>>();
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(max_lines)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    lines.reverse();
    record_journal_read(
        astra_core::history_work::HistoryWorkSite::SessionJournalTailRead,
        bytes.len(),
        lines.len(),
    );
    Ok(lines)
}

fn parse_journal_tail_entry(line: &str) -> Option<JournalTailEntry> {
    serde_json::from_str::<JournalTailEntry>(line).ok()
}

fn interruption_kind(entry: &JournalTailEntry) -> String {
    entry
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("interruption"))
        .and_then(|value| value.get("kind"))
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string()
}

fn interruption_is_resumable(entry: &JournalTailEntry) -> bool {
    let interruption = entry
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("interruption"));

    if let Some(resumable) = interruption
        .and_then(|value| value.get("resumable"))
        .and_then(|value| value.as_bool())
    {
        return resumable;
    }

    match interruption.and_then(|value| value.get("resume_action")) {
        Some(serde_json::Value::String(action)) => !matches!(
            action.as_str(),
            "start_new_session" | "requires_intervention"
        ),
        Some(serde_json::Value::Object(action)) => {
            action.contains_key("continue_immediately")
                || action.contains_key("wait_and_retry")
                || action.contains_key("compact_and_retry")
        }
        _ => true,
    }
}

fn is_recovery_activity_event(event_type: &JournalEventType) -> bool {
    !matches!(
        event_type,
        JournalEventType::SessionStart
            | JournalEventType::SessionEnd
            | JournalEventType::ConfigChange
            | JournalEventType::SyncMarker
            | JournalEventType::ContextAssemblyRecorded
            | JournalEventType::AdaptiveScenarioApplied
            | JournalEventType::AdaptivePerTurnApplied
            | JournalEventType::CompactionRetry
    )
}

/// Terminal plan-lifecycle actions — a `plan_lifecycle` event with one of
/// these `action` values marks the plan as no longer mid-flight.
const PLAN_TERMINAL_ACTIONS: &[&str] = &[
    "plan_completed",
    "plan_abandoned",
    "plan_failed",
    "plan_deleted",
    "plan_rejected",
];

/// Return `Some(plan_id)` if the tail contains a `plan_lifecycle`
/// `execution_started` event that has no later terminal-action event for the
/// same plan_id. That means the plan is still mid-flight and the session must
/// not be classified as cleanly `Completed`.
fn mid_flight_plan_id(tail_lines: &[String]) -> Option<String> {
    // Walk chronologically (oldest → newest). Track the most recent started
    // plan_id; clear it whenever a matching terminal event fires.
    let mut active: Option<String> = None;
    for line in tail_lines {
        let Some(entry) = parse_journal_tail_entry(line) else {
            continue;
        };
        if !matches!(entry.event_type, JournalEventType::PlanLifecycle) {
            continue;
        }
        let Some(meta) = entry.metadata.as_ref() else {
            continue;
        };
        let action = meta
            .get("summary")
            .and_then(|v| v.as_str())
            .or_else(|| meta.get("action").and_then(|v| v.as_str()))
            .unwrap_or("");
        let plan_id = meta
            .get("detail")
            .and_then(|d| d.get("plan_id"))
            .and_then(|v| v.as_str())
            .or_else(|| meta.get("plan_id").and_then(|v| v.as_str()))
            .map(str::to_string);
        match action {
            "execution_started" => {
                active = plan_id;
            }
            a if PLAN_TERMINAL_ACTIONS.contains(&a) => {
                if active.as_deref() == plan_id.as_deref() {
                    active = None;
                } else if active.is_some() && plan_id.is_none() {
                    // Terminal event without a plan_id still clears the most
                    // recent active plan — errs on the side of "not stale".
                    active = None;
                }
            }
            _ => {}
        }
    }
    active
}

/// Classify the latest session segment as completed, interrupted, or zombie.
///
/// Uses a bounded reverse tail read instead of loading the full JSONL file.
pub fn classify_session_end_state(session_id: &str) -> std::io::Result<SessionEndState> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let path = journal_file_path(session_id);
    if !path.exists() {
        return Ok(SessionEndState::Completed);
    }

    let tail_lines = read_journal_tail_lines(&path, RECOVERY_TAIL_LINE_LIMIT)?;
    let mut saw_activity_after_start = false;

    // Gate "Completed" results on plan state: if a plan is still mid-flight
    // (execution_started with no matching terminal event), treat the session
    // as Interrupted(plan_mid_flight) so find_stale_sessions / the reaper
    // notices. Computed up-front so the early returns below can consult it.
    let mid_flight = mid_flight_plan_id(&tail_lines);
    let completed_or_mid_flight = || {
        if mid_flight.is_some() {
            SessionEndState::Interrupted {
                kind: "plan_mid_flight".to_string(),
                resumable: true,
            }
        } else {
            SessionEndState::Completed
        }
    };

    for line in tail_lines.iter().rev() {
        let Some(entry) = parse_journal_tail_entry(line) else {
            continue;
        };
        match entry.event_type {
            JournalEventType::SessionEnd => return Ok(completed_or_mid_flight()),
            JournalEventType::InterruptionRecorded => {
                return Ok(SessionEndState::Interrupted {
                    kind: interruption_kind(&entry),
                    resumable: interruption_is_resumable(&entry),
                });
            }
            JournalEventType::SessionStart => {
                return Ok(if saw_activity_after_start {
                    SessionEndState::Zombie
                } else {
                    completed_or_mid_flight()
                });
            }
            _ if is_recovery_activity_event(&entry.event_type) => {
                saw_activity_after_start = true;
            }
            _ => {}
        }
    }

    Ok(if saw_activity_after_start {
        SessionEndState::Zombie
    } else {
        completed_or_mid_flight()
    })
}

/// Fast JSON string field extraction without full parse.
/// Looks for `"key":"value"` and returns the value (handles simple escapes).
fn extract_json_str(line: &str, needle: &str) -> Option<String> {
    let start = line.find(needle)? + needle.len();
    let rest = &line[start..];
    // Find closing quote, handling escaped quotes
    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'"' && (end == 0 || bytes[end - 1] != b'\\') {
            break;
        }
        end += 1;
    }
    if end == 0 || end >= bytes.len() {
        return None;
    }
    Some(rest[..end].replace("\\\"", "\"").replace("\\n", " "))
}

// ── Session listing with metadata ────────────────────────────────────────────

// ── Session cleanup / lifecycle ──────────────────────────────────────────────

/// Metadata about a session that's a candidate for cleanup.
#[derive(Debug, Clone)]
pub struct StaleSessionInfo {
    pub session_id: String,
    /// File modification time of the journal.
    pub last_modified: std::time::SystemTime,
    /// Journal file size in bytes.
    pub journal_bytes: u64,
    /// Turn count (fast count, not full parse).
    pub turns: u32,
    /// Total disk usage: journal + workspace dir (recursive).
    pub total_bytes: u64,
}

/// Find sessions whose journal file hasn't been modified in `max_age`.
///
/// `exclude_id` — the currently active session (never returned).
pub fn find_stale_sessions(
    max_age: std::time::Duration,
    exclude_id: Option<&str>,
) -> std::io::Result<Vec<StaleSessionInfo>> {
    let dir = journal_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let mut stale = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(sid) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if sid.starts_with("test-") || sid.starts_with("new-sess-") {
            continue;
        }
        if exclude_id == Some(sid) {
            continue;
        }
        let meta = entry.metadata()?;
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if mtime >= cutoff {
            continue; // still fresh
        }
        let journal_bytes = meta.len();
        let turns = count_turns(sid);
        let ws_dir = crate::session_workspace::workspace_dir_for(sid);
        let ws_bytes = dir_size_recursive(&ws_dir);
        stale.push(StaleSessionInfo {
            session_id: sid.to_string(),
            last_modified: mtime,
            journal_bytes,
            turns,
            total_bytes: journal_bytes + ws_bytes,
        });
    }
    // Sort oldest first
    stale.sort_by_key(|s| s.last_modified);
    Ok(stale)
}

/// Delete a session's journal file and workspace directory.
///
/// Returns `Ok(bytes_freed)` on success.
pub fn delete_session(session_id: &str) -> std::io::Result<u64> {
    delete_session_for_owner(&OwnerScope::local_user(), session_id)
}

pub fn delete_session_for_user(user_id: &str, session_id: &str) -> std::io::Result<u64> {
    let owner_scope = OwnerScope::user(user_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    delete_session_for_owner(&owner_scope, session_id)
}

pub fn delete_session_for_owner(
    owner_scope: &OwnerScope,
    session_id: &str,
) -> std::io::Result<u64> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let journal = journal_file_path_for_owner(owner_scope, session_id)?;
    let session_dir = crate::local_session_artifact_store()
        .session_dir_for_owner(owner_scope, session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut freed = 0u64;
    if journal.exists() {
        freed += std::fs::metadata(&journal).map(|m| m.len()).unwrap_or(0);
        std::fs::remove_file(&journal)?;
    }
    if session_dir.exists() {
        freed += dir_size_recursive(&session_dir);
        std::fs::remove_dir_all(&session_dir)?;
    }
    Ok(freed)
}

/// Recursively compute total size of a directory (best-effort, ignores errors).
///
/// Safeguards:
/// - Maximum depth of 10 levels to prevent deep traversal
/// - Maximum 1000 entries per call to prevent hangs on huge directories
fn dir_size_recursive(path: &std::path::Path) -> u64 {
    if !path.is_dir() {
        return 0;
    }
    walkdir_bounded(path, 0)
}

/// Max depth for recursive directory traversal (10 levels should cover most workspaces).
const MAX_WALKDIR_DEPTH: u32 = 10;
/// Max entries to process per directory (prevents hangs on huge flat directories).
const MAX_ENTRIES_PER_DIR: usize = 1000;

fn walkdir_bounded(path: &std::path::Path, depth: u32) -> u64 {
    if depth > MAX_WALKDIR_DEPTH {
        return 0; // Stop at max depth
    }
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.take(MAX_ENTRIES_PER_DIR).flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                total += walkdir_bounded(&entry.path(), depth + 1);
            }
        }
    }
    total
}

/// Compress a session's `.jsonl` journal to `.jsonl.gz` and remove the original.
///
/// Returns `Ok((original_bytes, compressed_bytes))` on success.
/// Only archives if the session has a `session_end` event (i.e., completed).
pub fn archive_journal(session_id: &str) -> std::io::Result<(u64, u64)> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let src = journal_file_path(session_id);
    if !src.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("journal file not found for {session_id}"),
        ));
    }
    // Check the journal has a session_end (don't archive active sessions)
    let content = std::fs::read(&src)?;
    if !content
        .windows(b"\"session_end\"".len())
        .any(|w| w == b"\"session_end\"")
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session has no session_end event — still active?",
        ));
    }
    let original_bytes = content.len() as u64;
    let dst = src.with_extension("jsonl.gz");
    let out_file = std::fs::File::create(&dst)?;
    let mut encoder = GzEncoder::new(out_file, Compression::default());
    encoder.write_all(&content)?;
    let out_file = encoder.finish()?;
    // Ensure compressed data is durable before deleting the original.
    out_file.sync_all()?;
    let compressed_bytes = std::fs::metadata(&dst)?.len();
    std::fs::remove_file(&src)?;
    Ok((original_bytes, compressed_bytes))
}

/// Find completed sessions eligible for archival (have session_end, not yet compressed).
///
/// `exclude_id` — the currently active session.
pub fn find_archivable_sessions(exclude_id: Option<&str>) -> std::io::Result<Vec<(String, u64)>> {
    let dir = journal_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(sid) = name.strip_suffix(".jsonl") else {
            continue;
        };
        // Skip already-compressed (.jsonl.gz would not match .jsonl suffix)
        if sid.ends_with(".jsonl") {
            continue; // double extension guard
        }
        if sid.starts_with("test-") || sid.starts_with("new-sess-") {
            continue;
        }
        if exclude_id == Some(sid) {
            continue;
        }
        let meta = entry.metadata()?;
        let bytes = meta.len();
        // Quick check: has session_end?
        let path = entry.path();
        let has_end = std::fs::read_to_string(&path)
            .map(|c| c.contains("\"session_end\""))
            .unwrap_or(false);
        if has_end {
            result.push((sid.to_string(), bytes));
        }
    }
    result.sort_by_key(|b| std::cmp::Reverse(b.1)); // largest first
    Ok(result)
}

/// Resolve a session id to an exact journal filename stem.
///
/// Accepts:
/// - a full session id
/// - a unique prefix of a session id
/// - an id with a trailing `.jsonl`
pub fn resolve_session_id(query: &str) -> std::io::Result<String> {
    let query = query.trim();
    let query = query.strip_suffix(".jsonl").unwrap_or(query);
    if query.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session id cannot be empty",
        ));
    }
    let sessions = list_sessions()?;
    resolve_session_id_from_list(query, &sessions)
}

fn journal_dir_for_owner(owner_scope: &OwnerScope) -> std::io::Result<PathBuf> {
    crate::local_session_artifact_store()
        .owner_sessions_root(owner_scope)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

/// Helper: get the current owner's journal directory under [`local_sessions_dir()`].
fn journal_dir() -> PathBuf {
    journal_dir_for_owner(&OwnerScope::local_user())
        .expect("local owner user id must resolve journal dir")
}

fn resolve_session_id_from_list(query: &str, sessions: &[String]) -> std::io::Result<String> {
    if let Some(exact) = sessions.iter().find(|sid| sid.as_str() == query) {
        return Ok(exact.clone());
    }

    let matches: Vec<String> = sessions
        .iter()
        .filter(|sid| sid.starts_with(query))
        .cloned()
        .collect();

    match matches.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no session journal matches '{query}'"),
        )),
        _ => {
            let preview = matches
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let extra = if matches.len() > 5 {
                format!(" (+{} more)", matches.len() - 5)
            } else {
                String::new()
            };
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("session id prefix '{query}' is ambiguous: {preview}{extra}"),
            ))
        }
    }
}

// ── Builder helpers for common events ───────────────────────────────

impl JournalEvent {
    /// Attach the server-owned producer identity to an event emitted while a
    /// run is executing.  Pipeline telemetry is created by a few specialised
    /// builders, but it must obey the same run-scoped evidence contract as
    /// `llm_round` and tool events.
    pub fn with_producer_scope(mut self, run_id: Option<&str>) -> Self {
        let Some(run_id) = run_id.map(str::trim).filter(|run_id| !run_id.is_empty()) else {
            return self;
        };
        self.producer_scope = Some(JournalProducerScope {
            run_id: run_id.to_string(),
            parent_run_id: None,
            agent_id: None,
            local_turn: None,
        });
        self
    }

    fn base(event_type: JournalEventType, session_id: Option<&str>) -> Self {
        Self {
            event_type,
            ts: chrono::Utc::now().to_rfc3339(),
            session_id: session_id.map(|s| s.to_string()),
            producer_scope: None,
            turn: None,
            agentic_step: None,
            model: None,
            user_input: None,
            assistant_output: None,
            tool_count: None,
            tokens_in: None,
            tokens_out: None,
            duration_ms: None,
            error: None,
            config_key: None,
            config_value: None,
            turns_compacted: None,
            facts_stored: None,
            visible_tools: None,
            selected_skills: None,
            tools_used: None,
            tool_calls: None,
            tool_outcomes: None,
            budget_used: None,
            budget_pressure: None,
            stall_type: None,
            metadata: None,
            plan_subtask_id: None,
            ttft_ms: None,
            context_ms: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            memoria_ms: None,
            session_lineage: None,
            coordination: None,
            transcript_item: None,
            conversation_commit: None,
            edge_policy: None,
            context_assembly_trace: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            round: None,
            tool_calls_returned: None,
            offset_ms: None,
            llm_rounds: None,
            total_llm_ms: None,
            total_tool_ms: None,
            parent_event_id: None,
            git_head: None,
            git_branch: None,
        }
    }

    /// Create a minimal event with just event type and session ID.
    /// Public variant of `base()` for use by external crates.
    pub fn base_public(event_type: JournalEventType, session_id: Option<&str>) -> Self {
        Self::base(event_type, session_id)
    }

    pub fn with_agentic_step(mut self, agentic_step: Option<u32>) -> Self {
        self.agentic_step = agentic_step;
        self
    }

    pub fn with_conversation_commit(mut self, commit: ConversationCommitV1) -> Self {
        // The journal redaction switch is a storage privacy boundary, not
        // merely formatting for the legacy display fields. Do not introduce a
        // second raw-content lane when it is enabled.
        if !journal_content_redact_enabled() {
            self.conversation_commit = Some(commit);
        }
        self
    }

    /// Set the parent event ID for causal lineage.
    pub fn with_parent_event_id(mut self, parent_event_id: Option<String>) -> Self {
        self.parent_event_id = parent_event_id;
        self
    }

    /// Attach git snapshot (HEAD commit + branch) to this event.
    pub fn with_git_snapshot(mut self, head: Option<String>, branch: Option<String>) -> Self {
        self.git_head = head;
        self.git_branch = branch;
        self
    }

    /// Session start event.
    pub fn session_start(session_id: Option<&str>, model: Option<&str>) -> Self {
        let mut evt = Self::base(JournalEventType::SessionStart, session_id);
        evt.model = model.map(|s| s.to_string());
        evt
    }

    /// Startup bootstrap event: records per-phase timestamps (microsecond precision).
    ///
    /// `phases` is an ordered list of `(phase_name, timestamp_us_since_process_start)` tuples.
    /// Stored in `metadata.phases` as `[{name, us}]` and `metadata.total_us`.
    pub fn bootstrap(session_id: Option<&str>, phases: &[(&str, u64)], total_us: u64) -> Self {
        let mut evt = Self::base(JournalEventType::Bootstrap, session_id);
        let phase_entries: Vec<serde_json::Value> = phases
            .iter()
            .map(|(name, us)| serde_json::json!({"name": name, "us": us}))
            .collect();
        evt.metadata = Some(serde_json::json!({
            "phases": phase_entries,
            "total_us": total_us,
        }));
        evt
    }

    /// Lightweight trace span for cross-boundary observability (edge ↔ cloud).
    ///
    /// `span_id` is a short unique identifier; `parent_span_id` links to the parent span.
    /// `name` is the span operation name (e.g. "context_assembly", "llm_call").
    /// Record a trace span within the journal (phase timing, tool exec, etc.).
    ///
    /// Use the [`TraceSpanBuilder`] to construct — the builder enforces
    /// required fields at compile time and avoids the 8-argument constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn trace_span(
        session_id: Option<&str>,
        turn: Option<u32>,
        span_id: &str,
        parent_span_id: Option<&str>,
        name: &str,
        start_us: u64,
        end_us: u64,
        attrs: Option<&HashMap<String, String>>,
        trace_id: Option<&str>,
    ) -> Self {
        TraceSpanBuilder::default()
            .session_id(session_id)
            .turn(turn)
            .span_id(span_id.to_string())
            .parent_span_id(parent_span_id.map(str::to_string))
            .name(name.to_string())
            .start_us(start_us)
            .end_us(end_us)
            .attrs(attrs)
            .trace_id(trace_id.map(str::to_string))
            .build()
    }

    /// Record a trace span via the builder.
    pub fn trace_span_v2(builder: TraceSpanBuilder) -> Self {
        builder.build()
    }
}

/// Builder for [`JournalEvent::trace_span`]. Enforces required fields at
/// compile time and avoids the 8-argument constructor.
///
/// Adds `trace_id` for cross-boundary (edge ↔ cloud) correlation.
#[derive(Debug, Default, Clone)]
pub struct TraceSpanBuilder {
    session_id: Option<String>,
    turn: Option<u32>,
    span_id: Option<String>,
    parent_span_id: Option<String>,
    name: Option<String>,
    start_us: Option<u64>,
    end_us: Option<u64>,
    attrs: Option<HashMap<String, String>>,
    /// Cross-boundary correlation id (edge ↔ cloud)
    trace_id: Option<String>,
}

impl TraceSpanBuilder {
    pub fn session_id(mut self, v: Option<&str>) -> Self {
        self.session_id = v.map(str::to_string);
        self
    }

    pub fn turn(mut self, v: Option<u32>) -> Self {
        self.turn = v;
        self
    }

    pub fn span_id(mut self, v: String) -> Self {
        self.span_id = Some(v);
        self
    }

    pub fn parent_span_id(mut self, v: Option<String>) -> Self {
        self.parent_span_id = v;
        self
    }

    pub fn name(mut self, v: String) -> Self {
        self.name = Some(v);
        self
    }

    pub fn start_us(mut self, v: u64) -> Self {
        self.start_us = Some(v);
        self
    }

    pub fn end_us(mut self, v: u64) -> Self {
        self.end_us = Some(v);
        self
    }

    pub fn attrs(mut self, v: Option<&HashMap<String, String>>) -> Self {
        self.attrs = v.cloned();
        self
    }

    pub fn trace_id(mut self, v: Option<String>) -> Self {
        self.trace_id = v;
        self
    }

    pub fn build(self) -> JournalEvent {
        let span_id = self.span_id.expect("TraceSpanBuilder: span_id is required");
        let name = self.name.expect("TraceSpanBuilder: name is required");
        let start_us = self
            .start_us
            .expect("TraceSpanBuilder: start_us is required");
        let end_us = self.end_us.expect("TraceSpanBuilder: end_us is required");
        let mut evt = JournalEvent::base(JournalEventType::TraceSpan, self.session_id.as_deref());
        evt.turn = self.turn;
        let mut meta = serde_json::json!({
            "span_id": &span_id,
            "name": &name,
            "start_us": start_us,
            "end_us": end_us,
            "duration_us": end_us.saturating_sub(start_us),
            // Keep the coarse duration at the stable top-level metadata key
            // used by the event-ingestion index. `duration_us` remains the
            // source of precision; this enables per-phase latency queries
            // without JSON-path scans or phase-specific extraction rules.
            "duration_ms": end_us.saturating_sub(start_us) / 1_000,
        });
        if let Some(ref pid) = self.parent_span_id {
            meta["parent_span_id"] = serde_json::Value::String(pid.clone());
        }
        if let Some(ref a) = self.attrs {
            let attrs_map: serde_json::Map<String, serde_json::Value> = a
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            meta["attrs"] = serde_json::Value::Object(attrs_map);
        }
        if let Some(ref tid) = self.trace_id {
            meta["trace_id"] = serde_json::Value::String(tid.clone());
        }
        evt.metadata = Some(meta);
        evt
    }
}

impl JournalEvent {
    /// Persist one canonical conversation item in its owning local session
    /// journal. Returns `None` for malformed messages without a role.
    pub fn transcript_item(
        session_id: &str,
        run_id: &str,
        agent_id: &str,
        item_seq: u64,
        message: &serde_json::Value,
    ) -> Option<Self> {
        let role = message.get("role")?.as_str()?.trim();
        if role.is_empty() || run_id.trim().is_empty() || agent_id.trim().is_empty() {
            return None;
        }

        let message = if journal_content_redact_enabled() {
            serde_json::json!({
                "role": role,
                "content": journal_content_marker(&message.to_string()),
            })
        } else {
            message.clone()
        };
        let mut event = Self::base(JournalEventType::TranscriptItem, Some(session_id));
        event.transcript_item = Some(JournalTranscriptItem {
            source_event_id: local_transcript_source_event_id(
                session_id, run_id, agent_id, item_seq,
            ),
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
            item_seq,
            message,
        });
        Some(event)
    }

    /// Persist bounded, typed non-conversational evidence inside a local
    /// child's canonical transcript lane. This uses the same run-local
    /// sequence as messages so replay preserves the order in which the agent
    /// encountered coordination and permission boundaries.
    pub fn transcript_evidence(
        session_id: &str,
        run_id: &str,
        agent_id: &str,
        item_seq: u64,
        evidence: &astra_turn_types::AgentTranscriptEvidence,
    ) -> Option<Self> {
        let message = serde_json::json!({
            "role": "event",
            "evidence": evidence,
        });
        Self::transcript_item(session_id, run_id, agent_id, item_seq, &message)
    }

    /// Record that this session was forked from `lineage.parent_session_id`.
    pub fn session_fork(
        session_id: Option<&str>,
        lineage: SessionLineage,
        label_note: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::SessionFork, session_id);
        evt.session_lineage = Some(lineage);
        if let Some(n) = label_note.filter(|s| !s.is_empty()) {
            evt.user_input = Some(truncate(n, 200));
        }
        evt
    }

    /// Cloud–edge sync or multi-agent coordination marker (policy version, correlation id, etc.).
    pub fn sync_marker(
        session_id: Option<&str>,
        policy: Option<EdgePolicySnapshot>,
        coordination: Option<CoordinationMeta>,
        note: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::SyncMarker, session_id);
        evt.edge_policy = policy;
        evt.coordination = coordination;
        if let Some(n) = note.filter(|s| !s.is_empty()) {
            evt.user_input = Some(truncate(n, 200));
        }
        evt
    }

    /// Permission audit event. The payload shape is owned by
    /// `astra-turn-core::permission_audit`; the journal stores it as
    /// structured metadata so offline session inspection can reconstruct the
    /// complete permission chain.
    pub fn permission_audit(
        session_id: Option<&str>,
        turn: Option<u32>,
        payload: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PermissionAudit, session_id);
        evt.turn = turn;
        evt.metadata = Some(payload);
        evt
    }

    pub fn approval_required(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        tool_name: &str,
        approval_kind: &str,
        detail: Option<&str>,
    ) -> Self {
        Self::approval_required_for_run(
            session_id,
            turn,
            request_id,
            None,
            tool_name,
            approval_kind,
            detail,
        )
    }

    pub fn approval_required_for_run(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        run_id: Option<&str>,
        tool_name: &str,
        approval_kind: &str,
        detail: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ApprovalRequired, session_id);
        evt.turn = turn;
        evt.user_input = Some(truncate(
            &format!("approval_required {tool_name} {request_id}"),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "approval": {
                "request_id": request_id,
                "run_id": run_id.filter(|s| !s.is_empty()),
                "tool_name": tool_name,
                "approval_kind": approval_kind,
                "detail": detail.filter(|s| !s.is_empty()),
            }
        }));
        evt
    }

    pub fn approval_decision(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        tool_name: Option<&str>,
        approval_kind: Option<&str>,
        decision: &str,
        reason: Option<&str>,
    ) -> Self {
        Self::approval_decision_for_run(
            session_id,
            turn,
            request_id,
            None,
            tool_name,
            approval_kind,
            decision,
            reason,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn approval_decision_for_run(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        run_id: Option<&str>,
        tool_name: Option<&str>,
        approval_kind: Option<&str>,
        decision: &str,
        reason: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ApprovalDecision, session_id);
        evt.turn = turn;
        let summary_tool = tool_name.filter(|s| !s.is_empty()).unwrap_or("unknown");
        evt.user_input = Some(truncate(
            &format!("approval_decision {summary_tool} {request_id} {decision}"),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "approval": {
                "request_id": request_id,
                "run_id": run_id.filter(|s| !s.is_empty()),
                "tool_name": tool_name.filter(|s| !s.is_empty()),
                "approval_kind": approval_kind.filter(|s| !s.is_empty()),
                "decision": decision,
                "reason": reason.filter(|s| !s.is_empty()),
            }
        }));
        evt
    }

    pub fn approval_timeout(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        tool_name: &str,
        approval_kind: &str,
    ) -> Self {
        Self::approval_timeout_for_run(session_id, turn, request_id, None, tool_name, approval_kind)
    }

    pub fn approval_timeout_for_run(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        run_id: Option<&str>,
        tool_name: &str,
        approval_kind: &str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ApprovalTimeout, session_id);
        evt.turn = turn;
        evt.error = Some(truncate(
            &format!("approval timeout for {tool_name} ({request_id})"),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "approval": {
                "request_id": request_id,
                "run_id": run_id.filter(|s| !s.is_empty()),
                "tool_name": tool_name,
                "approval_kind": approval_kind,
            }
        }));
        evt
    }

    pub fn ask_user_prompted(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        run_id: Option<&str>,
        prompt: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AskUserPrompted, session_id);
        evt.turn = turn;
        evt.user_input = Some(truncate(&format!("ask_user_prompted {request_id}"), 200));
        evt.metadata = Some(serde_json::json!({
            "ask_user": {
                "request_id": request_id,
                "run_id": run_id.filter(|s| !s.is_empty()),
                "prompt": prompt,
            }
        }));
        evt
    }

    pub fn ask_user_response(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        run_id: Option<&str>,
        status: &str,
        answers: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AskUserResponse, session_id);
        evt.turn = turn;
        evt.user_input = Some(truncate(
            &format!("ask_user_response {request_id} {status}"),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "ask_user": {
                "request_id": request_id,
                "run_id": run_id.filter(|s| !s.is_empty()),
                "status": status,
                "answers": answers,
            }
        }));
        evt
    }

    pub fn execution_boundary_opened(
        session_id: Option<&str>,
        turn: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        checkpoints: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ExecutionBoundaryOpened, session_id);
        evt.turn = Some(turn);
        let tx_label = transaction_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("-");
        evt.user_input = Some(truncate(
            &format!("execution_boundary_opened {boundary_kind} {tx_label}"),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "execution_boundary": {
                "kind": boundary_kind,
                "transaction_id": normalize_optional_str(transaction_id),
                "rollback_on_failure": true,
                "checkpoints": checkpoints,
            }
        }));
        evt
    }

    pub fn execution_boundary_committed(
        session_id: Option<&str>,
        turn: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        detail: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ExecutionBoundaryCommitted, session_id);
        evt.turn = Some(turn);
        let tx_label = transaction_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("-");
        evt.user_input = Some(truncate(
            &format!("execution_boundary_committed {boundary_kind} {tx_label}"),
            200,
        ));
        let mut boundary = serde_json::Map::from_iter([
            (
                "kind".to_string(),
                serde_json::Value::String(boundary_kind.to_string()),
            ),
            (
                "transaction_id".to_string(),
                normalize_optional_str(transaction_id)
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            ),
            (
                "rollback_on_failure".to_string(),
                serde_json::Value::Bool(true),
            ),
        ]);
        if let Some(detail) = detail {
            boundary.insert("detail".to_string(), detail);
        }
        evt.metadata = Some(serde_json::json!({
            "execution_boundary": serde_json::Value::Object(boundary),
        }));
        evt
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execution_boundary_aborted(
        session_id: Option<&str>,
        turn: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        reason: &str,
        trigger_tool_name: Option<&str>,
        trigger_request_id: Option<&str>,
        rollback: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ExecutionBoundaryAborted, session_id);
        evt.turn = Some(turn);
        let tx_label = transaction_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("-");
        evt.error = Some(truncate(
            &format!("execution boundary aborted: {boundary_kind} {tx_label}"),
            200,
        ));
        let mut boundary = serde_json::Map::from_iter([
            (
                "kind".to_string(),
                serde_json::Value::String(boundary_kind.to_string()),
            ),
            (
                "transaction_id".to_string(),
                normalize_optional_str(transaction_id)
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            ),
            (
                "rollback_on_failure".to_string(),
                serde_json::Value::Bool(true),
            ),
            (
                "reason".to_string(),
                serde_json::Value::String(truncate(reason, 500)),
            ),
        ]);
        if let Some(trigger_tool_name) = normalize_optional_str(trigger_tool_name) {
            boundary.insert(
                "trigger_tool_name".to_string(),
                serde_json::Value::String(trigger_tool_name),
            );
        }
        if let Some(trigger_request_id) = normalize_optional_str(trigger_request_id) {
            boundary.insert(
                "trigger_request_id".to_string(),
                serde_json::Value::String(trigger_request_id),
            );
        }
        if let Some(rollback) = rollback {
            boundary.insert("rollback".to_string(), rollback);
        }
        evt.metadata = Some(serde_json::json!({
            "execution_boundary": serde_json::Value::Object(boundary),
        }));
        evt
    }

    /// After a successful MatrixOne pull of preferences (startup or post-login audit).
    ///
    /// Structured fields live under `metadata.cloud_pull` for analytics; `user_input` holds a short
    /// human-readable summary for export and grep.
    pub fn cloud_pull_sync_marker(
        session_id: Option<&str>,
        profile: &str,
        source: &str,
        preference_keys_merged: &[String],
        reachable_empty_ack: bool,
    ) -> Self {
        let note = format!(
            "cloud_pull {source} profile={profile} prefs={}{}",
            preference_keys_merged.len(),
            if reachable_empty_ack {
                " empty_ack"
            } else {
                ""
            }
        );
        let mut evt = Self::sync_marker(session_id, None, None, Some(note.as_str()));
        evt.metadata = Some(serde_json::json!({
            "cloud_pull": {
                "profile": profile,
                "source": source,
                "preference_keys_merged": preference_keys_merged,
                "reachable_empty_ack": reachable_empty_ack,
            }
        }));
        evt
    }

    /// Turn completion event.
    #[allow(clippy::too_many_arguments)]
    pub fn turn(
        session_id: Option<&str>,
        turn: u32,
        model: Option<&str>,
        user_input: &str,
        assistant_output: &str,
        tool_count: u32,
        tokens_in: u64,
        tokens_out: u64,
        duration_ms: u64,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::Turn, session_id);
        evt.turn = Some(turn);
        evt.model = model.map(|s| s.to_string());
        if journal_content_redact_enabled() {
            evt.user_input = Some(journal_content_marker(user_input));
            evt.assistant_output = Some(journal_content_marker(assistant_output));
        } else {
            evt.user_input = Some(truncate(user_input, 500));
            evt.assistant_output = Some(truncate(assistant_output, 10000));
        }
        evt.tool_count = Some(tool_count);
        evt.tokens_in = Some(tokens_in);
        evt.tokens_out = Some(tokens_out);
        evt.duration_ms = Some(duration_ms);
        evt
    }

    /// Add cache token counts to a turn event (builder pattern).
    pub fn with_cache_tokens(mut self, cache_read: u64, cache_creation: u64) -> Self {
        if cache_read > 0 {
            self.cache_read_tokens = Some(cache_read);
        }
        if cache_creation > 0 {
            self.cache_creation_tokens = Some(cache_creation);
        }
        self
    }

    /// Attach the terminal run id to a journal event for attempt provenance.
    pub fn with_run_id(mut self, run_id: Option<&str>) -> Self {
        let Some(run_id) = run_id.filter(|value| !value.is_empty()) else {
            return self;
        };
        let metadata = self.metadata.get_or_insert_with(|| serde_json::json!({}));
        if !metadata.is_object() {
            *metadata = serde_json::json!({
                "previous_metadata": metadata.clone(),
            });
        }
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("run_id".into(), serde_json::json!(run_id));
        }
        self
    }

    /// Attach typed user intents that were applied while the run was active.
    /// The top-level `user_input` remains the human-readable turn input; this
    /// ledger preserves identity and delivery semantics for replay and audit.
    pub fn with_applied_user_intents<'a, I>(mut self, intents: I) -> Self
    where
        I: IntoIterator<
            Item = (
                &'a str,
                UserIntentDelivery,
                UserIntentStatus,
                usize,
                &'a str,
            ),
        >,
    {
        let redacted = journal_content_redact_enabled();
        let events = intents
            .into_iter()
            .filter_map(|(intent_id, delivery, status, event_index, content)| {
                let intent_id = intent_id.trim();
                let content = content.trim();
                if intent_id.is_empty() || content.is_empty() || status != UserIntentStatus::Applied
                {
                    return None;
                }
                let content = if redacted {
                    journal_content_marker(content)
                } else {
                    truncate(content, 500)
                };
                Some(serde_json::json!({
                    "intent_id": intent_id,
                    "delivery": delivery,
                    "status": status,
                    "event_index": event_index,
                    "content": content,
                }))
            })
            .collect::<Vec<_>>();
        if events.is_empty() {
            return self;
        }

        let metadata = self.metadata.get_or_insert_with(|| serde_json::json!({}));
        if !metadata.is_object() {
            *metadata = serde_json::json!({
                "previous_metadata": metadata.clone(),
            });
        }
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("user_intents".into(), serde_json::Value::Array(events));
        }
        self
    }

    /// Turn error event.
    pub fn turn_error(
        session_id: Option<&str>,
        turn: u32,
        model: Option<&str>,
        user_input: &str,
        error: &str,
        duration_ms: u64,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::TurnError, session_id);
        evt.turn = Some(turn);
        evt.model = model.map(|s| s.to_string());
        if journal_content_redact_enabled() {
            evt.user_input = Some(journal_content_marker(user_input));
        } else {
            evt.user_input = Some(truncate(user_input, 500));
        }
        evt.error = Some(truncate(error, 500));
        evt.duration_ms = Some(duration_ms);
        evt
    }

    /// Compact event.
    pub fn compact(
        session_id: Option<&str>,
        turn: u32,
        turns_compacted: usize,
        facts_stored: usize,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::Compact, session_id);
        evt.turn = Some(turn);
        evt.turns_compacted = Some(turns_compacted);
        evt.facts_stored = Some(facts_stored);
        evt
    }

    /// Compact event with an optional LLM-generated summary attached in metadata.
    pub fn compact_with_summary(
        session_id: Option<&str>,
        turn: u32,
        turns_compacted: usize,
        facts_stored: usize,
        summary: Option<&str>,
    ) -> Self {
        let mut evt = Self::compact(session_id, turn, turns_compacted, facts_stored);
        if let Some(s) = summary
            && !s.is_empty()
        {
            evt.metadata = Some(serde_json::json!({ "compact_summary": s }));
        }
        evt
    }

    /// Config change event.
    pub fn config_change(session_id: Option<&str>, key: &str, value: &str) -> Self {
        let mut evt = Self::base(JournalEventType::ConfigChange, session_id);
        evt.config_key = Some(key.to_string());
        evt.config_value = Some(value.to_string());
        evt
    }

    /// Config-version transition event.
    ///
    /// Emitted when the active content-addressed config version changes
    /// — either at session startup (`from = None`, `source = "startup"`),
    /// after the user saves an edit via `/config` (`from = Some(prev_id)`,
    /// `source = "slash_config_edit"`), or when the CLI `--settings`
    /// overlay resolves to a new id (`source = "settings_overlay"`).
    ///
    /// Carries the ids in `metadata.config_version.{from, to, source}`
    /// so downstream audit queries (`astra audit show <session>`,
    /// `astra config version diff`) can reconstruct the sequence of
    /// configs a session actually ran under without needing a separate
    /// table join.
    pub fn config_version_change(
        session_id: Option<&str>,
        turn: u32,
        from: Option<&str>,
        to: &str,
        source: &str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ConfigChange, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "config_version": {
                "from": from,
                "to": to,
                "source": source,
            }
        }));
        evt
    }

    /// Error event (non-turn).
    pub fn error(session_id: Option<&str>, error: &str) -> Self {
        let mut evt = Self::base(JournalEventType::Error, session_id);
        evt.error = Some(truncate(error, 500));
        evt
    }

    /// A tool call failed with a non-zero exit, crash, signal, or timeout.
    /// Stores the error message and the associated [`ToolCallRecord`].
    pub fn tool_call_error(
        session_id: Option<&str>,
        turn: u32,
        tool_name: &str,
        error_msg: &str,
        record: ToolCallRecord,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ToolCallError, session_id);
        evt.turn = Some(turn);
        evt.error = Some(truncate(error_msg, 500));
        evt.tools_used = Some(vec![tool_name.to_string()]);
        evt.tool_calls = Some(vec![record]);
        evt
    }

    /// Session end event.
    pub fn session_end(session_id: Option<&str>, total_turns: u32) -> Self {
        let mut evt = Self::base(JournalEventType::SessionEnd, session_id);
        evt.turn = Some(total_turns);
        evt
    }

    /// Attach tool surface data to a turn event.
    pub fn with_tool_surface(
        mut self,
        visible_tools: Vec<String>,
        selected_skills: Vec<String>,
        tools_used: Vec<String>,
        budget_used: u32,
    ) -> Self {
        let visible_tools = normalize_name_list(&visible_tools);
        let selected_skills = normalize_name_list(&selected_skills);
        let mut tools_used = normalize_name_list(&tools_used);
        tools_used.sort_unstable();
        self.visible_tools = Some(visible_tools);
        if !selected_skills.is_empty() {
            self.selected_skills = Some(selected_skills);
        }
        self.tools_used = Some(tools_used);
        self.budget_used = Some(budget_used);
        self
    }

    /// Attach budget pressure to a turn event (0.0-0.9 from compaction tier).
    pub fn with_budget_pressure(mut self, pressure: f64) -> Self {
        self.budget_pressure = Some(pressure);
        self
    }

    /// Attach per-tool-call audit records to a turn event.
    pub fn with_tool_calls(mut self, records: Vec<ToolCallRecord>) -> Self {
        let records = normalize_tool_call_records(records);
        if !records.is_empty() {
            let outcomes = ToolOutcomeSummary::from_records(&records);
            debug_assert!(outcomes.is_consistent());
            self.tool_count = Some(outcomes.executed);
            self.tool_outcomes = Some(outcomes);
            self.tool_calls = Some(records);
        } else {
            // An empty detail vector means this projection has no per-call
            // records. It does not invalidate an authoritative aggregate
            // count already supplied by a remote execution owner.
            self.tool_calls = None;
            self.tool_outcomes = None;
        }
        self
    }

    /// Tag this turn event as belonging to a plan mode subtask.
    pub fn with_plan_subtask(mut self, subtask_id: Option<&str>) -> Self {
        self.plan_subtask_id = subtask_id.map(|s| s.to_string());
        self
    }

    /// Set time to first token (streaming latency).
    pub fn with_ttft(mut self, ttft_ms: Option<u64>) -> Self {
        self.ttft_ms = ttft_ms;
        self
    }

    /// Set context assembly time (prompt building).
    pub fn with_context_time(mut self, context_ms: Option<u64>) -> Self {
        self.context_ms = context_ms;
        self
    }

    /// Set memoria search time.
    pub fn with_memoria_time(mut self, memoria_ms: Option<u64>) -> Self {
        self.memoria_ms = memoria_ms;
        self
    }

    /// Routing telemetry for this REPL turn (journal + analytics).
    pub fn with_routing_telemetry(
        mut self,
        routing_domain_hint: Option<String>,
        entity_learn_skipped_no_domain: bool,
    ) -> Self {
        self.routing_domain_hint = routing_domain_hint;
        self.entity_learn_skipped_no_domain = entity_learn_skipped_no_domain;
        self
    }
    /// Stall detection event.
    pub fn stall_detected(
        session_id: Option<&str>,
        turn: u32,
        stall_type: &str,
        nudge_count: u32,
        confidence: f64,
        avoid_tools: &[String],
    ) -> Self {
        let avoid_tools = normalize_name_list(avoid_tools);
        let mut evt = Self::base(JournalEventType::StallDetected, session_id);
        evt.turn = Some(turn);
        evt.stall_type = Some(stall_type.to_string());
        evt.metadata = Some(serde_json::json!({
            "nudge_count": nudge_count,
            "confidence": confidence,
            "avoid_tools": avoid_tools,
        }));
        evt
    }

    /// Session checkpoint event.
    pub fn checkpoint(
        session_id: Option<&str>,
        turn: u32,
        summary: &str,
        total_tokens: u64,
        tools_used_count: usize,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::Checkpoint, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "summary": truncate(summary, 500),
            "total_tokens": total_tokens,
            "tools_used_count": tools_used_count,
        }));
        evt
    }

    /// TurnGuard verdict event — records unified non-happy-path decisions.
    ///
    /// Only emitted for non-Healthy verdicts (Info, Warning, Critical).
    /// Captures severity, injected messages, avoided tools, and advisory_threshold_reached.
    fn turn_guard_avoid_reason_codes(
        avoid_tools: &[String],
        health_avoidance_tools: &[String],
        timeout_dominant_tools: &[String],
        nudge_count: usize,
        non_timeout_errors: usize,
    ) -> Vec<&'static str> {
        let mut codes = Vec::new();
        if !avoid_tools.is_empty() && !health_avoidance_tools.is_empty() {
            codes.push("tool_health_avoidance");
        }
        if non_timeout_errors > 0 {
            codes.push("session_failures");
        }
        if !timeout_dominant_tools.is_empty() {
            codes.push("timeout_dominant");
        }
        if nudge_count > 0 {
            codes.push("stall_recovery");
        }
        codes
    }

    fn turn_guard_avoid_reason_summary(
        health_avoidance_tools: &[String],
        timeout_dominant_tools: &[String],
        nudge_count: usize,
        non_timeout_errors: usize,
        total_timeouts: usize,
    ) -> Option<String> {
        let mut parts = Vec::new();
        if !health_avoidance_tools.is_empty() {
            parts.push(format!(
                "health avoidance tools: {}",
                health_avoidance_tools.join(", ")
            ));
        }
        if non_timeout_errors > 0 {
            parts.push(format!(
                "{non_timeout_errors} non-timeout failure(s) recorded"
            ));
        }
        if total_timeouts > 0 {
            if timeout_dominant_tools.is_empty() {
                parts.push(format!("{total_timeouts} timeout failure(s) recorded"));
            } else {
                parts.push(format!(
                    "timeout-dominant tools: {}",
                    timeout_dominant_tools.join(", ")
                ));
            }
        }
        if nudge_count > 0 {
            parts.push(format!("{nudge_count} stall/divergence nudge(s) issued"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn turn_guard_verdict(
        session_id: Option<&str>,
        turn: u32,
        severity: &str,
        injections: &[String],
        avoid_tools: &[String],
        health_avoidance_tools: &[String],
        advisory_threshold_reached: bool,
        nudge_count: usize,
        total_errors: usize,
        total_timeouts: usize,
        timeout_dominant_tools: &[String],
        total_cache_hits: usize,
        flaky_count: usize,
    ) -> Self {
        let avoid_tools = normalize_name_list(avoid_tools);
        let health_avoidance_tools = normalize_name_list(health_avoidance_tools);
        let timeout_dominant_tools = normalize_name_list(timeout_dominant_tools);
        let non_timeout_errors = total_errors.saturating_sub(total_timeouts);
        let avoid_reason_codes = Self::turn_guard_avoid_reason_codes(
            &avoid_tools,
            &health_avoidance_tools,
            &timeout_dominant_tools,
            nudge_count,
            non_timeout_errors,
        );
        let avoid_reason_summary = Self::turn_guard_avoid_reason_summary(
            &health_avoidance_tools,
            &timeout_dominant_tools,
            nudge_count,
            non_timeout_errors,
            total_timeouts,
        );
        let mut evt = Self::base(JournalEventType::TurnGuardVerdict, session_id);
        evt.turn = Some(turn);
        evt.stall_type = Some(severity.to_string());
        evt.metadata = Some(serde_json::json!({
            "severity": severity,
            "injections": injections.len(),
            "injection_preview": injections.first().map(|s| truncate(s, 200)),
            "avoid_tools": avoid_tools,
            "avoid_tools_count": avoid_tools.len(),
            "health_avoidance_tools": health_avoidance_tools,
            "timeout_dominant_tools": timeout_dominant_tools,
            "avoid_reason_codes": avoid_reason_codes,
            "avoid_reason_summary": avoid_reason_summary,
            "advisory_threshold_reached": advisory_threshold_reached,
            "nudge_count": nudge_count,
            "total_errors": total_errors,
            "non_timeout_errors": non_timeout_errors,
            "health_avoidance_count": health_avoidance_tools.len(),
            "total_timeouts": total_timeouts,
            "total_cache_hits": total_cache_hits,
            "flaky_tools": flaky_count,
        }));
        evt
    }

    #[allow(clippy::too_many_arguments)]
    pub fn turn_evaluation(
        session_id: Option<&str>,
        turn: Option<u32>,
        source: &str,
        live_query: bool,
        success: bool,
        quality: f64,
        confidence: f64,
        budget_pressure: f64,
        stall_count: usize,
        verdict_warning: bool,
        tool_call_count: usize,
        signals: Vec<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::TurnEvaluation, session_id);
        evt.turn = turn;
        evt.metadata = Some(serde_json::json!({
            "source": source,
            "live_query": live_query,
            "success": success,
            "quality": quality,
            "confidence": confidence,
            "budget_pressure": budget_pressure,
            "stall_count": stall_count,
            "verdict_warning": verdict_warning,
            "tool_call_count": tool_call_count,
            "signal_count": signals.len(),
            "signals": signals,
        }));
        evt
    }

    /// Build a plan progress event — emitted when a subtask starts, completes, or plan finishes.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_progress(
        session_id: Option<&str>,
        turn: u32,
        subtask_id: &str,
        subtask_title: &str,
        action: &str, // "started" | "completed" | "skipped" | "plan_complete" | "plan_paused"
        progress_pct: u32,
        total_subtasks: usize,
        completed_subtasks: usize,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PlanProgress, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "subtask_id": subtask_id,
            "subtask_title": subtask_title,
            "action": action,
            "progress_pct": progress_pct,
            "total_subtasks": total_subtasks,
            "completed_subtasks": completed_subtasks,
        }));
        evt
    }

    /// Plan edited — subtask added/removed/reordered, goal changed.
    pub fn plan_edit(
        session_id: Option<&str>,
        action: &str,
        metadata: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PlanEdit, session_id);
        evt.metadata = Some(serde_json::json!({
            "action": action,
            "detail": metadata,
        }));
        evt
    }

    /// Plan lifecycle event — created, completed, abandoned, replanned.
    pub fn plan_lifecycle(
        session_id: Option<&str>,
        summary: &str,
        metadata: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PlanLifecycle, session_id);
        evt.metadata = Some(serde_json::json!({
            "summary": summary,
            "detail": metadata,
        }));
        evt
    }

    /// Task lifecycle event — created, updated, completed, failed, cancelled.
    pub fn task_lifecycle(
        session_id: Option<&str>,
        turn: u32,
        summary: &str,
        metadata: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::TaskLifecycle, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "summary": summary,
            "detail": metadata,
        }));
        evt
    }

    /// Goal steering event — manual goal set or plan-goal alignment took over.
    pub fn goal_steered(
        session_id: Option<&str>,
        turn: u32,
        source: &str,
        previous_goal: Option<&str>,
        new_goal: &str,
        metadata: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::GoalSteered, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "source": source,
            "previous_goal": previous_goal,
            "new_goal": new_goal,
            "detail": metadata,
        }));
        evt
    }

    /// Verification completed — emitted after subtask or global verification.
    pub fn verification_completed(
        session_id: Option<&str>,
        turn: u32,
        subtask_id: &str,
        scope: &str, // "subtask" | "global"
        passed: bool,
        results: &serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::VerificationCompleted, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "subtask_id": subtask_id,
            "scope": scope,
            "passed": passed,
            "results": results,
        }));
        evt
    }

    /// Delegation started event — emitted when a delegation group is spawned.
    pub fn delegation_started(
        session_id: Option<&str>,
        delegation_id: &str,
        parent_run_id: &str,
        pattern: &str,
        agent_ids: &[String],
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationStarted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "parent_run_id": parent_run_id,
            "pattern": pattern,
            "agent_ids": agent_ids,
            "agent_count": agent_ids.len(),
        }));
        evt
    }

    /// Delegation sub-run started event — emitted when a single sub-run enters running state.
    #[allow(clippy::too_many_arguments)]
    pub fn delegation_sub_run_started(
        session_id: Option<&str>,
        delegation_id: &str,
        sub_run_id: &str,
        parent_run_id: &str,
        agent_id: &str,
        status: &str,
        depth: u32,
        retry_of: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationSubRunStarted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "sub_run_id": sub_run_id,
            "parent_run_id": parent_run_id,
            "agent_id": agent_id,
            "status": status,
            "depth": depth,
            "retry_of": retry_of,
        }));
        evt
    }

    /// Delegation sub-run completed event — emitted when a single sub-run finishes.
    pub fn delegation_sub_run_completed(
        session_id: Option<&str>,
        delegation_id: &str,
        sub_run_id: &str,
        agent_id: &str,
        status: &str,
        error: Option<&str>,
        output_preview: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationSubRunCompleted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "sub_run_id": sub_run_id,
            "agent_id": agent_id,
            "status": status,
            "error": error.map(|msg| truncate(msg, 500)),
            "output_preview": output_preview.map(|msg| truncate(msg, 500)),
        }));
        evt
    }

    /// Delegation retry event - emitted when a verification-gated sub-run spawns a retry.
    pub fn delegation_retry(
        session_id: Option<&str>,
        delegation_id: &str,
        original_run_id: &str,
        retry_run_id: &str,
        agent_id: &str,
        attempt: u32,
        reason: &str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationRetry, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "original_run_id": original_run_id,
            "retry_run_id": retry_run_id,
            "agent_id": agent_id,
            "attempt": attempt,
            "reason": reason,
        }));
        evt
    }

    /// Delegation completed event — emitted when all sub-runs finish and results aggregate.
    #[allow(clippy::too_many_arguments)]
    pub fn delegation_completed(
        session_id: Option<&str>,
        delegation_id: &str,
        pattern: &str,
        total_sub_runs: usize,
        succeeded: usize,
        failed: usize,
        aggregated_status: &str,
        aggregated_output_preview: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationCompleted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "pattern": pattern,
            "total_sub_runs": total_sub_runs,
            "succeeded": succeeded,
            "failed": failed,
            "aggregated_status": aggregated_status,
            "aggregated_output_preview": aggregated_output_preview.map(|msg| truncate(msg, 500)),
        }));
        evt
    }

    /// Agent spawned event — marks the exact moment a child agent starts.
    /// Emitted by the spawner after successful registration so the unified
    /// timeline can show when each child was created.
    #[allow(clippy::too_many_arguments)]
    pub fn agent_spawned(
        session_id: Option<&str>,
        agent_id: &str,
        run_id: &str,
        parent_run_id: &str,
        agent_type: &str,
        description: &str,
        model: Option<&str>,
        inherit_prefix: bool,
        execution_metadata: Option<&serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AgentSpawned, session_id);
        let mut metadata = serde_json::json!({
            "agent_id": agent_id,
            "run_id": run_id,
            "parent_run_id": parent_run_id,
            "agent_type": agent_type,
            "description": description,
            "model": model,
            "inherit_prefix": inherit_prefix,
        });
        merge_execution_boundary_metadata(&mut metadata, execution_metadata);
        evt.metadata = Some(metadata);
        evt
    }

    /// Spawn event with the immutable fanout slot identity needed to recover a
    /// group after the parent turn's in-memory runtime binding has gone away.
    #[allow(clippy::too_many_arguments)]
    pub fn agent_spawned_with_fanout(
        session_id: Option<&str>,
        agent_id: &str,
        run_id: &str,
        parent_run_id: &str,
        agent_type: &str,
        description: &str,
        model: Option<&str>,
        inherit_prefix: bool,
        fanout_slot: Option<&serde_json::Value>,
        execution_metadata: Option<&serde_json::Value>,
    ) -> Self {
        let mut event = Self::agent_spawned(
            session_id,
            agent_id,
            run_id,
            parent_run_id,
            agent_type,
            description,
            model,
            inherit_prefix,
            execution_metadata,
        );
        if let Some(fanout_slot) = fanout_slot
            && let Some(metadata) = event
                .metadata
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
        {
            metadata.insert("fanout_slot".to_string(), fanout_slot.clone());
        }
        event
    }

    /// Agent terminated event — persists final state of a spawned agent.
    #[allow(clippy::too_many_arguments)]
    pub fn agent_terminated(
        session_id: Option<&str>,
        agent_id: &str,
        run_id: &str,
        agent_type: &str,
        status: &str,
        finish_reason: Option<&str>,
        turns_completed: Option<u32>,
        tool_calls: u32,
        prompt_tokens: u64,
        completion_tokens: u64,
        duration_ms: u64,
        execution_metadata: Option<&serde_json::Value>,
    ) -> Self {
        Self::agent_terminated_with_metric_completeness(
            session_id,
            agent_id,
            run_id,
            agent_type,
            status,
            finish_reason,
            turns_completed,
            tool_calls,
            prompt_tokens,
            completion_tokens,
            duration_ms,
            None,
            execution_metadata,
        )
    }

    /// Persist a terminal spawned-agent fact without treating unavailable
    /// cancellation-time counters as measured zeroes.
    #[allow(clippy::too_many_arguments)]
    pub fn agent_terminated_with_metric_completeness(
        session_id: Option<&str>,
        agent_id: &str,
        run_id: &str,
        agent_type: &str,
        status: &str,
        finish_reason: Option<&str>,
        turns_completed: Option<u32>,
        tool_calls: u32,
        prompt_tokens: u64,
        completion_tokens: u64,
        duration_ms: u64,
        metrics_completeness: Option<&str>,
        execution_metadata: Option<&serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AgentTerminated, session_id);
        let mut metadata = serde_json::json!({
            "agent_id": agent_id,
            "run_id": run_id,
            "agent_type": agent_type,
            "status": status,
            "tool_calls": tool_calls,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "duration_ms": duration_ms,
        });
        if let Some(turns_completed) = turns_completed {
            metadata["turns_completed"] = serde_json::json!(turns_completed);
        }
        if let Some(finish_reason) = finish_reason.filter(|reason| !reason.is_empty()) {
            metadata["finish_reason"] = serde_json::Value::String(finish_reason.to_string());
        }
        if let Some(metrics_completeness) = metrics_completeness
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            metadata["metrics_completeness"] =
                serde_json::Value::String(metrics_completeness.to_string());
        }
        merge_execution_boundary_metadata(&mut metadata, execution_metadata);
        evt.metadata = Some(metadata);
        evt
    }

    /// Context assembly recorded event — deep observability for turn context composition.
    ///
    /// The `trace` should be a serialized `ContextAssemblyTrace` from runtime.
    /// Stores full context breakdown: system prompt, history, memory, tools, token budget.
    pub fn context_assembly_recorded(
        session_id: Option<&str>,
        turn: u32,
        trace: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ContextAssemblyRecorded, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "trace_recorded": true,
            "trace_kind": "context_assembly",
            "turn_id": trace.get("turn_id").and_then(|value| value.as_str()),
            "tool_count": trace
                .get("tools")
                .and_then(|tools| tools.get("visible_tools"))
                .and_then(|selected| selected.as_array())
                .map(Vec::len),
            "total_tokens": trace
                .get("token_budget")
                .and_then(|budget| budget.get("total_used"))
                .and_then(|value| value.as_u64()),
        }));
        evt.context_assembly_trace = Some(trace);
        evt
    }

    /// Full LLM request payload recorded in the session journal.
    pub fn llm_request_full(
        session_id: Option<&str>,
        turn: u32,
        round: u32,
        metadata: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::LlmRequestFull, session_id);
        evt.turn = Some(turn);
        evt.round = Some(round);
        evt.metadata = Some(metadata);
        evt
    }

    /// Full LLM response payload recorded in the session journal.
    pub fn llm_response_full(
        session_id: Option<&str>,
        turn: u32,
        round: u32,
        metadata: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::LlmResponseFull, session_id);
        evt.turn = Some(turn);
        evt.round = Some(round);
        evt.metadata = Some(metadata);
        evt
    }
    /// Focus drift detected — emitted when drift analysis finds significant drift.
    pub fn drift_detected(
        session_id: Option<&str>,
        turn: u32,
        severity: f64,
        cause: astra_core::DriftCause,
        evidence: Vec<astra_core::DriftEvidence>,
        recovery_suggestion: &str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DriftDetected, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "severity": severity,
            "cause": cause,
            "evidence_count": evidence.len(),
            "evidence": evidence,
            "recovery_suggestion": recovery_suggestion,
        }));
        evt
    }

    /// Adaptive scenario applied — emitted once per session when the adaptive
    /// profile selects a scenario and applies config adjustments.
    #[allow(clippy::too_many_arguments)]
    pub fn adaptive_scenario_applied(
        session_id: Option<&str>,
        turn: u32,
        scenario: &str,
        confidence: f64,
        config_changes: Vec<(String, String, String)>, // (key, from, to)
        experiment_id: Option<&str>,
        variant_id: Option<&str>,
        baseline_applied: bool,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AdaptiveScenarioApplied, session_id);
        evt.turn = Some(turn);
        let changes: Vec<serde_json::Value> = config_changes
            .iter()
            .map(|(k, from, to)| serde_json::json!({"key": k, "from": from, "to": to}))
            .collect();
        evt.metadata = Some(serde_json::json!({
            "scenario": scenario,
            "confidence": confidence,
            "config_changes": changes,
            "experiment_id": experiment_id,
            "variant_id": variant_id,
            "baseline_applied": baseline_applied,
        }));
        evt
    }

    /// Per-turn micro-adaptation applied — emitted when per-turn adaptation
    /// modifies runtime config based on immediate signals.
    pub fn adaptive_per_turn_applied(
        session_id: Option<&str>,
        turn: u32,
        changes: Vec<(String, String, String)>, // (key, from, to)
        triggers: Vec<String>,                  // reason strings
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AdaptivePerTurnApplied, session_id);
        evt.turn = Some(turn);
        let change_vals: Vec<serde_json::Value> = changes
            .iter()
            .map(|(k, from, to)| serde_json::json!({"key": k, "from": from, "to": to}))
            .collect();
        evt.metadata = Some(serde_json::json!({
            "changes": change_vals,
            "triggers": triggers,
        }));
        evt
    }

    /// Record a structured interruption (budget exhaustion, rate limit, cancel, etc.).
    pub fn interruption_recorded(
        session_id: Option<&str>,
        turn: u32,
        interruption_json: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::InterruptionRecorded, session_id);
        evt.turn = Some(turn);
        let kind_str = interruption_json
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let resumable = interruption_json
            .get("resumable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        evt.user_input = Some(truncate(
            &format!("interruption: {} (resumable={})", kind_str, resumable,),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "interruption": interruption_json,
        }));
        evt
    }

    /// Build a compaction retry telemetry event.
    ///
    /// Emitted after a successful compaction retry to capture operational metrics:
    /// tier escalation, tokens freed, budget satisfaction, and per-layer breakdown.
    #[allow(clippy::too_many_arguments)]
    pub fn compaction_retry(
        session_id: Option<&str>,
        turn: u32,
        tier: &str,
        tokens_freed: u64,
        budget_likely_satisfied: bool,
        retry_count: u32,
        layers: Vec<(String, u64)>,
        consecutive_context_window_errors: u32,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::CompactionRetry, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "compaction": {
                "tier": tier,
                "tokens_freed": tokens_freed,
                "budget_likely_satisfied": budget_likely_satisfied,
                "retry_count": retry_count,
                "consecutive_context_window_errors": consecutive_context_window_errors,
                "layers": layers.iter().map(|(name, freed)| {
                    serde_json::json!({ "name": name, "tokens_freed": freed })
                }).collect::<Vec<_>>(),
            }
        }));
        evt
    }

    /// Session-memory (session-memory.md) extraction outcome event.
    ///
    /// Metadata is a flat, self-describing object driven by the
    /// [`SessionMemoryExtractionOutcome`] enum.
    pub fn session_memory_extraction(
        session_id: Option<&str>,
        turn: u32,
        duration_ms: u64,
        outcome: SessionMemoryExtractionOutcome,
        breadcrumbs: &SessionMemoryExtractionBreadcrumbs,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::SessionMemoryExtraction, session_id);
        evt.turn = Some(turn);
        evt.duration_ms = Some(duration_ms);
        evt.metadata = Some(outcome.to_json(breadcrumbs));
        evt
    }

    /// Durable machine-readable evidence for degraded asynchronous work.
    ///
    /// The four classification fields must be stable identifiers. Free-form
    /// provider/backend errors belong in logs, not in this bounded contract.
    pub fn subsystem_diagnostic(
        session_id: Option<&str>,
        turn: u32,
        severity: SubsystemDiagnosticSeverity,
        subsystem: &'static str,
        operation: &'static str,
        code: &'static str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::SubsystemDiagnostic, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "severity": severity,
            "subsystem": subsystem,
            "operation": operation,
            "code": code,
        }));
        evt
    }

    /// Marks the point after which a verifier has complete evidence for one
    /// asynchronous subsystem's work on this turn.
    pub fn subsystem_settled(session_id: Option<&str>, turn: u32, subsystem: &'static str) -> Self {
        let mut evt = Self::base(JournalEventType::SubsystemSettled, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({"subsystem": subsystem}));
        evt
    }

    /// Context pipeline per-turn feedback event.
    pub fn pipeline_feedback(
        session_id: Option<&str>,
        turn: u32,
        event_payload: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PipelineFeedback, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(event_payload);
        evt
    }

    /// Context pipeline alert event.
    pub fn pipeline_alert(
        session_id: Option<&str>,
        turn: u32,
        event_payload: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PipelineAlert, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(event_payload);
        evt
    }

    /// Context pipeline compaction audit event.
    pub fn pipeline_compaction_audit(
        session_id: Option<&str>,
        turn: u32,
        event_payload: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PipelineCompactionAudit, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(event_payload);
        evt
    }
}

/// Stable identity for one logical item in a local run transcript.
///
/// Retrying an ambiguous append must reproduce this value. The message payload
/// is deliberately excluded: a retry must not become a second object merely
/// because a provider reformatted equivalent content, while a conflicting
/// payload at the same run sequence remains a data-integrity conflict for the
/// reader to surface rather than a silently distinct transcript item.
fn local_transcript_source_event_id(
    session_id: &str,
    run_id: &str,
    agent_id: &str,
    item_seq: u64,
) -> String {
    let identity = serde_json::json!([session_id, run_id, agent_id, item_seq]);
    let digest = Sha256::digest(identity.to_string().as_bytes());
    format!("local-transcript:sha256:{digest:x}")
}
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

pub const ASTRA_JOURNAL_CONTENT_REDACT_ENV: &str = "ASTRA_JOURNAL_CONTENT_REDACT";

static JOURNAL_CONTENT_REDACT_OVERRIDE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

pub fn set_journal_content_redact_override(enabled: Option<bool>) {
    let encoded = match enabled {
        Some(false) => 1,
        Some(true) => 2,
        None => 0,
    };
    JOURNAL_CONTENT_REDACT_OVERRIDE.store(encoded, std::sync::atomic::Ordering::Relaxed);
}

/// Returns true when [`ASTRA_JOURNAL_CONTENT_REDACT_ENV`]=`1` is set in the
/// environment. When enabled, the on-disk JSONL journal stores a privacy
/// marker (`<redacted: len=N sha=...>`) in place of `user_input` and
/// `assistant_output` fields.
pub fn journal_content_redact_enabled() -> bool {
    match JOURNAL_CONTENT_REDACT_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    std::env::var(ASTRA_JOURNAL_CONTENT_REDACT_ENV).as_deref() == Ok("1")
}

/// Replace raw user content with a deterministic privacy marker.
///
/// Uses a non-cryptographic 64-bit hash for dedup/debugging only — not as
/// a security primitive.
pub fn journal_content_marker(raw: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    raw.hash(&mut h);
    format!("<redacted: len={} sha={:016x}>", raw.len(), h.finish())
}

// ═══════════════════════════ Session Lifecycle ════════════════════════════

/// Result of a single session lifecycle maintenance run.
#[derive(Debug, Default)]
pub struct SessionMaintenanceResult {
    /// Number of sessions deleted (TTL expired).
    pub sessions_deleted: usize,
    /// Number of journals compressed (.jsonl → .jsonl.gz).
    pub journals_compressed: usize,
    /// Total disk bytes freed by deletion.
    pub bytes_freed: u64,
    /// Errors encountered (non-fatal, best-effort).
    pub errors: Vec<String>,
}

/// Run session lifecycle maintenance: delete expired sessions and compress old journals.
///
/// - `ttl_days`: sessions older than this are deleted entirely (default: 30).
/// - `compress_after_days`: journals older than this (but younger than ttl) are gzip-compressed (default: 7).
///
/// Both thresholds use the journal file's modification time. This function is
/// idempotent and safe to call at every REPL startup.
pub fn run_session_maintenance(
    ttl_days: u64,
    compress_after_days: u64,
) -> SessionMaintenanceResult {
    let dir = journal_dir();
    if !dir.exists() {
        return SessionMaintenanceResult::default();
    }
    run_session_maintenance_in(dir, ttl_days, compress_after_days)
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    use crate::interaction_contract::{
        InteractionDurableStore, InteractionKind, InteractionStatus,
    };

    #[test]
    fn find_latest_approval_decision_reads_latest_matching_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-approval").unwrap();

        writer
            .append(&JournalEvent::approval_decision(
                Some("sess-approval"),
                Some(7),
                "req-1",
                Some("write_file"),
                Some("standard"),
                "allow",
                None,
            ))
            .unwrap();
        writer
            .append(&JournalEvent::approval_decision(
                Some("sess-approval"),
                Some(9),
                "req-2",
                Some("bash"),
                Some("explicit"),
                "deny",
                Some("too dangerous"),
            ))
            .unwrap();

        let found = find_latest_approval_decision("sess-approval", "req-2")
            .unwrap()
            .expect("approval decision");
        assert_eq!(found.request_id, "req-2");
        assert_eq!(found.decision, "deny");
        assert_eq!(found.reason.as_deref(), Some("too dangerous"));
        assert_eq!(found.tool_name.as_deref(), Some("bash"));
        assert_eq!(found.approval_kind.as_deref(), Some("explicit"));
    }

    #[test]
    fn find_latest_approval_decision_ignores_non_matching_events() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-approval").unwrap();

        writer
            .append(&JournalEvent::approval_required(
                Some("sess-approval"),
                Some(4),
                "req-1",
                "write_file",
                "standard",
                Some("src/lib.rs"),
            ))
            .unwrap();

        let found = find_latest_approval_decision("sess-approval", "req-1").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn find_latest_approval_required_reads_matching_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-approval").unwrap();

        writer
            .append(&JournalEvent::approval_required(
                Some("sess-approval"),
                Some(11),
                "req-11",
                "bash",
                "explicit",
                Some("cargo test"),
            ))
            .unwrap();

        let found = find_latest_approval_required("sess-approval", "req-11")
            .unwrap()
            .expect("approval request");
        assert_eq!(found.request_id, "req-11");
        assert_eq!(found.turn, Some(11));
        assert_eq!(found.tool_name.as_deref(), Some("bash"));
        assert_eq!(found.approval_kind.as_deref(), Some("explicit"));
    }

    #[test]
    fn approval_lookup_for_run_ignores_same_request_id_from_other_run() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-approval-run").unwrap();

        writer
            .append(&JournalEvent::approval_required_for_run(
                Some("sess-approval-run"),
                Some(1),
                "shared-req",
                Some("run-a"),
                "bash",
                "explicit",
                Some("rm -rf tmp"),
            ))
            .unwrap();
        writer
            .append(&JournalEvent::approval_decision_for_run(
                Some("sess-approval-run"),
                Some(1),
                "shared-req",
                Some("run-a"),
                Some("bash"),
                Some("explicit"),
                "deny",
                Some("wrong run"),
            ))
            .unwrap();
        writer
            .append(&JournalEvent::approval_required_for_run(
                Some("sess-approval-run"),
                Some(2),
                "shared-req",
                Some("run-b"),
                "write_file",
                "standard",
                Some("src/lib.rs"),
            ))
            .unwrap();
        writer
            .append(&JournalEvent::approval_decision_for_run(
                Some("sess-approval-run"),
                Some(2),
                "shared-req",
                Some("run-b"),
                Some("write_file"),
                Some("standard"),
                "allow",
                None,
            ))
            .unwrap();

        let request =
            find_latest_approval_required_for_run("sess-approval-run", "shared-req", "run-b")
                .unwrap()
                .expect("run-b approval request");
        assert_eq!(request.run_id.as_deref(), Some("run-b"));
        assert_eq!(request.turn, Some(2));
        assert_eq!(request.tool_name.as_deref(), Some("write_file"));

        let decision =
            find_latest_approval_decision_for_run("sess-approval-run", "shared-req", "run-b")
                .unwrap()
                .expect("run-b approval decision");
        assert_eq!(decision.run_id.as_deref(), Some("run-b"));
        assert_eq!(decision.decision, "allow");
        assert_eq!(decision.reason, None);

        assert!(
            find_latest_approval_decision_for_run("sess-approval-run", "shared-req", "run-missing")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn approval_decision_projects_to_run_scoped_interaction_contract() {
        let decision = ApprovalJournalDecision {
            request_id: "req-contract".into(),
            run_id: Some("run-contract".into()),
            decision: "deny".into(),
            reason: Some("not safe".into()),
            tool_name: Some("bash".into()),
            approval_kind: Some("explicit".into()),
        };

        let contract = decision
            .interaction_contract("sess-contract", Some("user-contract"))
            .expect("run-scoped approval decision should become an interaction contract");

        assert_eq!(contract.kind, InteractionKind::Approval);
        assert_eq!(
            contract.durable_store,
            InteractionDurableStore::SessionJournal
        );
        assert_eq!(contract.status, InteractionStatus::Resolved);
        assert_eq!(contract.identity.user_id.as_deref(), Some("user-contract"));
        assert_eq!(contract.identity.session_id, "sess-contract");
        assert_eq!(contract.identity.run_id, "run-contract");
        assert_eq!(contract.identity.request_id, "req-contract");
        assert!(contract.is_wait_satisfied());
    }

    #[test]
    fn approval_decision_without_run_id_is_not_a_cross_pod_contract() {
        let decision = ApprovalJournalDecision {
            request_id: "legacy-req".into(),
            run_id: None,
            decision: "allow".into(),
            reason: None,
            tool_name: None,
            approval_kind: None,
        };

        assert!(
            decision
                .interaction_contract("sess-contract", Some("user-contract"))
                .is_none(),
            "legacy session/request-only decisions are not safe no-sticky interaction facts"
        );
    }

    #[test]
    fn append_approval_decision_for_run_is_idempotent_and_conflict_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());

        let first = append_approval_decision_for_run_if_absent(
            "sess-approval-idempotent",
            Some(3),
            "req-idem",
            "run-idem",
            Some("write_file"),
            Some("standard"),
            "allow",
            Some("approved"),
        )
        .unwrap();
        assert_eq!(first, ApprovalDecisionAppendOutcome::Appended);

        let duplicate = append_approval_decision_for_run_if_absent(
            "sess-approval-idempotent",
            Some(3),
            "req-idem",
            "run-idem",
            Some("write_file"),
            Some("standard"),
            "allow",
            Some("approved"),
        )
        .unwrap();
        assert_eq!(duplicate, ApprovalDecisionAppendOutcome::Idempotent);

        let conflict = append_approval_decision_for_run_if_absent(
            "sess-approval-idempotent",
            Some(3),
            "req-idem",
            "run-idem",
            Some("write_file"),
            Some("standard"),
            "deny",
            Some("late conflicting replay"),
        )
        .unwrap();
        let ApprovalDecisionAppendOutcome::Conflict(existing) = conflict else {
            panic!("expected conflict for distinct decision replay");
        };
        assert_eq!(existing.decision, "allow");
        assert_eq!(existing.reason.as_deref(), Some("approved"));

        let decisions = read_journal("sess-approval-idempotent")
            .unwrap()
            .into_iter()
            .filter(|event| event.event_type == JournalEventType::ApprovalDecision)
            .count();
        assert_eq!(decisions, 1, "idempotent/conflict paths must not append");
    }

    #[test]
    fn authenticated_approval_decisions_are_owner_isolated() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());

        let outcome = append_approval_decision_for_user_run_if_absent(
            "user-a",
            "sess-owner-approval",
            Some(1),
            "req-owner",
            "run-owner",
            Some("bash"),
            Some("explicit"),
            "allow",
            None,
        )
        .unwrap();
        assert_eq!(outcome, ApprovalDecisionAppendOutcome::Appended);

        assert!(
            find_latest_approval_decision_for_user_run(
                "user-a",
                "sess-owner-approval",
                "req-owner",
                "run-owner",
            )
            .unwrap()
            .is_some()
        );
        assert!(
            find_latest_approval_decision_for_user_run(
                "user-b",
                "sess-owner-approval",
                "req-owner",
                "run-owner",
            )
            .unwrap()
            .is_none()
        );
        assert!(
            find_latest_approval_decision_for_run("sess-owner-approval", "req-owner", "run-owner",)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn authenticated_approval_lookup_advances_by_durable_append_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::for_user("user-a", "sess-approval-cursor").unwrap();
        writer
            .append(&JournalEvent::session_start(
                Some("sess-approval-cursor"),
                Some("model-a"),
            ))
            .unwrap();

        let (missing, cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-approval-cursor",
            "req-cursor",
            "run-cursor",
            None,
        )
        .unwrap();
        assert!(missing.is_none());
        assert!(cursor.offset() > 0);

        writer
            .append(&JournalEvent::approval_decision_for_run(
                Some("sess-approval-cursor"),
                Some(1),
                "req-cursor",
                Some("run-cursor"),
                Some("write_file"),
                Some("standard"),
                "allow",
                None,
            ))
            .unwrap();

        let (found, next_cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-approval-cursor",
            "req-cursor",
            "run-cursor",
            Some(&cursor),
        )
        .unwrap();
        assert_eq!(found.unwrap().decision, "allow");
        assert!(next_cursor.offset() > cursor.offset());

        let (no_replay, stable_cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-approval-cursor",
            "req-cursor",
            "run-cursor",
            Some(&next_cursor),
        )
        .unwrap();
        assert!(no_replay.is_none());
        assert_eq!(stable_cursor, next_cursor);

        // Grow the old file enough that a replacement is unambiguously
        // shorter, then truncate it as an operator rotation/recovery would.
        for _ in 0..8 {
            writer
                .append(&JournalEvent::session_start(
                    Some("sess-approval-cursor"),
                    Some("padding-model"),
                ))
                .unwrap();
        }
        let (_, old_file_cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-approval-cursor",
            "missing-before-rotation",
            "run-cursor",
            Some(&stable_cursor),
        )
        .unwrap();
        assert!(old_file_cursor.offset() > stable_cursor.offset());

        std::fs::OpenOptions::new()
            .write(true)
            .open(writer.path())
            .unwrap()
            .set_len(0)
            .unwrap();
        writer
            .append(&JournalEvent::approval_decision_for_run(
                Some("sess-approval-cursor"),
                Some(2),
                "req-after-rotation",
                Some("run-cursor"),
                Some("bash"),
                Some("standard"),
                "deny",
                None,
            ))
            .unwrap();

        let (after_rotation, replacement_cursor) =
            find_latest_approval_decision_for_user_run_after(
                "user-a",
                "sess-approval-cursor",
                "req-after-rotation",
                "run-cursor",
                Some(&old_file_cursor),
            )
            .unwrap();
        assert_eq!(after_rotation.unwrap().decision, "deny");
        assert!(replacement_cursor.offset() < old_file_cursor.offset());
    }

    #[test]
    fn authenticated_approval_cursor_detects_equal_size_atomic_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::for_user("user-a", "sess-equal-rotation").unwrap();
        writer
            .append(&JournalEvent::approval_decision_for_run(
                Some("sess-equal-rotation"),
                Some(1),
                "req-before",
                Some("run-rotation"),
                Some("bash"),
                Some("standard"),
                "allow",
                None,
            ))
            .unwrap();
        let (_, cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-equal-rotation",
            "missing",
            "run-rotation",
            None,
        )
        .unwrap();

        let original = std::fs::read(writer.path()).unwrap();
        let original_text = String::from_utf8(original).unwrap();
        let replacement = original_text.replace("req-before", "req-afterx");
        assert_eq!(replacement.len() as u64, cursor.offset());
        let replacement_path = writer.path().with_extension("replacement");
        std::fs::write(&replacement_path, replacement).unwrap();
        std::fs::rename(&replacement_path, writer.path()).unwrap();

        let (found, next_cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-equal-rotation",
            "req-afterx",
            "run-rotation",
            Some(&cursor),
        )
        .unwrap();
        assert_eq!(found.unwrap().decision, "allow");
        assert_eq!(next_cursor.offset(), cursor.offset());
        assert_ne!(next_cursor, cursor, "file generation must change");
    }

    #[test]
    fn authenticated_approval_cursor_detects_same_inode_truncate_and_regrow() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::for_user("user-a", "sess-inode-regrow").unwrap();
        writer
            .append(&JournalEvent::approval_decision_for_run(
                Some("sess-inode-regrow"),
                Some(1),
                "req-before",
                Some("run-regrow"),
                Some("bash"),
                Some("standard"),
                "allow",
                None,
            ))
            .unwrap();
        let (_, cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-inode-regrow",
            "missing",
            "run-regrow",
            None,
        )
        .unwrap();

        let original = std::fs::read_to_string(writer.path()).unwrap();
        let replacement = original.replace("req-before", "req-afterx");
        assert_eq!(replacement.len() as u64, cursor.offset());
        // `write` truncates and regrows the existing inode. The entire change
        // occurs between polls, so length and inode alone are unchanged.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(writer.path(), replacement).unwrap();

        let (found, next_cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-inode-regrow",
            "req-afterx",
            "run-regrow",
            Some(&cursor),
        )
        .unwrap();
        assert_eq!(found.unwrap().decision, "allow");
        assert_eq!(next_cursor.offset(), cursor.offset());
        assert_ne!(
            next_cursor, cursor,
            "same-inode rewrite must replace cursor boundary evidence"
        );
    }

    #[test]
    fn authenticated_approval_cursor_detects_large_middle_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::for_user("user-a", "sess-large-regrow").unwrap();
        let padding = format!(
            "{}\n",
            serde_json::to_string(&JournalEvent::session_start(
                Some("sess-large-regrow"),
                Some("padding-model"),
            ))
            .unwrap()
        );
        let decision = format!(
            "{}\n",
            serde_json::to_string(&JournalEvent::approval_decision_for_run(
                Some("sess-large-regrow"),
                Some(1),
                "req-before",
                Some("run-regrow"),
                Some("bash"),
                Some("standard"),
                "allow",
                None,
            ))
            .unwrap()
        );
        let original = format!("{}{}{}", padding.repeat(180), decision, padding.repeat(180));
        assert!(original.len() > 32 * 1024);
        std::fs::write(writer.path(), &original).unwrap();
        let (_, cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-large-regrow",
            "missing",
            "run-regrow",
            None,
        )
        .unwrap();

        // Preserve inode, length, and both distant ends while changing only a
        // decision in the middle. Sampled prefix/suffix fingerprints miss this
        // operator truncate+regrow pattern permanently.
        let replacement = original.replace("req-before", "req-afterx");
        assert_eq!(replacement.len(), original.len());
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(writer.path(), replacement).unwrap();

        let (found, next_cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-large-regrow",
            "req-afterx",
            "run-regrow",
            Some(&cursor),
        )
        .unwrap();
        assert_eq!(found.unwrap().decision, "allow");
        assert_eq!(next_cursor.offset(), cursor.offset());
        assert_ne!(next_cursor, cursor, "middle rewrite must change generation");
    }

    #[cfg(unix)]
    #[test]
    fn approval_append_retries_when_locked_inode_is_rotated() {
        use fs2::FileExt;

        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::for_user("user-a", "sess-append-rotation").unwrap();
        writer
            .append(&JournalEvent::session_start(
                Some("sess-append-rotation"),
                Some("model-a"),
            ))
            .unwrap();
        let path = writer.path().to_path_buf();
        let locked_old = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        FileExt::lock_exclusive(&locked_old).unwrap();

        let (opened_tx, opened_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        *APPROVAL_APPEND_OPEN_HOOK.lock().unwrap() = Some(ApprovalAppendOpenHook {
            path: path.clone(),
            opened: opened_tx,
            resume: resume_rx,
        });
        let journal_dir = current_journal_dir_override().unwrap();
        let append = std::thread::spawn(move || {
            let _guard = JournalDirGuard::new(journal_dir);
            append_approval_decision_for_user_run_if_absent(
                "user-a",
                "sess-append-rotation",
                Some(1),
                "req-rotation",
                "run-rotation",
                Some("bash"),
                Some("standard"),
                "allow",
                None,
            )
        });
        opened_rx.recv().unwrap();
        let rotated = path.with_extension("rotated");
        std::fs::rename(&path, &rotated).unwrap();
        std::fs::write(&path, []).unwrap();
        FileExt::unlock(&locked_old).unwrap();
        resume_tx.send(()).unwrap();

        assert_eq!(
            append.join().unwrap().unwrap(),
            ApprovalDecisionAppendOutcome::Appended
        );
        let current = find_latest_approval_decision_for_user_run(
            "user-a",
            "sess-append-rotation",
            "req-rotation",
            "run-rotation",
        )
        .unwrap();
        assert_eq!(current.unwrap().decision, "allow");
        let rotated_text = std::fs::read_to_string(rotated).unwrap();
        assert!(!rotated_text.contains("req-rotation"));
    }

    #[test]
    fn unchanged_approval_cursor_does_not_wait_for_the_writer_lock() {
        use fs2::FileExt;

        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::for_user("user-a", "sess-fast-cursor").unwrap();
        writer
            .append(&JournalEvent::session_start(
                Some("sess-fast-cursor"),
                Some("model-a"),
            ))
            .unwrap();
        let (_, cursor) = find_latest_approval_decision_for_user_run_after(
            "user-a",
            "sess-fast-cursor",
            "missing",
            "run-fast",
            None,
        )
        .unwrap();

        let locked = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(writer.path())
            .unwrap();
        FileExt::lock_exclusive(&locked).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let cursor_for_thread = cursor.clone();
        let journal_dir = current_journal_dir_override().unwrap();
        let handle = std::thread::spawn(move || {
            let _guard = JournalDirGuard::new(journal_dir);
            let result = find_latest_approval_decision_for_user_run_after(
                "user-a",
                "sess-fast-cursor",
                "missing",
                "run-fast",
                Some(&cursor_for_thread),
            );
            let _ = sent.send(result);
        });
        let result = received.recv_timeout(std::time::Duration::from_secs(1));
        FileExt::unlock(&locked).unwrap();
        let result = result.expect("unchanged cursor must not wait for an exclusive writer lock");
        let (decision, next_cursor) = result.unwrap();
        assert!(decision.is_none());
        assert_eq!(next_cursor, cursor);
        handle.join().unwrap();
    }

    #[test]
    fn find_latest_ask_user_response_reads_latest_matching_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-ask-user").unwrap();

        writer
            .append(&JournalEvent::ask_user_prompted(
                Some("sess-ask-user"),
                Some(5),
                "ask-1",
                Some("run-1"),
                serde_json::json!({"questions": []}),
            ))
            .unwrap();
        writer
            .append(&JournalEvent::ask_user_response(
                Some("sess-ask-user"),
                Some(5),
                "ask-1",
                Some("run-1"),
                "submitted",
                Some(serde_json::json!({
                    "answers": [{
                        "question": "Continue?",
                        "answers": ["yes"],
                        "multi_select": false
                    }]
                })),
            ))
            .unwrap();

        let found = find_latest_ask_user_response("sess-ask-user", "ask-1")
            .unwrap()
            .expect("ask_user response");
        assert_eq!(found.request_id, "ask-1");
        assert_eq!(found.run_id.as_deref(), Some("run-1"));
        assert_eq!(found.status, "submitted");
        assert_eq!(
            found.answers.unwrap()["answers"][0]["answers"][0].as_str(),
            Some("yes")
        );
    }

    #[test]
    fn ask_user_prompt_lookup_and_terminal_append_are_run_scoped_and_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-ask-user-atomic").unwrap();
        let prompt = serde_json::json!({
            "questions": [{"question": "Continue?", "options": []}]
        });
        writer
            .append(&JournalEvent::ask_user_prompted(
                Some("sess-ask-user-atomic"),
                Some(7),
                "ask-atomic",
                Some("run-a"),
                prompt.clone(),
            ))
            .unwrap();

        let required =
            find_latest_ask_user_prompted_for_run("sess-ask-user-atomic", "ask-atomic", "run-a")
                .unwrap()
                .expect("canonical ask_user request");
        assert_eq!(required.turn, Some(7));
        assert_eq!(required.prompt, prompt);
        assert!(
            find_latest_ask_user_prompted_for_run("sess-ask-user-atomic", "ask-atomic", "run-b",)
                .unwrap()
                .is_none(),
            "a request id may not cross run boundaries"
        );

        let answers = serde_json::json!({"answers": []});
        assert_eq!(
            append_ask_user_response_for_run_if_absent(
                "sess-ask-user-atomic",
                required.turn,
                "ask-atomic",
                "run-a",
                "submitted",
                Some(answers.clone()),
            )
            .unwrap(),
            AskUserResponseAppendOutcome::Appended
        );
        assert_eq!(
            append_ask_user_response_for_run_if_absent(
                "sess-ask-user-atomic",
                required.turn,
                "ask-atomic",
                "run-a",
                "submitted",
                Some(answers),
            )
            .unwrap(),
            AskUserResponseAppendOutcome::Idempotent
        );
        let conflict = append_ask_user_response_for_run_if_absent(
            "sess-ask-user-atomic",
            required.turn,
            "ask-atomic",
            "run-a",
            "timeout",
            None,
        )
        .unwrap();
        assert!(matches!(
            conflict,
            AskUserResponseAppendOutcome::Conflict(AskUserJournalResponse { status, .. })
                if status == "submitted"
        ));

        let terminal_events = read_journal("sess-ask-user-atomic")
            .unwrap()
            .into_iter()
            .filter(|event| event.event_type == JournalEventType::AskUserResponse)
            .count();
        assert_eq!(terminal_events, 1);
    }

    #[test]
    fn ask_user_response_projects_to_run_scoped_interaction_contract() {
        let response = AskUserJournalResponse {
            request_id: "ask-contract".into(),
            run_id: Some("run-contract".into()),
            status: "cancelled".into(),
            answers: None,
        };

        let contract = response
            .interaction_contract("sess-contract", Some("user-contract"))
            .expect("run-scoped ask_user response should become an interaction contract");

        assert_eq!(contract.kind, InteractionKind::UserPrompt);
        assert_eq!(
            contract.durable_store,
            InteractionDurableStore::SessionJournal
        );
        assert_eq!(contract.status, InteractionStatus::Cancelled);
        assert_eq!(contract.identity.user_id.as_deref(), Some("user-contract"));
        assert_eq!(contract.identity.session_id, "sess-contract");
        assert_eq!(contract.identity.run_id, "run-contract");
        assert_eq!(contract.identity.request_id, "ask-contract");
        assert!(contract.is_wait_satisfied());
    }

    #[test]
    fn ask_user_response_lookup_for_run_ignores_other_run_same_request_id() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-ask-user-run").unwrap();

        writer
            .append(&JournalEvent::ask_user_response(
                Some("sess-ask-user-run"),
                Some(1),
                "ask-shared",
                Some("run-a"),
                "submitted",
                Some(serde_json::json!({"answers": [{"question": "A?", "answers": ["a"]}]})),
            ))
            .unwrap();
        writer
            .append(&JournalEvent::ask_user_response(
                Some("sess-ask-user-run"),
                Some(2),
                "ask-shared",
                Some("run-b"),
                "cancelled",
                None,
            ))
            .unwrap();

        let found =
            find_latest_ask_user_response_for_run("sess-ask-user-run", "ask-shared", "run-a")
                .unwrap()
                .expect("run-a ask_user response");
        assert_eq!(found.run_id.as_deref(), Some("run-a"));
        assert_eq!(found.status, "submitted");
        assert_eq!(
            found.answers.unwrap()["answers"][0]["answers"][0].as_str(),
            Some("a")
        );

        let cancelled =
            find_latest_ask_user_response_for_run("sess-ask-user-run", "ask-shared", "run-b")
                .unwrap()
                .expect("run-b ask_user response");
        assert_eq!(cancelled.run_id.as_deref(), Some("run-b"));
        assert_eq!(cancelled.status, "cancelled");
        assert!(cancelled.answers.is_none());
    }

    #[test]
    fn permission_audit_event_round_trips_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-permission-audit").unwrap();

        writer
            .append(&JournalEvent::permission_audit(
                Some("sess-permission-audit"),
                Some(3),
                serde_json::json!({
                    "kind": "evaluated",
                    "correlation_id": "perm-1",
                    "decision": "need_external",
                }),
            ))
            .unwrap();

        let events = read_journal("sess-permission-audit").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, JournalEventType::SessionStart);
        assert_eq!(events[1].event_type, JournalEventType::PermissionAudit);
        assert_eq!(events[1].turn, Some(3));
        assert_eq!(
            events[1]
                .metadata
                .as_ref()
                .and_then(|value| value.get("kind"))
                .and_then(serde_json::Value::as_str),
            Some("evaluated")
        );
    }

    #[test]
    fn execution_boundary_events_round_trip() {
        let opened = JournalEvent::execution_boundary_opened(
            Some("sess-boundary"),
            7,
            "tool_batch",
            Some("tx-7"),
            serde_json::json!({
                "file_after_sequence": 3,
                "database_after_sequence": 1,
            }),
        );
        let committed = JournalEvent::execution_boundary_committed(
            Some("sess-boundary"),
            7,
            "tool_batch",
            Some("tx-7"),
            Some(serde_json::json!({
                "completed_request_id": "tr-2",
            })),
        );
        let aborted = JournalEvent::execution_boundary_aborted(
            Some("sess-boundary"),
            7,
            "turn_rollback",
            None,
            "tool failed",
            Some("write_file"),
            Some("tr-3"),
            Some(serde_json::json!({
                "summary": "Rolled back 1 file edit from turn 7",
            })),
        );

        let opened_json = serde_json::to_string(&opened).unwrap();
        let committed_json = serde_json::to_string(&committed).unwrap();
        let aborted_json = serde_json::to_string(&aborted).unwrap();

        let restored_opened: JournalEvent = serde_json::from_str(&opened_json).unwrap();
        let restored_committed: JournalEvent = serde_json::from_str(&committed_json).unwrap();
        let restored_aborted: JournalEvent = serde_json::from_str(&aborted_json).unwrap();

        assert_eq!(
            restored_opened.event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            restored_committed.event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );
        assert_eq!(
            restored_aborted.event_type,
            JournalEventType::ExecutionBoundaryAborted
        );
        assert_eq!(restored_opened.turn, Some(7));
        assert_eq!(
            restored_opened
                .metadata
                .as_ref()
                .and_then(|m| m.get("execution_boundary"))
                .and_then(|m| m.get("transaction_id"))
                .and_then(serde_json::Value::as_str),
            Some("tx-7")
        );
        assert_eq!(
            restored_aborted
                .metadata
                .as_ref()
                .and_then(|m| m.get("execution_boundary"))
                .and_then(|m| m.get("trigger_tool_name"))
                .and_then(serde_json::Value::as_str),
            Some("write_file")
        );
    }

    #[test]
    fn context_assembly_recorded_carries_metadata_summary() {
        let evt = JournalEvent::context_assembly_recorded(
            Some("sess"),
            3,
            serde_json::json!({
                "turn_id": "turn-3",
                "tools": {"visible_tools": [{"tool_name": "read_file"}]},
                "token_budget": {"total_used": 1234}
            }),
        );

        assert!(evt.context_assembly_trace.is_some());
        let metadata = evt.metadata.as_ref().expect("context metadata");
        assert_eq!(metadata["trace_recorded"], true);
        assert_eq!(metadata["turn_id"], "turn-3");
        assert_eq!(metadata["tool_count"], 1);
        assert_eq!(metadata["total_tokens"], 1234);
    }

    #[test]
    fn agent_terminated_omits_unknown_turn_count_instead_of_fabricating_zero() {
        let event = JournalEvent::agent_terminated(
            Some("sess"),
            "agent-1",
            "run-1",
            "general-purpose",
            "interrupted",
            Some("execution_incomplete"),
            None,
            2,
            100,
            20,
            500,
            None,
        );
        let metadata = event.metadata.expect("agent metadata");

        assert!(metadata.get("turns_completed").is_none());
        assert_eq!(metadata["tool_calls"], 2);
        assert_eq!(metadata["finish_reason"], "execution_incomplete");
    }
}

/// Testable version that operates on an explicit directory.
fn run_session_maintenance_in(
    dir: PathBuf,
    ttl_days: u64,
    compress_after_days: u64,
) -> SessionMaintenanceResult {
    use std::time::{Duration, SystemTime};

    let now = SystemTime::now();
    let ttl_threshold = now
        .checked_sub(Duration::from_secs(ttl_days * 86400))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let compress_threshold = now
        .checked_sub(Duration::from_secs(compress_after_days * 86400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut result = SessionMaintenanceResult::default();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            result.errors.push(format!("read_dir failed: {e}"));
            return result;
        }
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Only process .jsonl files (active journals)
        let session_id = match name_str.strip_suffix(".jsonl") {
            Some(sid) => sid.to_string(),
            None => continue,
        };
        // Skip .jsonl.gz — already compressed
        if name_str.ends_with(".jsonl.gz") {
            continue;
        }

        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };

        if mtime < ttl_threshold {
            // Session expired: delete journal + session directory
            let freed = delete_session_files(&dir, &session_id);
            result.bytes_freed += freed;
            result.sessions_deleted += 1;
        } else if mtime < compress_threshold {
            // Journal old enough to compress
            match compress_journal(&dir, &session_id) {
                Ok(()) => result.journals_compressed += 1,
                Err(e) => result.errors.push(format!("compress {session_id}: {e}")),
            }
        }
    }

    // Also clean up orphaned .jsonl.gz files past TTL
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(sid) = name_str.strip_suffix(".jsonl.gz") {
                let mtime = match entry.metadata().and_then(|m| m.modified()) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if mtime < ttl_threshold {
                    let freed = delete_session_files(&dir, sid);
                    result.bytes_freed += freed;
                    result.sessions_deleted += 1;
                }
            }
        }
    }

    result
}

/// Delete all files for a session: .jsonl, .jsonl.gz, and the session directory.
/// Returns the total bytes freed.
fn delete_session_files(sessions_dir: &Path, session_id: &str) -> u64 {
    let mut freed: u64 = 0;
    // Journal file (.jsonl)
    let journal = sessions_dir.join(format!("{session_id}.jsonl"));
    if let Ok(meta) = journal.metadata() {
        freed += meta.len();
        let _ = std::fs::remove_file(&journal);
    }
    // Compressed journal (.jsonl.gz)
    let gz = sessions_dir.join(format!("{session_id}.jsonl.gz"));
    if let Ok(meta) = gz.metadata() {
        freed += meta.len();
        let _ = std::fs::remove_file(&gz);
    }
    // Session directory (checkpoints, workspace, tool results, etc.)
    let session_dir = sessions_dir.join(session_id);
    if session_dir.is_dir() {
        if let Ok(size) = dir_size(&session_dir) {
            freed += size;
        }
        let _ = std::fs::remove_dir_all(&session_dir);
    }
    freed
}

/// Compress a .jsonl file to .jsonl.gz using gzip, then remove the original.
fn compress_journal(sessions_dir: &Path, session_id: &str) -> std::io::Result<()> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::{BufRead, BufReader, Write};

    let src = sessions_dir.join(format!("{session_id}.jsonl"));
    let dst = sessions_dir.join(format!("{session_id}.jsonl.gz"));

    // Don't re-compress if .gz already exists
    if dst.exists() {
        let _ = std::fs::remove_file(&src);
        return Ok(());
    }

    let reader = BufReader::new(std::fs::File::open(&src)?);
    let file = std::fs::File::create(&dst)?;
    let mut encoder = GzEncoder::new(file, Compression::default());

    for line in reader.lines() {
        let line = line?;
        encoder.write_all(line.as_bytes())?;
        encoder.write_all(b"\n")?;
    }
    let out_file = encoder.finish()?;
    // Ensure compressed data is durable before deleting the original.
    out_file.sync_all()?;

    // Remove original after successful compression
    std::fs::remove_file(&src)?;
    Ok(())
}

/// Recursively compute total size of a directory tree.
fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_file() {
                total += entry.metadata()?.len();
            } else if ft.is_dir() {
                total += dir_size(&entry.path())?;
            }
        }
    }
    Ok(total)
}

// ═══════════════════════════════════════════════════════════ Tests ═════
#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::{DriftCause, DriftEvidence, EvidenceType};
    use tempfile::tempdir;

    const REAL_SESSION_0AC769_FIXTURE: &str =
        include_str!("../fixtures/real_session_0ac769_min.jsonl");

    #[test]
    fn trace_span_exposes_indexable_milliseconds_without_losing_microseconds() {
        let event = TraceSpanBuilder::default()
            .span_id("phase-1".to_string())
            .name("model_inference".to_string())
            .start_us(1_500)
            .end_us(12_734)
            .build();
        let metadata = event.metadata.expect("trace span metadata");

        assert_eq!(metadata["duration_us"], serde_json::json!(11_234));
        assert_eq!(metadata["duration_ms"], serde_json::json!(11));
    }

    fn base_tool_record(name: &str, ok: bool, preview: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: preview.map(ToString::to_string),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    #[test]
    fn real_session_fixture_parses_with_expected_rounds_and_repeat_signals() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        let path = journal_file_path(sid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, REAL_SESSION_0AC769_FIXTURE).unwrap();

        let (events, non_empty_lines, malformed_lines) = read_journal_for_digest(sid).unwrap();
        assert_eq!(non_empty_lines, 14);
        assert_eq!(malformed_lines, 0);
        assert_eq!(events.len(), 14);

        let llm_rounds: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::LlmRound)
            .collect();
        assert_eq!(
            llm_rounds.len(),
            7,
            "fixture should preserve the 7-round loop"
        );

        let turn = events
            .iter()
            .find(|event| event.event_type == JournalEventType::Turn)
            .expect("turn event");
        assert_eq!(
            turn.user_input.as_deref(),
            Some("review b273c589a73799070a71f4cfc6d55349b534d8d1")
        );
        assert!(
            turn.assistant_output
                .as_deref()
                .unwrap_or("")
                .contains("not b273c589"),
            "fixture should preserve the wrong-prefetch symptom"
        );

        let eval = events
            .iter()
            .find(|event| event.event_type == JournalEventType::TurnEvaluation)
            .expect("turn_evaluation event");
        let metadata = eval.metadata.as_ref().expect("turn evaluation metadata");
        assert_eq!(metadata["tool_call_count"], 12);
        assert_eq!(metadata["signal_count"], 4);
        assert_eq!(metadata["quality"], 0.5);
        assert_eq!(metadata["confidence"], 0.7);

        let signals = metadata["signals"].as_array().expect("signals array");
        let repeat_tools: std::collections::BTreeSet<_> = signals
            .iter()
            .filter(|signal| signal["kind"].as_str() == Some("repeat_tool_call"))
            .filter_map(|signal| signal["tool"].as_str())
            .collect();
        assert_eq!(
            repeat_tools,
            std::collections::BTreeSet::from(["git", "read_file"])
        );
    }

    #[test]
    fn journal_dir_guard_overrides_local_sessions_dir_nested() {
        let outer = tempdir().unwrap();
        let inner = tempdir().unwrap();
        let outer_sessions = outer.path().join("sessions");
        let inner_sessions = inner.path().join("sessions");
        std::fs::create_dir_all(&outer_sessions).unwrap();
        std::fs::create_dir_all(&inner_sessions).unwrap();

        let _g1 = JournalDirGuard::new(&outer_sessions);
        assert_eq!(local_sessions_dir(), outer_sessions);
        {
            let _g2 = JournalDirGuard::new(&inner_sessions);
            assert_eq!(local_sessions_dir(), inner_sessions);
        }
        assert_eq!(local_sessions_dir(), outer_sessions);
    }

    #[test]
    fn cargo_test_processes_default_to_target_scoped_state() {
        let executable =
            Path::new("/workspace/target/debug/deps/session_journal_tests-0123456789abcdef");
        assert_eq!(
            cargo_test_process_sessions_dir_for(executable, 42),
            Some(PathBuf::from(
                "/workspace/target/test-state/session_journal_tests-0123456789abcdef-42/sessions"
            ))
        );
    }

    #[test]
    fn production_and_unhashed_debug_binaries_keep_the_real_state_contract() {
        assert_eq!(
            cargo_test_process_sessions_dir_for(Path::new("/workspace/target/debug/astra"), 42),
            None
        );
        assert_eq!(
            cargo_test_process_sessions_dir_for(
                Path::new("/workspace/target/debug/deps/astra-integration"),
                42,
            ),
            None
        );
    }

    #[test]
    #[serial_test::serial(process_journal_dir_guard)]
    fn process_journal_dir_guard_interleaved_drop_uses_guard_identity() {
        let shared = tempdir().unwrap();
        let shared_sessions = shared.path().join("sessions");
        std::fs::create_dir_all(&shared_sessions).unwrap();

        let outer = ProcessJournalDirGuard::new(&shared_sessions);
        let outer_id = outer.id;
        let inner = ProcessJournalDirGuard::new(&shared_sessions);
        let inner_id = inner.id;

        assert_ne!(outer_id, inner_id);
        assert_eq!(local_sessions_dir(), shared_sessions);

        drop(outer);
        {
            let overrides = PROCESS_SESSIONS_DIR_OVERRIDES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(
                !overrides.iter().any(|override_| override_.id == outer_id),
                "dropping the outer guard must remove the outer override, not the same-path inner override"
            );
            assert!(
                overrides.iter().any(|override_| override_.id == inner_id),
                "the same-path inner override must remain active after interleaved outer drop"
            );
        }
        assert_eq!(local_sessions_dir(), shared_sessions);

        drop(inner);
        let overrides = PROCESS_SESSIONS_DIR_OVERRIDES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            !overrides
                .iter()
                .any(|override_| { override_.id == outer_id || override_.id == inner_id })
        );
    }

    #[test]
    fn journal_event_session_start_serializes() {
        let evt = JournalEvent::session_start(Some("sess-1"), Some("gpt-4"));
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"session_start\""));
        assert!(json.contains("\"session_id\":\"sess-1\""));
        assert!(json.contains("\"model\":\"gpt-4\""));
        // Shouldn't have null fields
        assert!(!json.contains("\"turn\""));
    }

    #[test]
    fn journal_event_plan_edit_serializes_and_round_trips() {
        let meta = serde_json::json!({"subtask_count": 2});
        let evt = JournalEvent::plan_edit(Some("sid-plan"), "Plan edited: add step", Some(meta));
        assert_eq!(evt.event_type, JournalEventType::PlanEdit);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"plan_edit\""));
        assert!(json.contains("Plan edited"));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::PlanEdit);
        let m = parsed.metadata.expect("metadata");
        assert_eq!(
            m.get("action").and_then(|v| v.as_str()),
            Some("Plan edited: add step")
        );
    }

    #[test]
    fn journal_event_plan_lifecycle_serializes_and_round_trips() {
        let detail = serde_json::json!({"mode": "auto", "subtask_count": 3});
        let evt =
            JournalEvent::plan_lifecycle(Some("sid-lc"), "Plan execution started", Some(detail));
        assert_eq!(evt.event_type, JournalEventType::PlanLifecycle);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"plan_lifecycle\""));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::PlanLifecycle);
        let m = parsed.metadata.expect("metadata");
        assert_eq!(
            m.get("summary").and_then(|v| v.as_str()),
            Some("Plan execution started")
        );
    }

    #[test]
    fn journal_event_goal_steered_serializes_and_round_trips() {
        let evt = JournalEvent::goal_steered(
            Some("sid-goal"),
            4,
            "control_plane:goal",
            Some("old goal"),
            "new goal",
            Some(serde_json::json!({"mode": "manual"})),
        );
        assert_eq!(evt.event_type, JournalEventType::GoalSteered);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"goal_steered\""));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::GoalSteered);
        assert_eq!(parsed.turn, Some(4));
        let metadata = parsed.metadata.expect("metadata");
        assert_eq!(
            metadata.get("source").and_then(|value| value.as_str()),
            Some("control_plane:goal")
        );
        assert_eq!(
            metadata
                .get("previous_goal")
                .and_then(|value| value.as_str()),
            Some("old goal")
        );
        assert_eq!(
            metadata.get("new_goal").and_then(|value| value.as_str()),
            Some("new goal")
        );
    }

    #[test]
    fn journal_event_drift_detected_round_trips_structured_cause_and_evidence() {
        let evt = JournalEvent::drift_detected(
            Some("sid-drift"),
            7,
            0.75,
            DriftCause::MemoryMiss {
                expected_but_not_retrieved: vec!["session history".into(), "repo context".into()],
                query_used: "debug repeated session start".into(),
            },
            vec![DriftEvidence {
                turn: 6,
                evidence_type: EvidenceType::MemoryMismatch,
                description: "Retrieved unrelated CI memories instead of resume context".into(),
                confidence: 0.9.into(),
            }],
            "Re-query with explicit session-resume terms",
        );

        assert_eq!(evt.event_type, JournalEventType::DriftDetected);
        assert_eq!(evt.turn, Some(7));
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        let meta = parsed.metadata.expect("metadata");

        assert_eq!(meta.get("severity").and_then(|v| v.as_f64()), Some(0.75));
        assert_eq!(meta.get("evidence_count").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(
            meta.get("recovery_suggestion").and_then(|v| v.as_str()),
            Some("Re-query with explicit session-resume terms")
        );
        assert_eq!(
            meta.get("cause")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str()),
            Some("MemoryMiss")
        );
        let evidence = meta
            .get("evidence")
            .and_then(|v| v.as_array())
            .expect("evidence array");
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].get("turn").and_then(|v| v.as_u64()), Some(6));
        assert_eq!(
            evidence[0].get("evidence_type").and_then(|v| v.as_str()),
            Some("MemoryMismatch")
        );
    }

    #[test]
    fn journal_event_session_fork_round_trip() {
        let lineage = SessionLineage {
            parent_session_id: "parent-uuid".into(),
            forked_after_turn: Some(3),
            label: Some("try plan B".into()),
        };
        let evt = JournalEvent::session_fork(Some("child-uuid"), lineage, Some("note"));
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"session_fork\""));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::SessionFork);
        assert!(parsed.session_lineage.is_some());
    }

    #[test]
    fn read_journal_edge_cases() {
        // --- tail returns only last n events ---
        {
            let tmp = tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = "00000000-0000-0000-0000-000000000190";
            let writer = JournalWriter::new(sid).expect("journal writer");
            for turn in 1..=5 {
                writer
                    .append(&JournalEvent::turn(
                        Some(sid),
                        turn,
                        Some("test-model"),
                        "user",
                        "assistant",
                        0,
                        0,
                        0,
                        0,
                    ))
                    .expect("append turn");
            }
            let tail = read_journal_tail(sid, 2).expect("read tail");
            let turns: Vec<u32> = tail.iter().filter_map(|event| event.turn).collect();
            assert_eq!(turns, vec![4, 5]);
        }

        // --- ignores truncated final JSON line ---
        {
            let tmp = tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = "00000000-0000-0000-0000-000000000191";
            let event = JournalEvent::turn(
                Some(sid),
                1,
                Some("test-model"),
                "user",
                "assistant",
                0,
                0,
                0,
                0,
            );
            let valid = serde_json::to_string(&event).unwrap();
            let path = journal_file_path(sid);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{valid}\n{{\"type\":\"turn\",\"turn\":")).unwrap();
            let events = read_journal(sid).expect("truncated tail should not poison journal");
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].turn, Some(1));
            let (_events, non_empty_lines, malformed_lines) =
                read_journal_for_digest(sid).expect("digest read should count malformed tail");
            assert_eq!(non_empty_lines, 2);
            assert_eq!(malformed_lines, 1);
        }
    }

    #[test]
    fn read_journal_tail_returns_exact_events_beyond_recovery_byte_budget() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "00000000-0000-0000-0000-000000000199";
        let payload = "x".repeat(512);
        let mut jsonl = String::new();
        for turn in 1..=300 {
            let event = JournalEvent::turn(
                Some(sid),
                turn,
                Some("test-model"),
                &payload,
                &payload,
                0,
                0,
                0,
                0,
            );
            jsonl.push_str(&serde_json::to_string(&event).unwrap());
            jsonl.push('\n');
        }
        let path = journal_file_path(sid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, jsonl).unwrap();

        let tail = read_journal_tail(sid, 200).unwrap();
        let turns = tail
            .iter()
            .filter_map(|event| event.turn)
            .collect::<Vec<_>>();
        assert_eq!(turns.len(), 200);
        assert_eq!(turns.first(), Some(&101));
        assert_eq!(turns.last(), Some(&300));
    }

    #[test]
    fn journal_event_cloud_pull_sync_marker_round_trip() {
        let keys = vec!["explain_mode".to_string()];
        let evt = JournalEvent::cloud_pull_sync_marker(
            Some("sid-1"),
            "work",
            "repl_startup",
            &keys,
            false,
        );
        assert_eq!(evt.event_type, JournalEventType::SyncMarker);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"sync_marker\""));
        assert!(json.contains("\"cloud_pull\""));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::SyncMarker);
        let meta = parsed.metadata.expect("metadata");
        let cp = meta.get("cloud_pull").expect("cloud_pull");
        assert_eq!(cp.get("profile").and_then(|v| v.as_str()), Some("work"));
        assert_eq!(
            cp.get("source").and_then(|v| v.as_str()),
            Some("repl_startup")
        );
        let pref = cp
            .get("preference_keys_merged")
            .and_then(|v| v.as_array())
            .expect("prefs array");
        assert_eq!(pref.len(), 1);
        assert_eq!(pref[0].as_str(), Some("explain_mode"));
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn journal_event_cloud_pull_sync_marker_empty_ack_round_trip() {
        let evt = JournalEvent::cloud_pull_sync_marker(
            Some("s-empty"),
            "default",
            "post_login",
            &[],
            true,
        );
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"reachable_empty_ack\":true"));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        let cp = parsed
            .metadata
            .as_ref()
            .and_then(|m| m.get("cloud_pull"))
            .unwrap();
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn cloud_pull_sync_marker_append_to_journal_file() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let sid = format!("test-cloud-pull-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&sid).unwrap();
        let evt =
            JournalEvent::cloud_pull_sync_marker(Some(&sid), "default", "post_login", &[], true);
        writer.append(&evt).unwrap();
        let events = read_journal(&sid).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, JournalEventType::SessionStart);
        assert_eq!(events[1].event_type, JournalEventType::SyncMarker);
        let cp = events[1]
            .metadata
            .as_ref()
            .and_then(|m| m.get("cloud_pull"))
            .expect("cloud_pull");
        assert_eq!(
            cp.get("source").and_then(|v| v.as_str()),
            Some("post_login")
        );
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(true)
        );
        std::fs::remove_file(writer.path()).ok();
    }

    #[test]
    fn journal_event_turn_round_trip() {
        let evt = JournalEvent::turn(
            Some("sess-2"),
            3,
            Some("claude"),
            "hello",
            "world",
            2,
            100,
            50,
            1234,
        );
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::Turn);
        assert_eq!(parsed.turn, Some(3));
        assert_eq!(parsed.tool_count, Some(2));
        assert_eq!(parsed.tokens_in, Some(100));
        assert_eq!(parsed.tokens_out, Some(50));
        assert_eq!(parsed.duration_ms, Some(1234));
    }

    #[test]
    fn journal_writer_and_basic_events() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".astra").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();

        // Writer creates file with newline-terminated JSON
        let write_path = dir.join("test-sess.jsonl");
        let writer = JournalWriter {
            path: write_path.clone(),
            owner_scope: OwnerScope::local_user(),
            session_id: "test-sess".to_string(),
            created_parent_dirs: Vec::new(),
        };
        writer
            .append(&JournalEvent::session_start(Some("test-sess"), None))
            .unwrap();
        let content = std::fs::read_to_string(&write_path).unwrap();
        assert!(content.contains("session_start"));
        assert!(content.ends_with('\n'));

        // Writer appends multiple events
        let multi_path = dir.join("multi.jsonl");
        let writer = JournalWriter {
            path: multi_path.clone(),
            owner_scope: OwnerScope::local_user(),
            session_id: "multi".to_string(),
            created_parent_dirs: Vec::new(),
        };
        writer
            .append(&JournalEvent::session_start(Some("m"), None))
            .unwrap();
        writer
            .append(&JournalEvent::config_change(Some("m"), "model", "x"))
            .unwrap();
        writer
            .append(&JournalEvent::session_end(Some("m"), 5))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&multi_path)
                .unwrap()
                .lines()
                .count(),
            3
        );

        // Compact events
        let c = JournalEvent::compact(Some("s"), 5, 10, 3);
        assert_eq!(c.event_type, JournalEventType::Compact);
        assert_eq!(c.turns_compacted, Some(10));
        assert_eq!(c.facts_stored, Some(3));
        assert!(c.metadata.is_none());

        let cs = JournalEvent::compact_with_summary(
            Some("s"),
            5,
            10,
            3,
            Some("User worked on fixing auth bugs"),
        );
        assert_eq!(
            cs.metadata.unwrap()["compact_summary"],
            "User worked on fixing auth bugs"
        );

        // Compact with empty summary omits metadata
        assert!(
            JournalEvent::compact_with_summary(Some("s"), 5, 10, 3, Some(""))
                .metadata
                .is_none()
        );
        assert!(
            JournalEvent::compact_with_summary(Some("s"), 5, 10, 3, None)
                .metadata
                .is_none()
        );

        // Config change
        let cc = JournalEvent::config_change(Some("s"), "model", "gpt-4o");
        assert_eq!(cc.event_type, JournalEventType::ConfigChange);
        assert_eq!(cc.config_key.as_deref(), Some("model"));
        assert_eq!(cc.config_value.as_deref(), Some("gpt-4o"));

        // Error event
        let err = JournalEvent::error(Some("s"), "connection refused");
        assert_eq!(err.event_type, JournalEventType::Error);
        assert_eq!(err.error.as_deref(), Some("connection refused"));

        // Truncate helpers
        assert_eq!(truncate("hello", 10), "hello");
        let long = "a".repeat(600);
        let t = truncate(&long, 500);
        assert!(t.len() <= 504);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn journal_event_skip_none_fields() {
        let evt = JournalEvent::session_start(None, None);
        let json = serde_json::to_string(&evt).unwrap();
        // Should NOT contain null session_id or model
        assert!(!json.contains("\"session_id\""));
        assert!(!json.contains("\"model\""));
        assert!(!json.contains("\"turn\""));
    }

    // ── tool surface tracking (p5g observability + p6e feedback) ──

    #[test]
    fn turn_event_with_tool_surface_round_trip() {
        let evt = JournalEvent::turn(
            Some("s1"),
            1,
            Some("gpt-4"),
            "最新的pr?",
            "Here are the PRs...",
            2,
            500,
            200,
            1234,
        )
        .with_tool_surface(
            vec![
                "bash".into(),
                " github ".into(),
                "github".into(),
                "".into(),
                "read_file".into(),
            ],
            vec![
                " tune-performance ".into(),
                "tune-performance".into(),
                " ".into(),
            ],
            vec![" github ".into(), "github".into(), "".into()],
            45,
        )
        .with_budget_pressure(0.6);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.visible_tools.as_ref().unwrap(),
            &vec![
                "bash".to_string(),
                "github".to_string(),
                "read_file".to_string()
            ]
        );
        assert_eq!(
            parsed.selected_skills.as_ref().unwrap(),
            &["tune-performance"]
        );
        assert_eq!(parsed.tools_used.as_ref().unwrap(), &["github"]);
        assert_eq!(parsed.budget_used, Some(45));
        assert_eq!(parsed.budget_pressure, Some(0.6));
    }

    #[test]
    fn turn_event_without_tool_surface_omits_fields() {
        let evt = JournalEvent::turn(Some("s2"), 1, None, "hello", "world", 0, 10, 5, 100);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(
            !json.contains("visible_tools"),
            "should omit None fields: {json}"
        );
        assert!(
            !json.contains("tools_used"),
            "should omit None fields: {json}"
        );
        assert!(
            !json.contains("budget_used"),
            "should omit None fields: {json}"
        );
        assert!(
            !json.contains("budget_pressure"),
            "should omit None fields: {json}"
        );
    }

    #[test]
    fn journal_write_read_with_selection_data() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".astra").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sel-test.jsonl");
        let writer = JournalWriter {
            path: path.clone(),
            owner_scope: OwnerScope::local_user(),
            session_id: "sel-test".to_string(),
            created_parent_dirs: Vec::new(),
        };

        writer
            .append(&JournalEvent::session_start(
                Some("sel-test"),
                Some("gpt-4"),
            ))
            .unwrap();
        writer
            .append(
                &JournalEvent::turn(
                    Some("sel-test"),
                    1,
                    Some("gpt-4"),
                    "pr?",
                    "...",
                    1,
                    100,
                    50,
                    500,
                )
                .with_tool_surface(
                    vec!["bash".into(), "github".into()],
                    vec![],
                    vec!["github".into()],
                    35,
                )
                .with_budget_pressure(0.3),
            )
            .unwrap();
        writer
            .append(&JournalEvent::session_end(Some("sel-test"), 1))
            .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let events: Vec<JournalEvent> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(events.len(), 3);

        // Verify the turn event has selection data
        let turn = &events[1];
        assert_eq!(turn.event_type, JournalEventType::Turn);
        assert_eq!(turn.visible_tools.as_ref().unwrap().len(), 2);
        assert!(turn.selected_skills.is_none());
        assert_eq!(turn.tools_used.as_ref().unwrap(), &["github"]);
        assert_eq!(turn.budget_used, Some(35));
        assert_eq!(turn.budget_pressure, Some(0.3));
    }

    #[test]
    fn with_run_id_preserves_existing_metadata() {
        let evt = JournalEvent::turn(Some("s2"), 1, None, "hello", "world", 0, 10, 5, 100)
            .with_budget_pressure(0.4)
            .with_run_id(Some("run-22"));
        let metadata = evt.metadata.as_ref().expect("metadata");
        assert_eq!(metadata["run_id"], "run-22");
        assert_eq!(evt.budget_pressure, Some(0.4));
    }

    #[test]
    fn turn_event_with_tool_calls_round_trip() {
        let evt = JournalEvent::turn(
            Some("s1"),
            3,
            Some("gpt-4"),
            "pr呢？",
            "Here are PRs...",
            1,
            300,
            150,
            800,
        )
        .with_tool_surface(vec!["github".into()], vec![], vec!["github".into()], 20)
        .with_tool_calls(vec![
            ToolCallRecord {
                name: " github ".into(),
                ok: true,
                ms: 761,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: Some("owner/repo".into()),
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: Some(" github ".into()),
                ..Default::default()
            },
            ToolCallRecord {
                name: " ".into(),
                ok: false,
                ms: 1,
                error: Some("blank tool name".into()),
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: Some(" ".into()),
                ..Default::default()
            },
        ]);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        let calls = parsed.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "github");
        assert_eq!(calls[0].original_tool_name.as_deref(), Some("github"));
        assert!(calls[0].ok);
        assert_eq!(calls[0].ms, 761);
    }

    #[test]
    fn with_tool_calls_empty_omits_field() {
        let evt = JournalEvent::turn(Some("s1"), 1, None, "hi", "hello", 0, 10, 5, 50)
            .with_tool_calls(vec![]);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(
            !json.contains("tool_calls"),
            "empty tool_calls should be omitted: {json}"
        );
        assert!(!json.contains("tool_outcomes"), "{json}");
    }

    #[test]
    fn with_tool_calls_empty_preserves_authoritative_aggregate_count() {
        let evt = JournalEvent::turn(Some("s1"), 1, None, "hi", "hello", 7, 10, 5, 50)
            .with_tool_calls(vec![]);

        assert_eq!(evt.tool_count, Some(7));
        assert!(evt.tool_calls.is_none());
        assert!(evt.tool_outcomes.is_none());
    }

    #[test]
    fn with_tool_calls_derives_tool_count_from_material_normalized_records() {
        let evt = JournalEvent::turn(Some("s1"), 1, None, "hi", "hello", 99, 10, 5, 50)
            .with_tool_calls(vec![
                base_tool_record(" bash ", true, Some("ok")),
                ToolCallRecord {
                    name: "skill".into(),
                    ok: true,
                    ms: 1,
                    surgically_removed: Some(true),
                    ..Default::default()
                },
                ToolCallRecord {
                    name: " ".into(),
                    ok: true,
                    ms: 1,
                    ..Default::default()
                },
            ]);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.tool_count, Some(1));
        let outcomes = parsed.tool_outcomes.expect("derived outcome summary");
        assert_eq!(outcomes.requested, 2);
        assert_eq!(outcomes.executed, 1);
        assert_eq!(outcomes.succeeded, 1);
        assert_eq!(outcomes.suppressed, 1);
        assert!(outcomes.is_consistent());
        let calls = parsed.tool_calls.expect("audit records should be retained");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[1].surgically_removed, Some(true));
    }

    #[test]
    fn tool_outcome_summary_is_mutually_exclusive_and_roundtrips_error_kind() {
        let records = vec![
            ToolCallRecord {
                name: "read_file".into(),
                ok: true,
                ms: 3,
                disposition: Some(ToolCallDisposition::Executed),
                ..Default::default()
            },
            ToolCallRecord {
                name: "git".into(),
                ok: false,
                ms: 4,
                error_kind: Some(astra_core::ErrorKind::ToolInvalidArgs),
                disposition: Some(ToolCallDisposition::Executed),
                ..Default::default()
            },
            ToolCallRecord {
                name: "bash".into(),
                ok: false,
                ms: 0,
                disposition: Some(ToolCallDisposition::Rejected),
                ..Default::default()
            },
            ToolCallRecord {
                name: "read_file".into(),
                ok: true,
                ms: 0,
                disposition: Some(ToolCallDisposition::Reused),
                ..Default::default()
            },
            ToolCallRecord {
                name: "grep".into(),
                ok: true,
                ms: 0,
                disposition: Some(ToolCallDisposition::Suppressed),
                ..Default::default()
            },
            ToolCallRecord {
                name: "memory".into(),
                ok: true,
                ms: 0,
                disposition: Some(ToolCallDisposition::Deferred),
                ..Default::default()
            },
        ];

        let event = JournalEvent::turn(Some("s1"), 1, None, "hi", "done", 99, 1, 1, 1)
            .with_tool_calls(records);
        let parsed: JournalEvent =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        let outcomes = parsed.tool_outcomes.expect("tool outcomes");

        assert_eq!(parsed.tool_count, Some(2));
        assert_eq!(outcomes.requested, 6);
        assert_eq!(outcomes.executed, 2);
        assert_eq!(outcomes.succeeded, 1);
        assert_eq!(outcomes.failed, 1);
        assert_eq!(outcomes.rejected, 1);
        assert_eq!(outcomes.reused, 1);
        assert_eq!(outcomes.suppressed, 1);
        assert_eq!(outcomes.deferred, 1);
        assert!(outcomes.is_consistent());
        let calls = parsed.tool_calls.unwrap();
        assert_eq!(
            calls
                .iter()
                .map(ToolCallRecord::canonical_terminal_event_type)
                .collect::<Vec<_>>(),
            vec![
                "tool_call_completed",
                "tool_call_failed",
                "tool_call_rejected",
                "tool_call_reused",
                "tool_call_suppressed",
                "tool_call_deferred",
            ]
        );
        assert_eq!(
            calls[1].error_kind,
            Some(astra_core::ErrorKind::ToolInvalidArgs)
        );
    }

    #[test]
    fn resolve_session_id_accepts_exact_match() {
        let sessions = vec![
            "abc12345-0000-0000-0000-000000000000".to_string(),
            "def67890-0000-0000-0000-000000000000".to_string(),
        ];
        let resolved =
            resolve_session_id_from_list("abc12345-0000-0000-0000-000000000000", &sessions)
                .unwrap();
        assert_eq!(resolved, "abc12345-0000-0000-0000-000000000000");
    }

    #[test]
    fn resolve_session_id_accepts_unique_prefix() {
        let sessions = vec![
            "f5d90983-7130-41b6-8947-9827257c34f4".to_string(),
            "0be92d83-fb65-47d0-815a-dc8442930c3a".to_string(),
        ];
        let resolved = resolve_session_id_from_list("f5d90983-713", &sessions).unwrap();
        assert_eq!(resolved, "f5d90983-7130-41b6-8947-9827257c34f4");
    }

    #[test]
    fn resolve_session_id_rejects_ambiguous_prefix() {
        let sessions = vec![
            "f5d90983-7130-41b6-8947-9827257c34f4".to_string(),
            "f5d90983-7131-4b27-8b15-cfdc3375390f".to_string(),
        ];
        let err = resolve_session_id_from_list("f5d90983-713", &sessions).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn resolve_session_id_rejects_unknown_prefix() {
        let sessions = vec!["abc12345-0000-0000-0000-000000000000".to_string()];
        let err = resolve_session_id_from_list("missing", &sessions).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("no session journal matches"));
    }

    #[test]
    fn turn_guard_verdict_event_serializes() {
        let evt = JournalEvent::turn_guard_verdict(
            Some("sess-1"),
            3,
            "warning",
            &["Stall detected: repeated bash calls".to_string()],
            &[" bash ".to_string(), "bash".to_string(), "".to_string()],
            &[" bash ".to_string(), "bash".to_string(), " ".to_string()],
            false,
            1,
            2,
            0, // total_timeouts
            &[],
            0, // total_cache_hits
            0, // flaky_count
        );
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"turn_guard_verdict\""));
        assert!(json.contains("\"turn\":3"));
        // stall_type field reused for severity
        assert!(json.contains("\"stall_type\":\"warning\""));

        // Metadata should contain verdict details
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::TurnGuardVerdict);
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["severity"], "warning");
        assert_eq!(meta["injections"], 1);
        assert_eq!(meta["avoid_tools"][0], "bash");
        assert_eq!(meta["avoid_tools_count"], 1);
        assert_eq!(meta["health_avoidance_tools"][0], "bash");
        assert_eq!(meta["avoid_reason_codes"][0], "tool_health_avoidance");
        assert_eq!(meta["avoid_reason_codes"][1], "session_failures");
        assert_eq!(meta["avoid_reason_codes"][2], "stall_recovery");
        assert_eq!(
            meta["avoid_reason_summary"],
            "health avoidance tools: bash; 2 non-timeout failure(s) recorded; 1 stall/divergence nudge(s) issued"
        );
        assert_eq!(meta["advisory_threshold_reached"], false);
        assert_eq!(meta["nudge_count"], 1);
        assert_eq!(meta["total_errors"], 2);
        assert_eq!(meta["non_timeout_errors"], 2);
        assert_eq!(meta["health_avoidance_count"], 1);
        assert_eq!(meta["total_timeouts"], 0);
        assert_eq!(meta["total_cache_hits"], 0);
        assert_eq!(meta["flaky_tools"], 0);
    }

    #[test]
    fn turn_guard_verdict_critical_advisory_threshold_reached() {
        let evt = JournalEvent::turn_guard_verdict(
            Some("sess-1"),
            5,
            "critical",
            &[
                "CRITICAL: multiple stalls".to_string(),
                "Tool health degraded".to_string(),
            ],
            &["bash".to_string(), "grep".to_string()],
            &["bash".to_string(), "grep".to_string()],
            true,
            3,
            5,
            2, // total_timeouts
            &["bash".to_string()],
            1, // total_cache_hits
            1, // flaky_count
        );
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["severity"], "critical");
        assert_eq!(meta["advisory_threshold_reached"], true);
        assert_eq!(meta["injections"], 2);
        assert_eq!(meta["nudge_count"], 3);
        assert_eq!(meta["non_timeout_errors"], 3);
        assert_eq!(meta["timeout_dominant_tools"][0], "bash");
        assert_eq!(meta["total_timeouts"], 2);
        assert_eq!(meta["total_cache_hits"], 1);
        assert_eq!(meta["flaky_tools"], 1);
        // injection_preview should truncate to first injection
        assert!(
            meta["injection_preview"]
                .as_str()
                .unwrap()
                .contains("CRITICAL")
        );
    }

    #[test]
    fn turn_guard_verdict_info_minimal() {
        let evt = JournalEvent::turn_guard_verdict(
            None,
            1,
            "info",
            &[],
            &[],
            &[],
            false,
            0,
            1,
            0,
            &[],
            0,
            0,
        );
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::TurnGuardVerdict);
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["injections"], 0);
        assert!(meta["injection_preview"].is_null());
        assert_eq!(meta["advisory_threshold_reached"], false);
        assert_eq!(meta["non_timeout_errors"], 1);
        assert_eq!(meta["avoid_tools_count"], 0);
    }

    #[test]
    fn turn_evaluation_event_serializes() {
        let evt = JournalEvent::turn_evaluation(
            Some("sess-1"),
            Some(4),
            "cli_repl",
            true,
            true,
            0.91,
            0.72,
            0.18,
            1,
            false,
            2,
            vec![serde_json::json!({
                "kind": "all_tools_healthy",
                "weight": 0.4,
                "message": "All tool calls completed successfully"
            })],
        );
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"turn_evaluation\""));
        assert!(json.contains("\"turn\":4"));

        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::TurnEvaluation);
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["source"], "cli_repl");
        assert_eq!(meta["live_query"], true);
        assert_eq!(meta["success"], true);
        assert_eq!(meta["quality"], 0.91);
        assert_eq!(meta["confidence"], 0.72);
        assert_eq!(meta["budget_pressure"], 0.18);
        assert_eq!(meta["stall_count"], 1);
        assert_eq!(meta["verdict_warning"], false);
        assert_eq!(meta["tool_call_count"], 2);
        assert_eq!(meta["signal_count"], 1);
        assert_eq!(meta["signals"][0]["kind"], "all_tools_healthy");
    }

    #[test]
    fn turn_evaluation_event_without_turn_is_allowed() {
        let evt = JournalEvent::turn_evaluation(
            Some("sess-2"),
            None,
            "server_runtime",
            false,
            false,
            0.35,
            0.81,
            0.64,
            2,
            true,
            0,
            vec![],
        );
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::TurnEvaluation);
        assert_eq!(parsed.turn, None);
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["source"], "server_runtime");
        assert_eq!(meta["signal_count"], 0);
        assert_eq!(meta["signals"], serde_json::json!([]));
    }

    #[test]
    fn stall_and_checkpoint_events() {
        let journal_root = tempfile::TempDir::new().unwrap();
        let _journal_guard = JournalDirGuard::new(journal_root.path());
        // --- stall_detected field correctness ---
        {
            let evt = JournalEvent::stall_detected(
                Some("sess-1"),
                5,
                "repetition_stall",
                2,
                0.7,
                &[
                    " bash ".to_string(),
                    "bash".to_string(),
                    "".to_string(),
                    "grep".to_string(),
                ],
            );
            assert_eq!(evt.event_type, JournalEventType::StallDetected);
            assert_eq!(evt.turn, Some(5));
            assert_eq!(evt.stall_type.as_deref(), Some("repetition_stall"));
            let meta = evt.metadata.unwrap();
            assert_eq!(meta["nudge_count"], 2);
            assert_eq!(meta["confidence"], 0.7);
            assert_eq!(meta["avoid_tools"], serde_json::json!(["bash", "grep"]));
        }

        // --- stall_detected JSON roundtrip ---
        {
            let evt = JournalEvent::stall_detected(
                Some("sess-2"),
                3,
                "exploration_stall",
                1,
                0.5,
                &["list_dir".to_string()],
            );
            let json = serde_json::to_string(&evt).unwrap();
            let restored: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.event_type, JournalEventType::StallDetected);
            assert_eq!(restored.turn, Some(3));
        }

        // --- stall_detected confidence range ---
        for confidence in [0.0, 0.5, 0.8, 1.0] {
            let evt = JournalEvent::stall_detected(Some("s"), 1, "stall", 0, confidence, &[]);
            let meta = evt.metadata.unwrap();
            let stored = meta["confidence"].as_f64().unwrap();
            assert!(
                (stored - confidence).abs() < 1e-9,
                "confidence {confidence} should be stored exactly, got {stored}"
            );
        }

        // --- checkpoint field correctness ---
        {
            let evt = JournalEvent::checkpoint(
                Some("sess-1"),
                10,
                "Completed token efficiency phase",
                50_000,
                15,
            );
            assert_eq!(evt.event_type, JournalEventType::Checkpoint);
            assert_eq!(evt.turn, Some(10));
            let meta = evt.metadata.unwrap();
            assert_eq!(meta["summary"], "Completed token efficiency phase");
            assert_eq!(meta["total_tokens"], 50_000);
            assert_eq!(meta["tools_used_count"], 15);
        }

        // --- checkpoint JSON roundtrip ---
        {
            let evt = JournalEvent::checkpoint(Some("sess-1"), 5, "Phase A done", 10_000, 8);
            let json = serde_json::to_string(&evt).unwrap();
            let restored: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.event_type, JournalEventType::Checkpoint);
            assert_eq!(restored.turn, Some(5));
            let meta = restored.metadata.unwrap();
            assert_eq!(meta["summary"], "Phase A done");
            assert_eq!(meta["total_tokens"], 10_000);
            assert_eq!(meta["tools_used_count"], 8);
        }

        // --- checkpoint summary truncation ---
        {
            let long_summary = "x".repeat(600);
            let evt = JournalEvent::checkpoint(Some("s"), 1, &long_summary, 0, 0);
            let meta = evt.metadata.unwrap();
            let stored = meta["summary"].as_str().unwrap();
            assert!(
                stored.chars().count() <= 501,
                "summary should be truncated to ~500 chars, got {}",
                stored.chars().count()
            );
            assert!(
                stored.ends_with('…'),
                "truncated summary should end with ellipsis"
            );
        }

        // --- writer round-trip ---
        {
            let sid = format!("test-stall-ckpt-{}", uuid::Uuid::new_v4());
            let writer = JournalWriter::new(&sid).unwrap();
            writer
                .append(&JournalEvent::stall_detected(
                    Some(&sid),
                    3,
                    "repetition_stall",
                    1,
                    0.7,
                    &["bash".to_string()],
                ))
                .unwrap();
            writer
                .append(&JournalEvent::checkpoint(
                    Some(&sid),
                    5,
                    "Midpoint checkpoint",
                    20_000,
                    10,
                ))
                .unwrap();

            let events = read_journal(&sid).unwrap();
            assert_eq!(events.len(), 3);
            assert_eq!(events[0].event_type, JournalEventType::SessionStart);
            assert_eq!(events[1].event_type, JournalEventType::StallDetected);
            assert_eq!(events[1].stall_type.as_deref(), Some("repetition_stall"));
            assert_eq!(events[2].event_type, JournalEventType::Checkpoint);
            let meta = events[2].metadata.as_ref().unwrap();
            assert_eq!(meta["summary"], "Midpoint checkpoint");
            assert_eq!(meta["total_tokens"], 20_000);
        }
    }

    #[test]
    fn plan_progress_event_builder() {
        let evt = JournalEvent::plan_progress(
            Some("s1"),
            5,
            "add-tests",
            "Add unit tests",
            "started",
            40,
            5,
            2,
        );
        assert_eq!(evt.event_type, JournalEventType::PlanProgress);
        assert_eq!(evt.turn, Some(5));
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["subtask_id"], "add-tests");
        assert_eq!(meta["subtask_title"], "Add unit tests");
        assert_eq!(meta["action"], "started");
        assert_eq!(meta["progress_pct"], 40);
        assert_eq!(meta["total_subtasks"], 5);
        assert_eq!(meta["completed_subtasks"], 2);
    }

    #[test]
    fn plan_progress_serialization_roundtrip() {
        let evt =
            JournalEvent::plan_progress(Some("s1"), 3, "fix-bug", "Fix login", "started", 0, 3, 0);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::PlanProgress);
        assert_eq!(parsed.turn, Some(3));
        let meta = parsed.metadata.as_ref().unwrap();
        assert_eq!(meta["subtask_id"], "fix-bug");
        assert_eq!(meta["action"], "started");

        // Also test completed and plan_complete variants
        let evt2 =
            JournalEvent::plan_progress(Some("s1"), 5, "", "Full plan", "plan_complete", 100, 3, 3);
        let json2 = serde_json::to_string(&evt2).unwrap();
        let parsed2: JournalEvent = serde_json::from_str(&json2).unwrap();
        assert_eq!(
            parsed2.metadata.as_ref().unwrap()["action"],
            "plan_complete"
        );
        assert_eq!(parsed2.metadata.as_ref().unwrap()["progress_pct"], 100);
    }

    // ── count_turns tests ──────────────────────────────────────────────

    #[test]
    fn count_turns_counts_only_turn_events() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(dir.path());
        let sid = format!("count-test-{}", uuid::Uuid::new_v4());
        let lines = [
            r#"{"type":"session_start","ts":"2026-01-01T00:00:00Z","session_id":"s"}"#,
            r#"{"type":"turn","ts":"2026-01-01T00:00:01Z","session_id":"s","turn":1}"#,
            r#"{"type":"checkpoint","ts":"2026-01-01T00:00:02Z","session_id":"s"}"#,
            r#"{"type":"turn","ts":"2026-01-01T00:00:03Z","session_id":"s","turn":2}"#,
            r#"{"type":"session_end","ts":"2026-01-01T00:00:04Z","session_id":"s"}"#,
        ];
        let real_path = journal_file_path(&sid);
        std::fs::create_dir_all(real_path.parent().unwrap()).unwrap();
        std::fs::write(&real_path, lines.join("\n")).unwrap();

        let count = count_turns(&sid);
        assert_eq!(count, 2, "should count exactly 2 turn events");
    }

    #[test]
    fn count_turns_returns_zero_for_missing_session() {
        assert_eq!(count_turns("nonexistent-session-xyz-999"), 0);
    }

    #[test]
    fn count_turns_ignores_checkpoint_and_other_types() {
        let sid = format!("count-no-turns-{}", uuid::Uuid::new_v4());
        let real_path = journal_dir().join(format!("{sid}.jsonl"));
        std::fs::create_dir_all(journal_dir()).ok();
        std::fs::write(
            &real_path,
            r#"{"type":"session_start","ts":"2026-01-01T00:00:00Z"}
{"type":"checkpoint","ts":"2026-01-01T00:00:01Z"}
{"type":"session_end","ts":"2026-01-01T00:00:02Z"}"#,
        )
        .unwrap();

        assert_eq!(count_turns(&sid), 0);
        let _ = std::fs::remove_file(&real_path);
    }

    // ── list_sessions_by_time tests ────────────────────────────────────

    #[test]
    fn list_sessions_by_time_filters_test_prefixes() {
        std::fs::create_dir_all(journal_dir()).ok();

        // Create test-prefixed and real session files
        let real_sid = format!("real-session-{}", uuid::Uuid::new_v4());
        let test_sid = format!("test-session-{}", uuid::Uuid::new_v4());
        let new_sess = format!("new-sess-{}", uuid::Uuid::new_v4());

        let real_path = journal_dir().join(format!("{real_sid}.jsonl"));
        let test_path = journal_dir().join(format!("{test_sid}.jsonl"));
        let new_path = journal_dir().join(format!("{new_sess}.jsonl"));

        std::fs::write(&real_path, "{}").unwrap();
        std::fs::write(&test_path, "{}").unwrap();
        std::fs::write(&new_path, "{}").unwrap();

        let sessions = list_sessions_by_time(100).unwrap();
        assert!(
            sessions.contains(&real_sid),
            "real session should be listed"
        );
        assert!(
            !sessions.contains(&test_sid),
            "test- prefix should be filtered"
        );
        assert!(
            !sessions.contains(&new_sess),
            "new-sess- prefix should be filtered"
        );

        // Cleanup
        let _ = std::fs::remove_file(&real_path);
        let _ = std::fs::remove_file(&test_path);
        let _ = std::fs::remove_file(&new_path);
    }

    #[test]
    fn list_sessions_by_time_respects_limit() {
        std::fs::create_dir_all(journal_dir()).ok();

        let mut created = Vec::new();
        for i in 0..5 {
            let sid = format!("limit-test-{i}-{}", uuid::Uuid::new_v4());
            let path = journal_dir().join(format!("{sid}.jsonl"));
            std::fs::write(&path, "{}").unwrap();
            // Stagger mtime slightly
            std::thread::sleep(std::time::Duration::from_millis(10));
            created.push((sid, path));
        }

        let sessions = list_sessions_by_time(3).unwrap();
        let our_sessions: Vec<_> = sessions
            .iter()
            .filter(|s| s.starts_with("limit-test-"))
            .collect();
        assert!(
            our_sessions.len() <= 3,
            "should return at most 3 of our sessions, got {}",
            our_sessions.len()
        );

        // Cleanup
        for (_, path) in &created {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn list_sessions_by_time_for_user_is_owner_scoped() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let user_a_sid = format!("owner-a-{}", uuid::Uuid::new_v4());
        let user_b_sid = format!("owner-b-{}", uuid::Uuid::new_v4());
        let user_a_path = journal_file_path_for_user("user-a", &user_a_sid).unwrap();
        let user_b_path = journal_file_path_for_user("user-b", &user_b_sid).unwrap();
        std::fs::create_dir_all(user_a_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(user_b_path.parent().unwrap()).unwrap();
        std::fs::write(&user_a_path, "{}").unwrap();
        std::fs::write(&user_b_path, "{}").unwrap();

        let user_a_sessions = list_sessions_by_time_for_user("user-a", 100).unwrap();
        let user_b_sessions = list_sessions_by_time_for_user("user-b", 100).unwrap();

        assert!(user_a_sessions.contains(&user_a_sid));
        assert!(!user_a_sessions.contains(&user_b_sid));
        assert!(user_b_sessions.contains(&user_b_sid));
        assert!(!user_b_sessions.contains(&user_a_sid));
    }

    #[test]
    fn user_scoped_writer_never_falls_back_to_local_owner() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let session_id = format!("owner-writer-{}", uuid::Uuid::new_v4());
        let user_path = journal_file_path_for_user("user-a", &session_id).unwrap();
        let local_path = journal_file_path(&session_id);

        let writer = JournalWriter::for_user("user-a", &session_id).unwrap();
        writer
            .append(&JournalEvent::session_start(
                Some(&session_id),
                Some("model"),
            ))
            .unwrap();

        assert_eq!(writer.path(), &user_path);
        assert!(user_path.is_file());
        assert!(!local_path.exists());
        assert_eq!(
            read_journal_for_user("user-a", &session_id).unwrap().len(),
            1
        );
        assert!(read_journal(&session_id).unwrap().is_empty());
    }

    #[test]
    fn delete_session_for_user_removes_only_owner_bound_artifacts() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = format!("owner-delete-{}", uuid::Uuid::new_v4());
        let user_a = OwnerScope::user("user-a").unwrap();
        let user_b = OwnerScope::user("user-b").unwrap();
        let store = crate::local_session_artifact_store();
        let user_a_journal = journal_file_path_for_owner(&user_a, &sid).unwrap();
        let user_b_journal = journal_file_path_for_owner(&user_b, &sid).unwrap();
        let user_a_session_dir = store.session_dir_for_owner(&user_a, &sid).unwrap();
        let user_b_session_dir = store.session_dir_for_owner(&user_b, &sid).unwrap();
        std::fs::create_dir_all(user_a_journal.parent().unwrap()).unwrap();
        std::fs::create_dir_all(user_b_journal.parent().unwrap()).unwrap();
        std::fs::create_dir_all(user_a_session_dir.join("step_checkpoints")).unwrap();
        std::fs::create_dir_all(user_b_session_dir.join("step_checkpoints")).unwrap();
        std::fs::write(&user_a_journal, "{}").unwrap();
        std::fs::write(&user_b_journal, "{}").unwrap();
        std::fs::write(
            user_a_session_dir
                .join("step_checkpoints")
                .join("000001-heavy.json"),
            "{}",
        )
        .unwrap();
        std::fs::write(
            user_b_session_dir
                .join("step_checkpoints")
                .join("000001-heavy.json"),
            "{}",
        )
        .unwrap();

        let freed = delete_session_for_user("user-a", &sid).unwrap();

        assert!(freed > 0);
        assert!(!user_a_journal.exists());
        assert!(!user_a_session_dir.exists());
        assert!(user_b_journal.exists());
        assert!(
            user_b_session_dir
                .join("step_checkpoints")
                .join("000001-heavy.json")
                .exists()
        );
    }

    #[test]
    fn delegation_events() {
        // --- delegation_started ---
        {
            let agents = vec!["agent-a".to_string(), "agent-b".to_string()];
            let evt = JournalEvent::delegation_started(
                Some("s1"),
                "del-1",
                "run-parent",
                "fan_out",
                &agents,
            );
            assert_eq!(evt.event_type, JournalEventType::DelegationStarted);
            let meta = evt.metadata.as_ref().unwrap();
            assert_eq!(meta["delegation_id"], "del-1");
            assert_eq!(meta["pattern"], "fan_out");
            assert_eq!(meta["agent_count"], 2);
        }

        // --- delegation_sub_run_completed ---
        {
            let evt = JournalEvent::delegation_sub_run_completed(
                Some("s1"),
                "del-1",
                "run-sub-1",
                "agent-a",
                "completed",
                None,
                Some("finished the review"),
            );
            assert_eq!(evt.event_type, JournalEventType::DelegationSubRunCompleted);
            let meta = evt.metadata.as_ref().unwrap();
            assert_eq!(meta["agent_id"], "agent-a");
            assert_eq!(meta["status"], "completed");
            assert!(meta["error"].is_null());
            assert_eq!(meta["output_preview"], "finished the review");
        }

        // --- delegation_sub_run_started ---
        {
            let evt = JournalEvent::delegation_sub_run_started(
                Some("s1"),
                "del-1",
                "run-sub-1",
                "run-parent",
                "agent-a",
                "running",
                2,
                Some("run-sub-0"),
            );
            assert_eq!(evt.event_type, JournalEventType::DelegationSubRunStarted);
            let meta = evt.metadata.as_ref().unwrap();
            assert_eq!(meta["delegation_id"], "del-1");
            assert_eq!(meta["sub_run_id"], "run-sub-1");
            assert_eq!(meta["parent_run_id"], "run-parent");
            assert_eq!(meta["agent_id"], "agent-a");
            assert_eq!(meta["status"], "running");
            assert_eq!(meta["depth"], 2);
            assert_eq!(meta["retry_of"], "run-sub-0");
        }

        // --- delegation_retry ---
        {
            let evt = JournalEvent::delegation_retry(
                Some("s1"),
                "del-1",
                "run-sub-1",
                "run-sub-2",
                "agent-a",
                2,
                "quality too low",
            );
            assert_eq!(evt.event_type, JournalEventType::DelegationRetry);
            let meta = evt.metadata.as_ref().unwrap();
            assert_eq!(meta["original_run_id"], "run-sub-1");
            assert_eq!(meta["retry_run_id"], "run-sub-2");
            assert_eq!(meta["attempt"], 2);
            assert_eq!(meta["reason"], "quality too low");
        }

        // --- delegation_completed ---
        {
            let evt = JournalEvent::delegation_completed(
                Some("s1"),
                "del-1",
                "fan_out",
                3,
                2,
                1,
                "partial",
                Some("merged result preview"),
            );
            assert_eq!(evt.event_type, JournalEventType::DelegationCompleted);
            let meta = evt.metadata.as_ref().unwrap();
            assert_eq!(meta["succeeded"], 2);
            assert_eq!(meta["failed"], 1);
            assert_eq!(meta["aggregated_status"], "partial");
            assert_eq!(meta["aggregated_output_preview"], "merged result preview");
        }

        // --- serde roundtrip ---
        {
            let agents = vec!["a1".to_string()];
            let evt =
                JournalEvent::delegation_started(Some("s1"), "d1", "r1", "sequential", &agents);
            let json = serde_json::to_string(&evt).unwrap();
            let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.event_type, JournalEventType::DelegationStarted);
            assert!(json.contains("\"delegation_id\":\"d1\""));
        }
    }

    #[test]
    fn delegation_sub_run_completed_event_builder() {
        let evt = JournalEvent::delegation_sub_run_completed(
            Some("s1"),
            "del-1",
            "run-sub-1",
            "agent-a",
            "completed",
            None,
            Some("finished the review"),
        );
        assert_eq!(evt.event_type, JournalEventType::DelegationSubRunCompleted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["agent_id"], "agent-a");
        assert_eq!(meta["status"], "completed");
        assert!(meta["error"].is_null());
        assert_eq!(meta["output_preview"], "finished the review");
    }

    #[test]
    fn delegation_sub_run_started_event_builder() {
        let evt = JournalEvent::delegation_sub_run_started(
            Some("s1"),
            "del-1",
            "run-sub-1",
            "run-parent",
            "agent-a",
            "running",
            2,
            Some("run-sub-0"),
        );
        assert_eq!(evt.event_type, JournalEventType::DelegationSubRunStarted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-1");
        assert_eq!(meta["sub_run_id"], "run-sub-1");
        assert_eq!(meta["parent_run_id"], "run-parent");
        assert_eq!(meta["agent_id"], "agent-a");
        assert_eq!(meta["status"], "running");
        assert_eq!(meta["depth"], 2);
        assert_eq!(meta["retry_of"], "run-sub-0");
    }

    #[test]
    fn delegation_retry_event_builder() {
        let evt = JournalEvent::delegation_retry(
            Some("s1"),
            "del-1",
            "run-sub-1",
            "run-sub-2",
            "agent-a",
            2,
            "quality too low",
        );
        assert_eq!(evt.event_type, JournalEventType::DelegationRetry);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["original_run_id"], "run-sub-1");
        assert_eq!(meta["retry_run_id"], "run-sub-2");
        assert_eq!(meta["attempt"], 2);
        assert_eq!(meta["reason"], "quality too low");
    }

    #[test]
    fn delegation_completed_event_builder() {
        let evt = JournalEvent::delegation_completed(
            Some("s1"),
            "del-1",
            "fan_out",
            3,
            2,
            1,
            "partial",
            Some("merged result preview"),
        );
        assert_eq!(evt.event_type, JournalEventType::DelegationCompleted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["succeeded"], 2);
        assert_eq!(meta["failed"], 1);
        assert_eq!(meta["aggregated_status"], "partial");
        assert_eq!(meta["aggregated_output_preview"], "merged result preview");
    }

    // ── Session ID Validation Security Tests ──

    #[test]
    fn validate_session_id_rejects_invalid() {
        assert!(validate_session_id("../../etc/passwd").is_err());
        assert!(validate_session_id("../sibling").is_err());
        assert!(validate_session_id("a/b/c").is_err());
        assert!(validate_session_id("a\\b").is_err());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("   ").is_err());
        assert!(validate_session_id(".").is_err());
        assert!(validate_session_id("a\0b").is_err());
        assert!(validate_session_id("has\nnewline").is_err());
        assert!(validate_session_id("has\ttab").is_err());
        assert!(validate_session_id("has\x7Fdel").is_err());
        // Non-ASCII: Unicode invisible chars, RTL override, homoglyphs
        assert!(validate_session_id("café").is_err());
        assert!(validate_session_id("abc\u{200B}def").is_err());
        assert!(validate_session_id("\u{202E}secret").is_err());
        // Max length
        assert!(validate_session_id(&"a".repeat(201)).is_err());
        assert!(validate_session_id(&"a".repeat(200)).is_ok());
    }

    #[test]
    fn validate_session_id_accepts_safe_ids() {
        assert!(validate_session_id("abc-123").is_ok());
        assert!(validate_session_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_session_id("my_session").is_ok());
        assert!(validate_session_id("session.2024").is_ok());
    }

    #[test]
    #[should_panic(expected = "unsafe session ID")]
    fn journal_file_path_panics_on_traversal() {
        let _ = journal_file_path("../../etc/passwd");
    }

    #[test]
    fn session_maintenance_all_scenarios() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        // Helper: create a journal file with a backdated mtime.
        fn create_aged_journal(dir: &Path, session_id: &str, age_days: u64) {
            let path = dir.join(format!("{session_id}.jsonl"));
            std::fs::write(&path, r#"{"type":"session_start"}"#).unwrap();
            let mtime = filetime::FileTime::from_system_time(
                std::time::SystemTime::now()
                    - std::time::Duration::from_secs(age_days * 86400 + 3600),
            );
            filetime::set_file_mtime(&path, mtime).unwrap();
        }

        // Delete expired sessions (40 days, TTL=30)
        create_aged_journal(&dir, "old-session", 40);
        let session_dir = dir.join("old-session");
        std::fs::create_dir_all(session_dir.join("step_checkpoints")).unwrap();
        std::fs::write(session_dir.join("workspace.yaml"), "session_id: test").unwrap();
        create_aged_journal(&dir, "new-session", 1);
        let result = run_session_maintenance_in(dir.clone(), 30, 7);
        assert_eq!(result.sessions_deleted, 1);
        assert!(!dir.join("old-session.jsonl").exists());
        assert!(!dir.join("old-session").exists());
        assert!(dir.join("new-session.jsonl").exists());

        // Compress old journals (10 days, compress_after=7)
        create_aged_journal(&dir, "mid-session", 10);
        let result = run_session_maintenance_in(dir.clone(), 30, 7);
        assert_eq!(result.journals_compressed, 1);
        assert!(!dir.join("mid-session.jsonl").exists());
        assert!(dir.join("mid-session.jsonl.gz").exists());

        // Skip recent sessions (fresh, 0 days)
        std::fs::write(dir.join("fresh.jsonl"), r#"{"type":"session_start"}"#).unwrap();
        let result = run_session_maintenance_in(dir.clone(), 30, 7);
        assert_eq!(result.sessions_deleted, 0);
        assert_eq!(result.journals_compressed, 0);
        assert!(dir.join("fresh.jsonl").exists());

        // Delete expired compressed files (40 days old .gz)
        let gz_path = dir.join("archived.jsonl.gz");
        std::fs::write(&gz_path, b"fake-gz-data").unwrap();
        let mtime = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86400 + 3600),
        );
        filetime::set_file_mtime(&gz_path, mtime).unwrap();
        let result = run_session_maintenance_in(dir.clone(), 30, 7);
        assert_eq!(result.sessions_deleted, 1);
        assert!(!gz_path.exists());

        // Empty dir returns defaults
        let tmp2 = tempdir().unwrap();
        let result = run_session_maintenance_in(tmp2.path().to_path_buf(), 30, 7);
        assert_eq!(result.sessions_deleted, 0);
        assert_eq!(result.journals_compressed, 0);
        assert_eq!(result.bytes_freed, 0);
    }

    #[test]
    #[serial_test::serial(astra_journal_content_redact_env)]
    fn journal_content_redaction_and_markers() {
        // ── Content markers ──
        let a = journal_content_marker("hello world");
        let b = journal_content_marker("hello world");
        assert_eq!(a, b);
        assert!(a.starts_with("<redacted: len=11 sha="));
        assert!(a.ends_with('>'));
        assert!(!a.contains("hello"));
        assert_ne!(
            journal_content_marker("hello"),
            journal_content_marker("world")
        );

        // ── Redaction env toggle ──
        unsafe { std::env::remove_var("ASTRA_JOURNAL_CONTENT_REDACT") };
        assert!(!journal_content_redact_enabled());
        unsafe { std::env::set_var("ASTRA_JOURNAL_CONTENT_REDACT", "1") };
        assert!(journal_content_redact_enabled());
        unsafe { std::env::set_var("ASTRA_JOURNAL_CONTENT_REDACT", "0") };
        assert!(!journal_content_redact_enabled());

        // ── Turn event redacts content when env is set ──
        unsafe { std::env::set_var("ASTRA_JOURNAL_CONTENT_REDACT", "1") };
        let evt = JournalEvent::turn(
            Some("s1"),
            1,
            Some("gpt-4"),
            "secret query",
            "secret answer",
            0,
            10,
            5,
            100,
        );
        let user = evt.user_input.as_deref().unwrap_or("");
        let asst = evt.assistant_output.as_deref().unwrap_or("");
        assert!(!user.contains("secret query"), "user_input leaked: {user}");
        assert!(
            !asst.contains("secret answer"),
            "assistant_output leaked: {asst}"
        );
        assert!(user.starts_with("<redacted:"));
        assert!(asst.starts_with("<redacted:"));

        // ── Turn event keeps content when env unset ──
        unsafe { std::env::remove_var("ASTRA_JOURNAL_CONTENT_REDACT") };
        let evt = JournalEvent::turn(
            Some("s1"),
            1,
            Some("gpt-4"),
            "hello",
            "world",
            0,
            10,
            5,
            100,
        );
        assert_eq!(evt.user_input.as_deref(), Some("hello"));
        assert_eq!(evt.assistant_output.as_deref(), Some("world"));

        // ── Local child transcript preserves typed identity and raw message ──
        let message = serde_json::json!({
            "role": "assistant",
            "content": "child answer",
            "reasoning_content": "checked the invariant",
        });
        let evt =
            JournalEvent::transcript_item("parent-session", "local-run", "reviewer", 7, &message)
                .expect("valid transcript message");
        assert_eq!(evt.event_type, JournalEventType::TranscriptItem);
        assert_eq!(evt.session_id.as_deref(), Some("parent-session"));
        let payload = evt.transcript_item.expect("typed transcript payload");
        assert!(
            payload
                .source_event_id
                .starts_with("local-transcript:sha256:")
        );
        assert_eq!(payload.run_id, "local-run");
        assert_eq!(payload.agent_id, "reviewer");
        assert_eq!(payload.item_seq, 7);
        assert_eq!(payload.message, message);
        let retry = JournalEvent::transcript_item(
            "parent-session",
            "local-run",
            "reviewer",
            7,
            &serde_json::json!({"role": "assistant", "content": "provider retried"}),
        )
        .expect("retry transcript message");
        assert_eq!(
            retry
                .transcript_item
                .as_ref()
                .expect("retry payload")
                .source_event_id,
            payload.source_event_id
        );

        // Redaction covers the entire raw provider message, including tool
        // arguments and reasoning fields, rather than only visible content.
        unsafe { std::env::set_var("ASTRA_JOURNAL_CONTENT_REDACT", "1") };
        let redacted =
            JournalEvent::transcript_item("parent-session", "local-run", "reviewer", 8, &message)
                .unwrap()
                .transcript_item
                .unwrap()
                .message;
        assert_eq!(redacted["role"], "assistant");
        assert!(
            redacted["content"]
                .as_str()
                .unwrap()
                .starts_with("<redacted:")
        );
        assert!(redacted.get("reasoning_content").is_none());

        // ── Turn error redacts user input ──
        unsafe { std::env::set_var("ASTRA_JOURNAL_CONTENT_REDACT", "1") };
        let evt =
            JournalEvent::turn_error(Some("s1"), 1, Some("gpt-4"), "secret query", "boom", 50);
        let user = evt.user_input.as_deref().unwrap_or("");
        assert!(!user.contains("secret query"));
        assert!(user.starts_with("<redacted:"));
        assert_eq!(evt.error.as_deref(), Some("boom"));

        unsafe { std::env::remove_var("ASTRA_JOURNAL_CONTENT_REDACT") };
    }

    #[test]
    fn combined_guard_eval_stall_progress() {
        // ── turn_guard_verdict: warning with avoid_tools ──
        {
            let evt = JournalEvent::turn_guard_verdict(
                Some("sess-1"),
                3,
                "warning",
                &["Stall detected: repeated bash calls".to_string()],
                &["bash".to_string()],
                &["bash".to_string()],
                false,
                1,
                2,
                0,
                &[],
                0,
                0,
            );
            let json = serde_json::to_string(&evt).unwrap();
            assert!(json.contains("\"type\":\"turn_guard_verdict\""));
            assert!(json.contains("\"turn\":3"));
            assert!(json.contains("\"stall_type\":\"warning\""));
            let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.event_type, JournalEventType::TurnGuardVerdict);
            let meta = parsed.metadata.unwrap();
            assert_eq!(meta["severity"], "warning");
            assert_eq!(meta["injections"], 1);
            assert_eq!(meta["avoid_tools"][0], "bash");
            assert_eq!(meta["avoid_tools_count"], 1);
            assert_eq!(meta["health_avoidance_tools"][0], "bash");
            assert_eq!(meta["advisory_threshold_reached"], false);
            assert_eq!(meta["nudge_count"], 1);
            assert_eq!(meta["total_errors"], 2);
            assert_eq!(meta["non_timeout_errors"], 2);
            assert_eq!(meta["health_avoidance_count"], 1);
            assert_eq!(meta["total_timeouts"], 0);
            assert_eq!(meta["total_cache_hits"], 0);
            assert_eq!(meta["flaky_tools"], 0);
        }

        // ── turn_guard_verdict: critical advisory_threshold_reached ──
        {
            let evt = JournalEvent::turn_guard_verdict(
                Some("sess-1"),
                5,
                "critical",
                &[
                    "CRITICAL: multiple stalls".to_string(),
                    "Tool health degraded".to_string(),
                ],
                &["bash".to_string(), "grep".to_string()],
                &["bash".to_string(), "grep".to_string()],
                true,
                3,
                5,
                2,
                &["bash".to_string()],
                1,
                1,
            );
            let json = serde_json::to_string(&evt).unwrap();
            let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
            let meta = parsed.metadata.unwrap();
            assert_eq!(meta["severity"], "critical");
            assert_eq!(meta["advisory_threshold_reached"], true);
            assert_eq!(meta["injections"], 2);
            assert_eq!(meta["nudge_count"], 3);
            assert_eq!(meta["non_timeout_errors"], 3);
            assert_eq!(meta["timeout_dominant_tools"][0], "bash");
            assert_eq!(meta["total_timeouts"], 2);
            assert_eq!(meta["total_cache_hits"], 1);
            assert_eq!(meta["flaky_tools"], 1);
            assert!(
                meta["injection_preview"]
                    .as_str()
                    .unwrap()
                    .contains("CRITICAL")
            );
        }

        // ── turn_guard_verdict: info minimal ──
        {
            let evt = JournalEvent::turn_guard_verdict(
                None,
                1,
                "info",
                &[],
                &[],
                &[],
                false,
                0,
                1,
                0,
                &[],
                0,
                0,
            );
            let json = serde_json::to_string(&evt).unwrap();
            let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.event_type, JournalEventType::TurnGuardVerdict);
            let meta = parsed.metadata.unwrap();
            assert_eq!(meta["injections"], 0);
            assert!(meta["injection_preview"].is_null());
            assert_eq!(meta["advisory_threshold_reached"], false);
            assert_eq!(meta["non_timeout_errors"], 1);
            assert_eq!(meta["avoid_tools_count"], 0);
        }

        // ── turn_evaluation: full fields ──
        {
            let evt = JournalEvent::turn_evaluation(
                Some("sess-1"),
                Some(4),
                "cli_repl",
                true,
                true,
                0.91,
                0.72,
                0.18,
                1,
                false,
                2,
                vec![
                    serde_json::json!({"kind": "all_tools_healthy", "weight": 0.4, "message": "All tool calls completed successfully"}),
                ],
            );
            let json = serde_json::to_string(&evt).unwrap();
            assert!(json.contains("\"type\":\"turn_evaluation\""));
            assert!(json.contains("\"turn\":4"));
            let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.event_type, JournalEventType::TurnEvaluation);
            let meta = parsed.metadata.unwrap();
            assert_eq!(meta["source"], "cli_repl");
            assert_eq!(meta["live_query"], true);
            assert_eq!(meta["success"], true);
            assert_eq!(meta["quality"], 0.91);
            assert_eq!(meta["confidence"], 0.72);
            assert_eq!(meta["budget_pressure"], 0.18);
            assert_eq!(meta["stall_count"], 1);
            assert_eq!(meta["verdict_warning"], false);
            assert_eq!(meta["tool_call_count"], 2);
            assert_eq!(meta["signal_count"], 1);
            assert_eq!(meta["signals"][0]["kind"], "all_tools_healthy");
        }

        // ── turn_evaluation: without turn (None) ──
        {
            let evt = JournalEvent::turn_evaluation(
                Some("sess-2"),
                None,
                "server_runtime",
                false,
                false,
                0.35,
                0.81,
                0.64,
                2,
                true,
                0,
                vec![],
            );
            let json = serde_json::to_string(&evt).unwrap();
            let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.event_type, JournalEventType::TurnEvaluation);
            assert_eq!(parsed.turn, None);
            let meta = parsed.metadata.unwrap();
            assert_eq!(meta["source"], "server_runtime");
            assert_eq!(meta["signal_count"], 0);
            assert_eq!(meta["signals"], serde_json::json!([]));
        }

        // ── stall_detected: field correctness ──
        {
            let evt = JournalEvent::stall_detected(
                Some("sess-1"),
                5,
                "repetition_stall",
                2,
                0.7,
                &[
                    " bash ".to_string(),
                    "bash".to_string(),
                    "".to_string(),
                    "grep".to_string(),
                ],
            );
            assert_eq!(evt.event_type, JournalEventType::StallDetected);
            assert_eq!(evt.turn, Some(5));
            assert_eq!(evt.stall_type.as_deref(), Some("repetition_stall"));
            let meta = evt.metadata.unwrap();
            assert_eq!(meta["nudge_count"], 2);
            assert_eq!(meta["confidence"], 0.7);
            assert_eq!(meta["avoid_tools"], serde_json::json!(["bash", "grep"]));
        }

        // ── stall_detected: JSON roundtrip ──
        {
            let evt = JournalEvent::stall_detected(
                Some("sess-2"),
                3,
                "exploration_stall",
                1,
                0.5,
                &["list_dir".to_string()],
            );
            let json = serde_json::to_string(&evt).unwrap();
            let restored: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.event_type, JournalEventType::StallDetected);
            assert_eq!(restored.turn, Some(3));
        }

        // ── stall_detected: confidence range ──
        for confidence in [0.0, 0.5, 0.8, 1.0] {
            let evt = JournalEvent::stall_detected(Some("s"), 1, "stall", 0, confidence, &[]);
            let meta = evt.metadata.unwrap();
            let stored = meta["confidence"].as_f64().unwrap();
            assert!(
                (stored - confidence).abs() < 1e-9,
                "confidence {confidence} stored as {stored}"
            );
        }

        // ── checkpoint: field correctness ──
        {
            let evt = JournalEvent::checkpoint(
                Some("sess-1"),
                10,
                "Completed token efficiency phase",
                50_000,
                15,
            );
            assert_eq!(evt.event_type, JournalEventType::Checkpoint);
            assert_eq!(evt.turn, Some(10));
            let meta = evt.metadata.unwrap();
            assert_eq!(meta["summary"], "Completed token efficiency phase");
            assert_eq!(meta["total_tokens"], 50_000);
            assert_eq!(meta["tools_used_count"], 15);
        }

        // ── plan_progress event builder ──
        {
            let evt = JournalEvent::plan_progress(
                Some("s1"),
                5,
                "add-tests",
                "Add unit tests",
                "started",
                40,
                5,
                2,
            );
            assert_eq!(evt.event_type, JournalEventType::PlanProgress);
            assert_eq!(evt.turn, Some(5));
            let meta = evt.metadata.as_ref().unwrap();
            assert_eq!(meta["subtask_id"], "add-tests");
            assert_eq!(meta["subtask_title"], "Add unit tests");
            assert_eq!(meta["action"], "started");
            assert_eq!(meta["progress_pct"], 40);
            assert_eq!(meta["total_subtasks"], 5);
            assert_eq!(meta["completed_subtasks"], 2);
        }

        // ── plan_progress serialization roundtrip ──
        {
            let evt = JournalEvent::plan_progress(
                Some("s1"),
                3,
                "fix-bug",
                "Fix login",
                "started",
                0,
                3,
                0,
            );
            let json = serde_json::to_string(&evt).unwrap();
            let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.event_type, JournalEventType::PlanProgress);
            assert_eq!(parsed.turn, Some(3));
            assert_eq!(parsed.metadata.as_ref().unwrap()["subtask_id"], "fix-bug");
            assert_eq!(parsed.metadata.as_ref().unwrap()["action"], "started");
            let evt2 = JournalEvent::plan_progress(
                Some("s1"),
                5,
                "",
                "Full plan",
                "plan_complete",
                100,
                3,
                3,
            );
            let json2 = serde_json::to_string(&evt2).unwrap();
            let parsed2: JournalEvent = serde_json::from_str(&json2).unwrap();
            assert_eq!(
                parsed2.metadata.as_ref().unwrap()["action"],
                "plan_complete"
            );
            assert_eq!(parsed2.metadata.as_ref().unwrap()["progress_pct"], 100);
        }
    }

    #[test]
    fn combined_tool_record_serde() {
        // ── basic serialization round-trip ──
        {
            let record = ToolCallRecord {
                name: "github".into(),
                ok: true,
                ms: 761,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: Some("owner/repo".into()),
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            };
            let json = serde_json::to_string(&record).unwrap();
            assert!(json.contains("\"ok\":true"));
            assert!(!json.contains("\"error\""), "None error should be omitted");
            let parsed: ToolCallRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.name, "github");
            assert!(parsed.ok);
            assert_eq!(parsed.ms, 761);
            assert!(parsed.error.is_none());
        }

        // ── with error field ──
        {
            let record = ToolCallRecord {
                name: "github".into(),
                ok: false,
                ms: 587,
                error: Some("missing repo parameter".into()),
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            };
            let json = serde_json::to_string(&record).unwrap();
            assert!(json.contains("\"ok\":false"));
            assert!(json.contains("missing repo"));
            let parsed: ToolCallRecord = serde_json::from_str(&json).unwrap();
            assert!(!parsed.ok);
            assert_eq!(parsed.error.as_deref(), Some("missing repo parameter"));
        }

        // ── bulk array of 100 records ──
        {
            let records: Vec<ToolCallRecord> = (0..100)
                .map(|i| ToolCallRecord {
                    name: format!("tool_{i}"),
                    ok: i % 2 == 0,
                    ms: i as u64 * 100,
                    error: if i % 3 == 0 {
                        Some(format!("err_{i}"))
                    } else {
                        None
                    },
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                    result_preview: None,
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
                })
                .collect();
            let json = serde_json::to_string(&records).unwrap();
            let parsed: Vec<ToolCallRecord> = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.len(), 100);
            assert_eq!(parsed[99].name, "tool_99");
            assert_eq!(parsed[0].ms, 0);
        }

        // ── unicode error message ──
        {
            let record = ToolCallRecord {
                name: "github".into(),
                ok: false,
                ms: 500,
                error: Some("连接超时: タイムアウト 🚫".into()),
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            };
            let json = serde_json::to_string(&record).unwrap();
            let parsed: ToolCallRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.error.unwrap(), "连接超时: タイムアウト 🚫");
        }

        // ── u64::MAX ms value ──
        {
            let record = ToolCallRecord {
                name: "bash".into(),
                ok: true,
                ms: u64::MAX,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            };
            let json = serde_json::to_string(&record).unwrap();
            let parsed: ToolCallRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.ms, u64::MAX);
        }

        // ── surgical fields round-trip ──
        {
            let rec = ToolCallRecord {
                name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
                ok: true,
                ms: 0,
                error: None,
                input_bytes: None,
                output_bytes: Some(0),
                args_preview: None,
                result_preview: Some("(removed)".into()),
                file_path: None,
                surgically_removed: Some(true),
                original_tool_name: Some("read_file".to_string()),
                ..Default::default()
            };
            let json = serde_json::to_string(&rec).unwrap();
            assert!(json.contains("\"surgically_removed\":true"));
            assert!(json.contains("\"original_tool_name\":\"read_file\""));
            let deser: ToolCallRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.surgically_removed, Some(true));
            assert_eq!(deser.original_tool_name.as_deref(), Some("read_file"));
            assert!(deser.is_synthetic_placeholder());
        }

        // ── surgical fields omitted when None ──
        {
            let rec = base_tool_record("bash", true, Some("ok"));
            let json = serde_json::to_string(&rec).unwrap();
            assert!(
                !json.contains("surgically_removed"),
                "None surgical fields should be skipped"
            );
            assert!(
                !json.contains("original_tool_name"),
                "None original_tool_name should be skipped"
            );
        }

        // ── new fields omitted when None ──
        {
            let rec = ToolCallRecord {
                name: "bash".into(),
                ok: true,
                ms: 50,
                ..Default::default()
            };
            let json = serde_json::to_string(&rec).unwrap();
            assert!(!json.contains("start_offset_ms"));
            assert!(!json.contains("batch_id"));
            assert!(!json.contains("parallel"));
            assert!(!json.contains("\"round\""));
        }

        // ── new fields round-trip ──
        {
            let rec = ToolCallRecord {
                name: "read_file".into(),
                ok: true,
                ms: 10,
                start_offset_ms: Some(5000),
                batch_id: Some("b-0-0".into()),
                parallel: Some(true),
                round: Some(2),
                ..Default::default()
            };
            let json = serde_json::to_string(&rec).unwrap();
            assert!(json.contains("\"start_offset_ms\":5000"));
            assert!(json.contains("\"batch_id\":\"b-0-0\""));
            assert!(json.contains("\"parallel\":true"));
            assert!(json.contains("\"round\":2"));
            let deser: ToolCallRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.start_offset_ms, Some(5000));
            assert_eq!(deser.batch_id.as_deref(), Some("b-0-0"));
            assert_eq!(deser.parallel, Some(true));
            assert_eq!(deser.round, Some(2));
        }
    }

    #[test]
    fn combined_classification_and_predicates() {
        // ── is_synthetic_placeholder detection ──
        {
            let skipped = ToolCallRecord {
                name: "read_file".into(), ok: false, ms: 0, error: None,
                input_bytes: None, output_bytes: None, args_preview: None,
                result_preview: Some("Skipped: the skill already completed this work. Do NOT call `read_file` again.".into()),
                file_path: None, surgically_removed: None, original_tool_name: None, ..Default::default()
            };
            let deferred = ToolCallRecord {
                name: "bash".into(),
                ok: false,
                ms: 0,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: Some(
                    "Deferred: skill was invoked in this turn. Read the skill instructions above."
                        .into(),
                ),
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            };
            let dedup = ToolCallRecord {
                name: "skill".into(), ok: false, ms: 0, error: None,
                input_bytes: None, output_bytes: None, args_preview: None,
                result_preview: Some("Skill 'debug' was already loaded (turn 2). Follow those instructions directly.".into()),
                file_path: None, surgically_removed: None, original_tool_name: None, ..Default::default()
            };
            let invalid_delegate = ToolCallRecord {
                name: "delegate".into(),
                ok: false,
                ms: 0,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: Some(
                    "Invalid delegation request: agents must be a non-empty array.".into(),
                ),
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            };
            let actual_failure = ToolCallRecord {
                name: "skill".into(),
                ok: false,
                ms: 0,
                args_preview: None,
                error: Some("Unknown skill".into()),
                input_bytes: None,
                output_bytes: None,
                result_preview: Some("Unknown skill 'debug'.".into()),
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            };
            assert!(!skipped.is_synthetic_placeholder());
            assert!(!deferred.is_synthetic_placeholder());
            assert!(!dedup.is_synthetic_placeholder());
            assert!(!invalid_delegate.is_synthetic_placeholder());
            assert!(!actual_failure.is_synthetic_placeholder());
            assert!(
                ToolCallRecord {
                    name: "read_file".into(),
                    ok: true,
                    ms: 0,
                    result_class: Some(NOOP_OR_CACHED_RESULT_CLASS.to_string()),
                    ..Default::default()
                }
                .is_synthetic_placeholder(),
                "synthetic placeholder classification must come from structured fields"
            );
        }

        // ── is_synthetic_placeholder ── all patterns
        {
            let unflagged_sentinel = base_tool_record(
                SURGICAL_REMOVAL_TOOL_NAME,
                true,
                Some("(removed from context - skill covered this work)"),
            );
            assert!(
                !unflagged_sentinel.is_synthetic_placeholder(),
                "sentinel name alone must not be treated as a supported synthetic marker"
            );
            assert!(
                !base_tool_record("read_file", false, Some("Skipped: skill routed"))
                    .is_synthetic_placeholder(),
                "human-readable skipped text must not classify infrastructure state"
            );
            assert!(
                !base_tool_record("read_file", false, Some("Deferred: skill invoked"))
                    .is_synthetic_placeholder(),
                "human-readable deferred text must not classify infrastructure state"
            );
            let deferred_protocol_failure = ToolCallRecord {
                name: "agent_fanout".into(),
                ok: false,
                ms: 0,
                error: Some("tool_not_admitted".into()),
                result_preview: Some(
                    "Deferred: Error: Tool 'agent_fanout' is not available in this turn yet."
                        .into(),
                ),
                ..Default::default()
            };
            assert!(
                !deferred_protocol_failure.is_synthetic_placeholder(),
                "not-admitted deferred calls are protocol failures, not synthetic placeholders"
            );
            assert!(
                !base_tool_record(
                    "skill",
                    true,
                    Some("Skill 'debug' was already loaded (turn 2). Follow those instructions.")
                )
                .is_synthetic_placeholder(),
                "human-readable skill reentry text must not classify infrastructure state"
            );
            assert!(!base_tool_record("git", true, Some("diff")).is_synthetic_placeholder());
            assert!(
                !base_tool_record("grep", false, Some("error: bad regex"))
                    .is_synthetic_placeholder()
            );
            assert!(!base_tool_record("read_file", true, None).is_synthetic_placeholder());
            let flagged = ToolCallRecord {
                name: "read_file".to_string(),
                ok: true,
                ms: 50,
                surgically_removed: Some(true),
                original_tool_name: Some("read_file".to_string()),
                result_preview: Some("content".into()),
                output_bytes: Some(100),
                ..Default::default()
            };
            assert!(
                flagged.is_synthetic_placeholder(),
                "surgically_removed=true must classify as synthetic"
            );
        }

        // ── was_blocked_by_policy ──
        {
            assert!(ToolCallRecord {
                name: "read_file".to_string(), ok: false,
                error: Some("blocked_tool: Tool 'read_file' is currently restricted and cannot be executed.".into()),
                result_class: Some(BLOCKED_TOOL_RESULT_CLASS.to_string()),
                ..Default::default()
            }.was_blocked_by_policy());
            assert!(
                !ToolCallRecord {
                    name: "read_file".to_string(),
                    ok: false,
                    error: Some("Error: file not found".into()),
                    ..Default::default()
                }
                .was_blocked_by_policy()
            );
            assert!(
                !ToolCallRecord {
                    name: "read_file".to_string(),
                    ok: true,
                    error: None,
                    ..Default::default()
                }
                .was_blocked_by_policy()
            );
        }

        // ── is_noop_or_cached_result ──
        {
            assert!(
                ToolCallRecord {
                    name: "read_file".to_string(),
                    ok: true,
                    result_class: Some(NOOP_OR_CACHED_RESULT_CLASS.to_string()),
                    ..Default::default()
                }
                .is_noop_or_cached_result()
            );
            assert!(
                !ToolCallRecord {
                    name: "read_file".to_string(),
                    ok: true,
                    error: Some("cached_cross_turn".into()),
                    result_preview: Some("[cached_cross_turn: reused 200 bytes]".into()),
                    ..Default::default()
                }
                .is_noop_or_cached_result(),
                "error/result text alone must not drive infrastructure classification"
            );
            assert!(
                !(ToolCallRecord {
                    name: "nope".to_string(),
                    ok: false,
                    error: Some("unknown_tool: nope".to_string()),
                    ..Default::default()
                }
                .is_noop_or_cached_result())
            );
            assert!(
                !base_tool_record(
                    "read_file",
                    true,
                    Some(
                        "[File already fully read earlier in this turn and unchanged — refer to the earlier read_file result]"
                    )
                )
                .is_noop_or_cached_result(),
                "human-readable result text must not drive infrastructure classification"
            );
            assert!(
                !base_tool_record(
                    "bash",
                    false,
                    Some("Cached repeat skipped (call #3 for identical args, limit: 2).")
                )
                .is_noop_or_cached_result(),
                "human-readable error text must not drive infrastructure classification"
            );
            assert!(
                !base_tool_record("read_file", true, Some("fn main() {}"))
                    .is_noop_or_cached_result()
            );
        }

        // ── classify_session_end_state: Completed ──
        {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = format!("test-classify-{}", uuid::Uuid::new_v4());
            let w = JournalWriter::new(&sid).unwrap();
            w.append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
                .unwrap();
            w.append(&JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "fix auth flow",
                "I checked the login path.",
                0,
                10,
                5,
                10,
            ))
            .unwrap();
            w.append(&JournalEvent::session_end(Some(&sid), 1)).unwrap();
            assert_eq!(
                classify_session_end_state(&sid).unwrap(),
                SessionEndState::Completed
            );
        }

        // ── classify_session_end_state: Interrupted (resumable: rate_limited) ──
        {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = format!("test-classify-{}", uuid::Uuid::new_v4());
            let w = JournalWriter::new(&sid).unwrap();
            w.append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
                .unwrap();
            w.append(&JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "continue the migration",
                "I finished the schema diff.",
                0,
                10,
                5,
                10,
            ))
            .unwrap();
            w.append(&JournalEvent::interruption_recorded(Some(&sid), 1, serde_json::json!({
                "kind": "rate_limited", "resume_action": {"wait_and_retry": {"delay_seconds": 30}},
                "has_checkpoint": true, "tool_calls_completed": 2, "turns_completed": 1, "remaining_turns": 4
            }))).unwrap();
            assert_eq!(
                classify_session_end_state(&sid).unwrap(),
                SessionEndState::Interrupted {
                    kind: "rate_limited".to_string(),
                    resumable: true
                }
            );
        }

        // ── classify_session_end_state: Interrupted (non-resumable: auth_failure) ──
        {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = format!("test-classify-{}", uuid::Uuid::new_v4());
            let w = JournalWriter::new(&sid).unwrap();
            w.append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
                .unwrap();
            w.append(&JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "fetch CI logs",
                "Need valid credentials first.",
                0,
                10,
                5,
                10,
            ))
            .unwrap();
            w.append(&JournalEvent::interruption_recorded(Some(&sid), 1, serde_json::json!({
                "kind": "auth_failure", "resume_action": {"requires_intervention": {"description": "refresh credentials"}},
                "has_checkpoint": true
            }))).unwrap();
            assert_eq!(
                classify_session_end_state(&sid).unwrap(),
                SessionEndState::Interrupted {
                    kind: "auth_failure".to_string(),
                    resumable: false
                }
            );
        }

        // ── classify_session_end_state: plan_mid_flight ──
        {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = format!("test-classify-{}", uuid::Uuid::new_v4());
            let w = JournalWriter::new(&sid).unwrap();
            w.append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
                .unwrap();
            w.append(&JournalEvent::plan_lifecycle(
                Some(&sid),
                "execution_started",
                Some(serde_json::json!({"plan_id": "p-abc", "subtask_count": 3})),
            ))
            .unwrap();
            w.append(&JournalEvent::plan_progress(
                Some(&sid),
                1,
                "s1",
                "first step",
                "completed",
                33,
                3,
                1,
            ))
            .unwrap();
            w.append(&JournalEvent::session_end(Some(&sid), 1)).unwrap();
            match classify_session_end_state(&sid).unwrap() {
                SessionEndState::Interrupted { kind, resumable } => {
                    assert_eq!(kind, "plan_mid_flight");
                    assert!(resumable);
                }
                other => panic!("expected Interrupted(plan_mid_flight), got {other:?}"),
            }
        }

        // ── classify_session_end_state: plan completed before session_end ──
        {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = format!("test-classify-{}", uuid::Uuid::new_v4());
            let w = JournalWriter::new(&sid).unwrap();
            w.append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
                .unwrap();
            w.append(&JournalEvent::plan_lifecycle(
                Some(&sid),
                "execution_started",
                Some(serde_json::json!({"plan_id": "p-ok"})),
            ))
            .unwrap();
            w.append(&JournalEvent::plan_lifecycle(
                Some(&sid),
                "plan_completed",
                Some(serde_json::json!({"plan_id": "p-ok"})),
            ))
            .unwrap();
            w.append(&JournalEvent::session_end(Some(&sid), 1)).unwrap();
            assert_eq!(
                classify_session_end_state(&sid).unwrap(),
                SessionEndState::Completed
            );
        }

        // ── classify_session_end_state: plan abandoned → Completed ──
        {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = format!("test-classify-{}", uuid::Uuid::new_v4());
            let w = JournalWriter::new(&sid).unwrap();
            w.append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
                .unwrap();
            w.append(&JournalEvent::plan_lifecycle(
                Some(&sid),
                "execution_started",
                Some(serde_json::json!({"plan_id": "p-ab"})),
            ))
            .unwrap();
            w.append(&JournalEvent::plan_lifecycle(
                Some(&sid),
                "plan_abandoned",
                Some(serde_json::json!({"plan_id": "p-ab"})),
            ))
            .unwrap();
            w.append(&JournalEvent::session_end(Some(&sid), 1)).unwrap();
            assert_eq!(
                classify_session_end_state(&sid).unwrap(),
                SessionEndState::Completed
            );
        }

        // ── classify_session_end_state: Zombie (no session_end) ──
        {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = format!("test-classify-{}", uuid::Uuid::new_v4());
            let w = JournalWriter::new(&sid).unwrap();
            w.append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
                .unwrap();
            w.append(&JournalEvent::plan_progress(
                Some(&sid),
                1,
                "task-1",
                "Implement restart flow",
                "started",
                33,
                3,
                1,
            ))
            .unwrap();
            assert_eq!(
                classify_session_end_state(&sid).unwrap(),
                SessionEndState::Zombie
            );
        }

        // ── journal_event_new_fields serialized only when set ──
        {
            let ev = JournalEvent::base_public(JournalEventType::Turn, Some("s1"));
            let json = serde_json::to_string(&ev).unwrap();
            assert!(!json.contains("\"round\""));
            assert!(!json.contains("tool_calls_returned"));
            assert!(!json.contains("offset_ms"));
            assert!(!json.contains("llm_rounds"));
            assert!(!json.contains("total_llm_ms"));
            assert!(!json.contains("total_tool_ms"));
        }
    }
}

#[cfg(test)]
mod turn_event_buffer_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn begin_turn_initializes_round_zero() {
        let buf = TurnEventBuffer::begin_turn(Some("sess-1"), 3);
        assert_eq!(buf.current_round(), 0);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn begin_turn_with_round_uses_provided_round() {
        let buf = TurnEventBuffer::begin_turn_with_round(Some("sess-1"), 3, 4);
        assert_eq!(buf.current_round(), 4);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn late_session_binding_retrofits_first_round_events() {
        let mut buf = TurnEventBuffer::begin_turn(None, 1);
        buf.record_llm_round(LlmRoundRecord {
            duration_ms: 20,
            prompt_tokens: 100,
            completion_tokens: 10,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        buf.record(JournalEvent::base_public(JournalEventType::TraceSpan, None));

        buf.bind_session_id("session-streamed").unwrap();
        let events = buf.drain();
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| { event.session_id.as_deref() == Some("session-streamed") })
        );
    }

    #[test]
    fn late_session_binding_rejects_identity_conflicts_without_rewriting_events() {
        let mut buf = TurnEventBuffer::begin_turn(None, 1);
        buf.record(JournalEvent::base_public(
            JournalEventType::TraceSpan,
            Some("different-session"),
        ));

        buf.bind_session_id("session-streamed")
            .expect_err("conflicting event identity must reject the late binding");
        let events = buf.drain();
        assert_eq!(events[0].session_id.as_deref(), Some("different-session"));
    }

    #[test]
    fn record_llm_round_advances_round_counter() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-1"), 1);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(100),
            duration_ms: 500,
            prompt_tokens: 1000,
            completion_tokens: 200,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 2,
            tool_call_names: vec!["read_file".into(), "grep".into()],
            finish_reason: Some("tool_calls".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        assert_eq!(buf.current_round(), 1);
        assert_eq!(buf.len(), 1);

        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 300,
            prompt_tokens: 2000,
            completion_tokens: 100,
            cache_read_tokens: 500,
            cache_creation_tokens: 0,
            tool_calls_returned: 1,
            tool_call_names: vec!["write_file".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        assert_eq!(buf.current_round(), 2);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn llm_round_persists_exact_visible_action_union() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-actions"), 1);
        buf.set_visible_tool_names(vec![
            "start_work".to_string(),
            "settle_work_item".to_string(),
            "settle_work_item".to_string(),
        ]);
        buf.set_visible_tool_actions(std::collections::BTreeMap::from([(
            "agent_fanout".to_string(),
            vec!["get_results".to_string(), "stop_group".to_string()],
        )]));
        buf.record_llm_round(LlmRoundRecord::new(InferencePurpose::PrimaryAgent));

        let event = buf.drain().into_iter().next().expect("llm round event");
        assert_eq!(
            event.metadata.as_ref().expect("metadata")["visible_tool_actions"]["agent_fanout"],
            serde_json::json!(["get_results", "stop_group"])
        );
        assert_eq!(
            event.metadata.as_ref().expect("metadata")["visible_tools"],
            serde_json::json!(["start_work", "settle_work_item"]),
            "round evidence keeps exact tool names even when a lifecycle tool has no action enum"
        );
    }

    #[test]
    fn recorded_llm_round_event_has_correct_fields() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-1"), 5);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(42),
            duration_ms: 800,
            prompt_tokens: 3000,
            completion_tokens: 400,
            cache_read_tokens: 1000,
            cache_creation_tokens: 0,
            tool_calls_returned: 3,
            tool_call_names: vec!["a".into(), "b".into(), "c".into()],
            finish_reason: Some("tool_calls".into()),
            agentic_step: Some(4),
            source: Some("agentic_loop".into()),
            run_id: Some("run-42".into()),
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::SubAgent)
        });
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.event_type, JournalEventType::LlmRound);
        assert_eq!(ev.turn, Some(5));
        assert_eq!(ev.agentic_step, Some(4));
        assert_eq!(ev.round, Some(0));
        assert_eq!(ev.ttft_ms, Some(42));
        assert_eq!(ev.tokens_in, Some(3000));
        assert_eq!(ev.tokens_out, Some(400));
        assert_eq!(ev.cache_read_tokens, Some(1000));
        assert_eq!(ev.tool_calls_returned, Some(3));
        let meta = ev.metadata.as_ref().unwrap();
        assert_eq!(meta["tool_call_names"].as_array().unwrap().len(), 3);
        assert_eq!(meta["duration_ms"], 800);
        assert_eq!(meta["ttft_ms"], 42);
        assert_eq!(meta["prompt_tokens"], 3000);
        assert_eq!(meta["completion_tokens"], 400);
        assert_eq!(meta["cache_read_tokens"], 1000);
        assert_eq!(meta["source"], "agentic_loop");
        assert_eq!(
            ev.producer_scope
                .as_ref()
                .map(|scope| scope.run_id.as_str()),
            Some("run-42")
        );
        assert!(meta.get("run_id").is_none());
        assert_eq!(
            serde_json::from_value::<InferencePurpose>(meta["purpose"].clone())
                .expect("typed inference purpose"),
            InferencePurpose::SubAgent
        );
    }

    #[test]
    fn recorded_llm_round_omits_absent_optional_metadata() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-1"), 1);
        buf.record_llm_round(LlmRoundRecord::new(InferencePurpose::MemoryExtraction));

        let events = buf.drain();
        assert_eq!(
            events[0].metadata,
            Some(serde_json::json!({"purpose": "memory_extraction"}))
        );
    }

    #[test]
    fn recorded_llm_round_event_can_embed_tool_calls() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-embed"), 2);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(10),
            duration_ms: 200,
            prompt_tokens: 100,
            completion_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 1,
            tool_call_names: vec!["git".into()],
            finish_reason: Some("tool_calls".into()),
            agentic_step: Some(1),
            source: Some("agentic_loop".into()),
            run_id: Some("run-embed".into()),
            tool_calls: Some(vec![ToolCallRecord {
                name: "git".into(),
                ok: true,
                ms: 50,
                args_full: Some("{\"action\":\"diff\",\"stat_only\":true}".into()),
                result_preview: Some("diff --git ...".into()),
                round: Some(0),
                ..Default::default()
            }]),
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        let events = buf.drain();
        let ev = &events[0];
        let tool_calls = ev.tool_calls.as_ref().expect("embedded tool calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "git");
        assert_eq!(
            tool_calls[0].args_full.as_deref(),
            Some("{\"action\":\"diff\",\"stat_only\":true}")
        );
    }

    #[test]
    fn tool_call_record_keeps_live_authority_out_of_durable_json() {
        let raw = r#"{"command":"tool --token secret-token-value"}"#;
        let safe = r#"{"command":"tool --token [REDACTED:TOKEN_ARGUMENT]"}"#;
        let record = ToolCallRecord {
            name: "bash".into(),
            args_full: Some(safe.into()),
            runtime_args_full: Some(raw.into()),
            ..Default::default()
        };

        assert_eq!(record.authoritative_args_full(), Some(raw));
        let encoded = serde_json::to_string(&record).expect("record serializes");
        assert!(encoded.contains("[REDACTED:TOKEN_ARGUMENT]"));
        assert!(!encoded.contains("secret-token-value"));
        let debug = format!("{record:?}");
        assert!(!debug.contains("secret-token-value"));

        let restored: ToolCallRecord = serde_json::from_str(&encoded).expect("record restores");
        assert_eq!(restored.runtime_args_full, None);
        assert_eq!(restored.authoritative_args_full(), None);
    }

    #[test]
    fn next_batch_id_includes_round() {
        let mut buf = TurnEventBuffer::begin_turn(Some("s"), 0);
        assert_eq!(buf.next_batch_id(), "b-0-0");
        assert_eq!(buf.next_batch_id(), "b-0-1");
        // Advance round
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        assert_eq!(buf.next_batch_id(), "b-1-0");
    }

    /// Regression: llm_round events must carry the session-level turn number,
    /// not the internal agentic loop iteration count.
    #[test]
    fn llm_round_turn_uses_session_turn_number() {
        // Simulate session turn 7 (the 7th user message in the session)
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-turn"), 7);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(100),
            duration_ms: 500,
            prompt_tokens: 5000,
            completion_tokens: 200,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        let events = buf.drain();
        assert_eq!(
            events[0].turn,
            Some(7),
            "llm_round must use session turn number"
        );
    }

    /// Regression: text-only LLM responses (no tool calls) must still record
    /// an llm_round event so llm_rounds count is correct.
    #[test]
    fn text_only_response_records_llm_round() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-text"), 3);
        // Simulate a text-only response (0 tool calls)
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(48521),
            duration_ms: 120000,
            prompt_tokens: 24829,
            completion_tokens: 1281,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        assert_eq!(
            buf.current_round(),
            1,
            "round must advance even for text-only"
        );
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens_in, Some(24829));
        assert_eq!(events[0].tool_calls_returned, Some(0));
    }

    /// Regression: sub-call LLM rounds (e.g. headless/sub-run) must record
    /// their finish_reason + source and typed producer identity so the
    /// per-round token breakdown can be attributed to its originating run.
    #[test]
    fn llm_round_preserves_finish_reason_and_source_metadata() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-refl"), 2);
        // Normal round
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(100),
            duration_ms: 500,
            prompt_tokens: 10000,
            completion_tokens: 200,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 1,
            tool_call_names: vec!["read_file".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        // Tagged sub-call round
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 0,
            prompt_tokens: 54000,
            completion_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("sub_call".into()),
            agentic_step: None,
            source: Some("sub_call".into()),
            run_id: Some("run-subcall".into()),
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        assert_eq!(buf.current_round(), 2);
        let events = buf.drain();
        assert_eq!(events.len(), 2);
        let refl = &events[1];
        assert_eq!(refl.round, Some(1));
        assert_eq!(refl.tokens_in, Some(54000));
        let refl_meta = refl.metadata.as_ref().unwrap();
        assert_eq!(refl_meta["source"], "sub_call");
        assert_eq!(
            refl.producer_scope
                .as_ref()
                .map(|scope| scope.run_id.as_str()),
            Some("run-subcall")
        );
        assert!(refl_meta.get("run_id").is_none());
    }

    #[test]
    fn producer_turn_keeps_child_counter_out_of_root_turn_namespace() {
        let mut buf = TurnEventBuffer::begin_producer_turn(Some("sess-child"), 7);
        buf.record_llm_round(LlmRoundRecord {
            prompt_tokens: 100,
            completion_tokens: 20,
            source: Some("child_agent".into()),
            run_id: Some("child-run".into()),
            parent_run_id: Some("root-run".into()),
            agent_id: Some("agent-1".into()),
            ..LlmRoundRecord::new(InferencePurpose::SubAgent)
        });

        let event = buf.drain().pop().expect("child llm round");
        assert_eq!(event.turn, None, "child counter is not a session turn");
        let scope = event.producer_scope.expect("typed producer scope");
        assert_eq!(scope.run_id, "child-run");
        assert_eq!(scope.parent_run_id.as_deref(), Some("root-run"));
        assert_eq!(scope.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(scope.local_turn, Some(7));
        let metadata = event.metadata.expect("source metadata");
        assert_eq!(metadata["source"], "child_agent");
        assert!(metadata.get("run_id").is_none());
        assert!(metadata.get("agent_id").is_none());
    }

    /// Regression: rate-limited early exit must record an llm_round with
    /// finish_reason so the journal reflects the LLM call happened.
    #[test]
    fn rate_limited_round_records_finish_reason() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-rl"), 2);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(50),
            duration_ms: 200,
            prompt_tokens: 8000,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("rate_limited".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        let meta = events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["finish_reason"], "rate_limited");
        assert_eq!(events[0].tool_calls_returned, Some(0));
    }

    /// Regression: token-budget-exceeded early exit must record an llm_round.
    #[test]
    fn token_budget_exceeded_round_records_finish_reason() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-tb"), 5);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 100,
            prompt_tokens: 128000,
            completion_tokens: 50,
            cache_read_tokens: 64000,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("token_budget_exceeded".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        let meta = events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["finish_reason"], "token_budget_exceeded");
        assert_eq!(events[0].tokens_in, Some(128000));
        assert_eq!(events[0].cache_read_tokens, Some(64000));
    }

    #[test]
    fn flush_writes_events_to_journal() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-flush").unwrap();

        let mut buf = TurnEventBuffer::begin_turn(Some("sess-flush"), 1);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 100,
            prompt_tokens: 500,
            completion_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 1,
            tool_call_names: vec!["bash".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        buf.record(JournalEvent::base_public(
            JournalEventType::Turn,
            Some("sess-flush"),
        ));
        assert_eq!(buf.len(), 2);

        buf.flush(&writer).unwrap();
        assert!(buf.is_empty());

        // Verify written to disk — SessionStart is auto-prepended
        let content = std::fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        let ev0: JournalEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(ev0.event_type, JournalEventType::SessionStart);
        let ev1: JournalEvent = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(ev1.event_type, JournalEventType::LlmRound);
        let ev2: JournalEvent = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(ev2.event_type, JournalEventType::Turn);
    }

    #[test]
    fn flush_interrupted_marks_events_partial() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-interrupted").unwrap();

        let mut buf = TurnEventBuffer::begin_turn(Some("sess-interrupted"), 1);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(50),
            duration_ms: 200,
            prompt_tokens: 1000,
            completion_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 2,
            tool_call_names: vec!["read_file".into(), "grep".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });

        buf.flush_interrupted(&writer).unwrap();
        assert!(buf.is_empty());

        let content = std::fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let ev0: JournalEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(ev0.event_type, JournalEventType::SessionStart);
        let ev: JournalEvent = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(ev.event_type, JournalEventType::LlmRound);
        let partial = ev
            .metadata
            .as_ref()
            .and_then(|m| m.get("partial"))
            .and_then(|v| v.as_bool());
        assert_eq!(partial, Some(true));
    }

    #[test]
    fn flush_empty_buffer_is_noop() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-empty").unwrap();

        let mut buf = TurnEventBuffer::begin_turn(Some("sess-empty"), 1);
        buf.flush(&writer).unwrap();
        // File should not exist (no events written)
        assert!(!writer.path().exists());
    }

    #[test]
    fn drain_returns_events_and_clears_buffer() {
        let mut buf = TurnEventBuffer::begin_turn(Some("s"), 0);
        buf.record(JournalEvent::base_public(JournalEventType::Turn, Some("s")));
        buf.record(JournalEvent::base_public(JournalEventType::Turn, Some("s")));
        assert_eq!(buf.len(), 2);
        let drained = buf.drain();
        assert_eq!(drained.len(), 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_exposes_evicted_event_count() {
        let mut buf = TurnEventBuffer::begin_turn(Some("s"), 0);
        for _ in 0..(TURN_EVENT_BUFFER_CAP + 5) {
            buf.record(JournalEvent::base_public(JournalEventType::Turn, Some("s")));
        }

        assert_eq!(buf.len(), TURN_EVENT_BUFFER_CAP);
        assert_eq!(buf.dropped_events(), 5);

        let drained = buf.drain();
        assert_eq!(drained.len(), TURN_EVENT_BUFFER_CAP);
        assert_eq!(
            drained[0]
                .metadata
                .as_ref()
                .and_then(|meta| meta.get(TURN_EVENT_DROPPED_META_KEY))
                .and_then(|value| value.as_u64()),
            Some(5)
        );
        assert_eq!(buf.dropped_events(), 0);
    }

    #[test]
    fn append_bulk_writes_multiple_events_atomically() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-bulk").unwrap();

        let events = vec![
            JournalEvent::session_start(Some("sess-bulk"), Some("gpt-4")),
            JournalEvent::base_public(JournalEventType::Turn, Some("sess-bulk")),
            JournalEvent::session_end(Some("sess-bulk"), 1),
        ];
        writer.append_bulk(&events).unwrap();

        let content = std::fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn durable_readback_surfaces_sync_fence_failure() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-durable-readback-sync-fail").unwrap();
        writer
            .append(&JournalEvent::session_start(
                Some("sess-durable-readback-sync-fail"),
                Some("test-model"),
            ))
            .unwrap();

        let error = writer
            .durable_readback_with(|_| {
                Err(std::io::Error::other("injected durability fence failure"))
            })
            .expect_err("a failed sync fence must make readback uncertain");
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(
            error
                .to_string()
                .contains("injected durability fence failure")
        );
    }

    #[test]
    fn durable_readback_rejects_exact_json_without_terminal_newline() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-durable-readback-no-newline").unwrap();
        let event = JournalEvent::session_start(
            Some("sess-durable-readback-no-newline"),
            Some("test-model"),
        );
        std::fs::write(writer.path(), serde_json::to_vec(&event).unwrap()).unwrap();

        let error = writer
            .durable_readback()
            .expect_err("an unterminated tail cannot prove a durable complete record");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unterminated tail"));

        let commit = canonical_cas_test_commit("sess-durable-readback-no-newline", "next");
        let event =
            canonical_cas_test_event("sess-durable-readback-no-newline", "next", commit.clone());
        let lease = SessionExecutionLease::try_acquire("sess-durable-readback-no-newline").unwrap();
        assert!(matches!(
            writer.append_canonical_commit_cas(&lease, None, 1, &commit, &[event]),
            CanonicalCommitCasOutcome::Unknown(reason) if reason.contains("incomplete")
        ));
    }

    fn canonical_cas_test_commit(session_id: &str, marker: &str) -> ConversationCommitV1 {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": marker,
        })];
        ConversationCommitV1 {
            schema_version: astra_turn_types::CONVERSATION_COMMIT_SCHEMA_VERSION,
            base_root_hash: astra_turn_types::canonical_conversation_root(&[]),
            cursor: SessionCursorV1 {
                schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
                owner_id: "owner-cas-test".to_string(),
                session_id: session_id.to_string(),
                branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.to_string(),
                completed_turn: 1,
                journal_event_seq: 1,
                conversation_seq: 1,
                canonical_root_hash: astra_turn_types::canonical_conversation_root(&messages),
                projection_schema: astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION,
                compaction_generation: 0,
                config_version_id: None,
            },
            delta: astra_turn_types::ConversationDeltaV1::Append { messages },
        }
    }

    fn canonical_cas_test_event(
        session_id: &str,
        marker: &str,
        commit: ConversationCommitV1,
    ) -> JournalEvent {
        let mut event = JournalEvent::turn(
            Some(session_id),
            1,
            Some("test-model"),
            marker,
            marker,
            0,
            0,
            0,
            1,
        );
        event.conversation_commit = Some(commit);
        event
    }

    #[test]
    fn canonical_presence_accepts_one_exact_turn_without_self_conflict() {
        let sid = "sess-canonical-presence-exact";
        let commit = canonical_cas_test_commit(sid, "exact");
        let event = canonical_cas_test_event(sid, "exact", commit.clone());
        assert_eq!(
            classify_canonical_commit_presence(&[event], 0, 1, &commit),
            CanonicalCommitPresence::Exact
        );
    }

    #[test]
    fn session_execution_lease_rejects_second_instance_before_mock_invocation() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let _guard = JournalDirGuard::new(&dir);
        let session_id = "sess-execution-lease-admission";
        let invocation_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let winner_lease = SessionExecutionLease::try_acquire(session_id).unwrap();
        invocation_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let loser_dir = dir.clone();
        let loser_invocations = invocation_count.clone();
        let loser = std::thread::spawn(move || {
            let _guard = JournalDirGuard::new(&loser_dir);
            match SessionExecutionLease::try_acquire(session_id) {
                Ok(_lease) => {
                    loser_invocations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    false
                }
                Err(SessionExecutionLeaseError::Conflict { .. }) => true,
                Err(error) => panic!("unexpected lease error: {error}"),
            }
        });

        assert!(loser.join().unwrap());
        assert_eq!(
            invocation_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the losing process must be rejected before mock LLM/tool invocation"
        );
        drop(winner_lease);
        assert!(SessionExecutionLease::try_acquire(session_id).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn execution_lease_lock_inode_replacement_cannot_create_a_second_executor() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let session_id = "sess-execution-lease-inode-replacement";
        let invocation_count = std::sync::atomic::AtomicUsize::new(0);
        let first = SessionExecutionLease::try_acquire(session_id).unwrap();
        invocation_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let detached_lock_path = first.lock_path.with_extension("detached-lock");
        std::fs::rename(&first.lock_path, &detached_lock_path).unwrap();
        std::fs::write(&first.lock_path, b"replacement inode").unwrap();

        match SessionExecutionLease::try_acquire(session_id) {
            Ok(_second) => {
                invocation_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                panic!("a replacement lock inode must not admit a second executor");
            }
            Err(SessionExecutionLeaseError::Conflict {
                session_id: conflict,
            }) => {
                assert_eq!(conflict, session_id);
            }
            Err(error) => panic!("replacement must fail as a typed conflict: {error}"),
        }
        assert_eq!(
            invocation_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the competing executor must be rejected before its mock LLM/tool boundary"
        );

        drop(first);
        assert!(
            SessionExecutionLease::try_acquire(session_id).is_ok(),
            "dropping the kernel authority must allow the next per-turn owner"
        );
    }

    #[test]
    fn first_canonical_commit_requires_parent_directory_durability() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let session_id = "sess-canonical-parent-sync-failure";
        let writer = JournalWriter::new(session_id).unwrap();
        let lease = SessionExecutionLease::try_acquire(session_id).unwrap();
        let commit = canonical_cas_test_commit(session_id, "first");
        let event = canonical_cas_test_event(session_id, "first", commit.clone());
        fail_next_journal_directory_sync();

        assert!(matches!(
            writer.append_canonical_commit_cas(&lease, None, 1, &commit, &[event]),
            CanonicalCommitCasOutcome::Unknown(reason)
                if reason.contains("creation metadata")
                    && reason.contains("injected journal directory sync failure")
        ));
        assert_eq!(
            writer
                .complete_append_order_snapshot()
                .unwrap()
                .iter()
                .filter(|event| event.conversation_commit.is_some())
                .count(),
            1,
            "the bytes may exist, but authority must remain unknown when the directory fence fails"
        );
    }

    #[test]
    fn canonical_presence_marks_exact_plus_same_turn_conflict_unknown() {
        let sid = "sess-canonical-presence-conflict";
        let commit = canonical_cas_test_commit(sid, "exact");
        let exact = canonical_cas_test_event(sid, "exact", commit.clone());
        let conflicting = JournalEvent::turn(
            Some(sid),
            1,
            Some("test-model"),
            "different",
            "different",
            0,
            0,
            0,
            1,
        );
        assert!(matches!(
            classify_canonical_commit_presence(&[exact, conflicting], 0, 1, &commit),
            CanonicalCommitPresence::Unknown(_)
        ));
    }

    #[test]
    fn canonical_cas_rejects_turn_after_terminal_turn_error_identity() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let session_id = "sess-canonical-cas-turn-error";
        let writer = JournalWriter::new(session_id).unwrap();
        writer
            .append(&JournalEvent::turn_error(
                Some(session_id),
                1,
                Some("test-model"),
                "failed turn",
                "boom",
                1,
            ))
            .unwrap();
        let commit = canonical_cas_test_commit(session_id, "must-not-append");
        let event = canonical_cas_test_event(session_id, "must-not-append", commit.clone());
        let lease = SessionExecutionLease::try_acquire(session_id).unwrap();

        assert!(matches!(
            writer.append_canonical_commit_cas(&lease, None, 1, &commit, &[event]),
            CanonicalCommitCasOutcome::Conflict(reason) if reason.contains("already committed")
        ));
        let events = read_journal(session_id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.event_type,
                    JournalEventType::Turn | JournalEventType::TurnError
                ))
                .count(),
            1
        );
        assert_eq!(events[0].event_type, JournalEventType::SessionStart);
        assert_eq!(events[1].event_type, JournalEventType::TurnError);
    }

    #[test]
    fn canonical_cas_rejects_batch_with_turn_and_turn_error_terminal_identities() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let session_id = "sess-canonical-cas-invalid-terminal-batch";
        let writer = JournalWriter::new(session_id).unwrap();
        let lease = SessionExecutionLease::try_acquire(session_id).unwrap();
        let commit = canonical_cas_test_commit(session_id, "candidate");
        let candidate = canonical_cas_test_event(session_id, "candidate", commit.clone());
        let turn_error = JournalEvent::turn_error(
            Some(session_id),
            1,
            Some("test-model"),
            "candidate",
            "boom",
            1,
        );

        assert!(matches!(
            writer.append_canonical_commit_cas(
                &lease,
                None,
                1,
                &commit,
                &[turn_error, candidate],
            ),
            CanonicalCommitCasOutcome::NotCommitted(reason)
                if reason.contains("exactly one intended turn commit")
        ));
        assert!(writer.complete_append_order_snapshot().unwrap().is_empty());
    }

    #[test]
    fn concurrent_canonical_cas_writers_commit_exactly_one_same_turn() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let session_id = "sess-canonical-cas-race";
        let first_commit = canonical_cas_test_commit(session_id, "first");
        let second_commit = canonical_cas_test_commit(session_id, "second");
        let first_event = canonical_cas_test_event(session_id, "first", first_commit.clone());
        let second_event = canonical_cas_test_event(session_id, "second", second_commit.clone());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let outcomes = std::thread::scope(|scope| {
            let first_barrier = barrier.clone();
            let first_dir = dir.clone();
            let first = scope.spawn(move || {
                let _guard = JournalDirGuard::new(&first_dir);
                let writer = JournalWriter::new(session_id).unwrap();
                first_barrier.wait();
                writer.append_canonical_commit_cas_locked(
                    None,
                    1,
                    &first_commit,
                    &[first_event],
                    &[],
                )
            });
            let second_barrier = barrier.clone();
            let second_dir = dir.clone();
            let second = scope.spawn(move || {
                let _guard = JournalDirGuard::new(&second_dir);
                let writer = JournalWriter::new(session_id).unwrap();
                second_barrier.wait();
                writer.append_canonical_commit_cas_locked(
                    None,
                    1,
                    &second_commit,
                    &[second_event],
                    &[],
                )
            });
            vec![first.join().unwrap(), second.join().unwrap()]
        });

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, CanonicalCommitCasOutcome::Committed { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, CanonicalCommitCasOutcome::Conflict(_)))
                .count(),
            1
        );
        let _guard = JournalDirGuard::new(&dir);
        let turns = read_journal(session_id)
            .unwrap()
            .into_iter()
            .filter(|event| event.event_type == JournalEventType::Turn)
            .collect::<Vec<_>>();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn, Some(1));
    }

    #[test]
    fn canonical_cas_reports_unknown_without_writing_after_locked_inode_rotation() {
        use fs2::FileExt;

        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let _guard = JournalDirGuard::new(&dir);
        let session_id = "sess-canonical-cas-rotation";
        let writer = JournalWriter::new(session_id).unwrap();
        writer
            .append(&JournalEvent::session_start(
                Some(session_id),
                Some("old-generation"),
            ))
            .unwrap();
        let path = writer.path().clone();
        let detached_path = path.with_extension("detached.jsonl");
        let locked_old = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        locked_old.lock_exclusive().unwrap();

        let (opened_tx, opened_rx) = std::sync::mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
        *CANONICAL_COMMIT_CAS_OPEN_HOOK.lock().unwrap() = Some(CanonicalCommitCasOpenHook {
            path: path.clone(),
            opened: opened_tx,
            resume: resume_rx,
        });
        let commit = canonical_cas_test_commit(session_id, "current-generation");
        let event = canonical_cas_test_event(session_id, "current-generation", commit.clone());
        let thread_dir = dir.clone();
        let handle = std::thread::spawn(move || {
            let _guard = JournalDirGuard::new(&thread_dir);
            JournalWriter::new(session_id)
                .unwrap()
                .append_canonical_commit_cas_locked(None, 1, &commit, &[event], &[])
        });

        opened_rx.recv().unwrap();
        std::fs::rename(&path, &detached_path).unwrap();
        JournalWriter::new(session_id)
            .unwrap()
            .append(&JournalEvent::session_start(
                Some(session_id),
                Some("current-generation"),
            ))
            .unwrap();
        FileExt::unlock(&locked_old).unwrap();
        resume_tx.send(()).unwrap();

        assert!(matches!(
            handle.join().unwrap(),
            CanonicalCommitCasOutcome::Unknown(reason) if reason.contains("rotated")
        ));
        let current_events = read_journal(session_id).unwrap();
        assert_eq!(
            current_events
                .iter()
                .filter(|event| event.conversation_commit.is_some())
                .count(),
            0
        );
        let detached_content = std::fs::read_to_string(detached_path).unwrap();
        assert!(
            parse_journal_text(&detached_content)
                .0
                .iter()
                .all(|event| event.conversation_commit.is_none())
        );
    }

    /// Concurrent appends from multiple threads must remain record-separated.
    ///
    /// Regression for cancel-shutdown audit #2 fix: when the in-process
    /// `edge_callback_ledger` mutex was narrowed (so it no longer wrapped the
    /// journal write), two HTTP approval handlers could call
    /// `JournalWriter::append` simultaneously. The old implementation used
    /// `writeln!`, which issues the line and the trailing `\n` as **two**
    /// syscalls. With `O_APPEND`, that lost atomicity: two writers produced
    /// `{a}{b}\n\n` instead of `{a}\n{b}\n`, and the parser saw zero valid
    /// events. The fix is a single `write_all` of `line + "\n"`.
    #[test]
    fn concurrent_appends_remain_record_separated() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let session_id = "sess-concurrent-append";
        let n_threads = 8usize;
        let n_per_thread = 16usize;

        std::thread::scope(|scope| {
            for t in 0..n_threads {
                let dir = dir.clone();
                scope.spawn(move || {
                    let _guard = JournalDirGuard::new(&dir);
                    let writer = JournalWriter::new(session_id).unwrap();
                    for i in 0..n_per_thread {
                        let mut event =
                            JournalEvent::base_public(JournalEventType::Turn, Some(session_id));
                        event.user_input = Some(format!("t{t}-i{i}"));
                        writer.append(&event).unwrap();
                    }
                });
            }
        });

        let _guard = JournalDirGuard::new(&dir);
        let events = read_journal(session_id).unwrap();
        let session_start_count = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::SessionStart)
            .count();
        let turn_count = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::Turn)
            .count();
        assert_eq!(
            turn_count,
            n_threads * n_per_thread,
            "every concurrent append should produce one parseable turn record"
        );
        assert_eq!(
            session_start_count, 1,
            "concurrent first writes must not duplicate session_start"
        );
    }

    /// E2E regression test using real session data (a33177cc).
    ///
    /// Before the fix, llm_round events used 0-based turn numbers while
    /// turn events used 1-based numbers (state.turn += 1 happens before
    /// the turn event is written, but after stream_chat_sse returns).
    ///
    /// Real data (buggy):
    ///   llm_round turn=0  ← should be 1
    ///   turn      turn=1
    ///   llm_round turn=1  ← should be 2
    ///   llm_round turn=1  ← should be 2
    ///   turn      turn=2
    ///   llm_round turn=2  ← should be 3
    ///   turn      turn=3
    #[test]
    fn e2e_llm_round_turn_matches_turn_event() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-e2e-turn").unwrap();

        // Simulate 3 turns with the FIXED numbering (1-based).
        // Turn 1: "hi" — 1 round, text-only
        let mut buf1 = TurnEventBuffer::begin_turn(Some("sess-e2e-turn"), 1);
        buf1.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(988),
            duration_ms: 1831,
            prompt_tokens: 9375,
            completion_tokens: 11,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("stop".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        let obs1 = buf1.drain();
        writer.append_bulk(&obs1).unwrap();
        let turn1 = JournalEvent::turn(
            Some("sess-e2e-turn"),
            1,
            Some("qwen-turbo"),
            "hi",
            "你好！",
            0,
            9375,
            11,
            1831,
        );
        writer.append(&turn1).unwrap();

        // Turn 2: "描述一下这个项目" — 2 rounds, 1 tool call
        let mut buf2 = TurnEventBuffer::begin_turn(Some("sess-e2e-turn"), 2);
        buf2.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(2388),
            duration_ms: 3500,
            prompt_tokens: 10070,
            completion_tokens: 30,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 1,
            tool_call_names: vec!["read_file".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        buf2.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(1200),
            duration_ms: 7121,
            prompt_tokens: 19744,
            completion_tokens: 539,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("stop".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        let obs2 = buf2.drain();
        writer.append_bulk(&obs2).unwrap();
        let turn2 = JournalEvent::turn(
            Some("sess-e2e-turn"),
            2,
            Some("qwen-turbo"),
            "描述一下这个项目",
            "这个项目是...",
            1,
            29814,
            569,
            10621,
        );
        writer.append(&turn2).unwrap();

        // Turn 3: "review local changes" — 1 round, text-only (prefetch)
        let mut buf3 = TurnEventBuffer::begin_turn(Some("sess-e2e-turn"), 3);
        buf3.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(21633),
            duration_ms: 85243,
            prompt_tokens: 21454,
            completion_tokens: 1347,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("stop".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
            ..LlmRoundRecord::new(InferencePurpose::PrimaryAgent)
        });
        let obs3 = buf3.drain();
        writer.append_bulk(&obs3).unwrap();
        let turn3 = JournalEvent::turn(
            Some("sess-e2e-turn"),
            3,
            Some("qwen3.6-plus"),
            "review local changes",
            "Code review...",
            0,
            43815,
            3308,
            85243,
        );
        writer.append(&turn3).unwrap();

        // Parse back and verify consistency
        let content = std::fs::read_to_string(writer.path()).unwrap();
        let events: Vec<JournalEvent> = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let llm_rounds: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == JournalEventType::LlmRound)
            .collect();
        let turns: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == JournalEventType::Turn)
            .collect();

        assert_eq!(turns.len(), 3);
        assert_eq!(llm_rounds.len(), 4); // 1 + 2 + 1

        // Core invariant: every llm_round's turn must match its parent turn event
        // Turn 1 has 1 llm_round
        assert_eq!(llm_rounds[0].turn, Some(1), "llm_round[0] must be turn 1");
        assert_eq!(turns[0].turn, Some(1));

        // Turn 2 has 2 llm_rounds
        assert_eq!(llm_rounds[1].turn, Some(2), "llm_round[1] must be turn 2");
        assert_eq!(llm_rounds[2].turn, Some(2), "llm_round[2] must be turn 2");
        assert_eq!(turns[1].turn, Some(2));

        // Turn 3 has 1 llm_round
        assert_eq!(llm_rounds[3].turn, Some(3), "llm_round[3] must be turn 3");
        assert_eq!(turns[2].turn, Some(3));

        // Verify round numbers within each turn
        assert_eq!(llm_rounds[0].round, Some(0));
        assert_eq!(llm_rounds[1].round, Some(0));
        assert_eq!(llm_rounds[2].round, Some(1));
        assert_eq!(llm_rounds[3].round, Some(0));
    }

    /// Verify the needs_start_event logic for resumed sessions.
    #[test]
    fn needs_start_event_scenarios() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "needs-start-sid";

        // Empty journal → needs start
        assert!(journal_needs_session_start(sid).unwrap());

        // Clean end → needs start
        let events = [
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::base_public(JournalEventType::Turn, Some("s")),
            JournalEvent::session_end(Some("s"), 1),
        ];
        let writer = JournalWriter::new(sid).unwrap();
        writer.append_bulk(&events).unwrap();
        assert!(journal_needs_session_start(sid).unwrap());

        // Interrupted (start, turn, no end) → already has open start, skip
        let sid = "needs-start-open";
        let events = [
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::base_public(JournalEventType::Turn, Some("s")),
        ];
        let writer = JournalWriter::new(sid).unwrap();
        writer.append_bulk(&events).unwrap();
        assert!(!journal_needs_session_start(sid).unwrap());

        // start → end → start → turn (interrupted) → already has open start, skip
        let sid = "needs-start-nested";
        let events = [
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::session_end(Some("s"), 1),
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::base_public(JournalEventType::Turn, Some("s")),
        ];
        let writer = JournalWriter::new(sid).unwrap();
        writer.append_bulk(&events).unwrap();
        assert!(!journal_needs_session_start(sid).unwrap());

        // start → end → turn (orphan turn after clean end) → needs start
        let sid = "needs-start-orphan";
        let events = [
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::session_end(Some("s"), 1),
            JournalEvent::base_public(JournalEventType::Turn, Some("s")),
        ];
        let writer = JournalWriter::new(sid).unwrap();
        writer.append_bulk(&events).unwrap();
        assert!(journal_needs_session_start(sid).unwrap());
    }

    #[test]
    fn ensure_session_start_event_prepends_runtime_first_write() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "ensure-start-runtime-first-write";

        ensure_session_start_event(sid, Some("gpt-5")).unwrap();
        let writer = JournalWriter::new(sid).unwrap();
        writer
            .append(&JournalEvent::interruption_recorded(
                Some(sid),
                1,
                serde_json::json!({"kind":"budget_exhausted","resumable":true}),
            ))
            .unwrap();

        let events = read_journal(sid).unwrap();
        assert_eq!(events[0].event_type, JournalEventType::SessionStart);
        assert_eq!(events[1].event_type, JournalEventType::InterruptionRecorded);
    }

    #[test]
    fn writer_auto_prepends_session_start_for_first_non_start_event() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "auto-prepend-start-on-first-write";

        let writer = JournalWriter::new(sid).unwrap();
        writer
            .append(&JournalEvent::interruption_recorded(
                Some(sid),
                1,
                serde_json::json!({"kind":"manual_pause","resumable":true}),
            ))
            .unwrap();

        let events = read_journal(sid).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, JournalEventType::SessionStart);
        assert_eq!(events[1].event_type, JournalEventType::InterruptionRecorded);
    }

    #[test]
    fn auto_prepended_session_start_precedes_seed_timestamp() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "auto-prepend-start-timestamp";
        let path = journal_dir().join(format!("{sid}.jsonl"));
        let mut interruption = JournalEvent::interruption_recorded(
            Some(sid),
            1,
            serde_json::json!({"kind":"budget_exhausted","resumable":true}),
        );
        interruption.ts = "2026-01-01T00:00:02Z".to_string();

        let batch = [interruption];
        let prefixed = prepend_session_start_if_needed(&path, &batch).unwrap();
        assert_eq!(prefixed[0].event_type, JournalEventType::SessionStart);
        assert!(prefixed[0].ts < prefixed[1].ts);
    }

    #[test]
    fn auto_prepended_session_start_clones_ts_on_parse_failure() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "prepend-parse-failure";
        let path = journal_dir().join(format!("{sid}.jsonl"));
        let mut interruption =
            JournalEvent::interruption_recorded(Some(sid), 1, serde_json::json!({"kind":"test"}));
        interruption.ts = "not-a-valid-timestamp-2026".to_string();

        let batch = [interruption.clone()];
        let prefixed = prepend_session_start_if_needed(&path, &batch).unwrap();
        assert_eq!(prefixed[0].event_type, JournalEventType::SessionStart);
        // on parse failure, the SessionStart ts should match the seed ts rather
        // than Utc::now(), so the prepended event never ends up after the seed
        assert_eq!(prefixed[0].ts, prefixed[1].ts);
    }

    #[test]
    fn concurrent_first_writes_prepend_single_session_start() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let sid = "concurrent-prepend-single-start";

        std::thread::scope(|scope| {
            for turn in 1..=8 {
                let dir = dir.clone();
                scope.spawn(move || {
                    let _guard = JournalDirGuard::new(&dir);
                    let writer = JournalWriter::new(sid).unwrap();
                    writer
                        .append(&JournalEvent::interruption_recorded(
                            Some(sid),
                            turn,
                            serde_json::json!({"kind":"concurrent","turn":turn}),
                        ))
                        .unwrap();
                });
            }
        });

        let _guard = JournalDirGuard::new(&dir);
        let events = read_journal(sid).unwrap();
        let session_start_count = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::SessionStart)
            .count();
        let interruption_count = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::InterruptionRecorded)
            .count();
        assert_eq!(session_start_count, 1);
        assert_eq!(interruption_count, 8);
        assert_eq!(
            events.len(),
            9,
            "exactly 1 SessionStart + 8 InterruptionRecorded"
        );
        // SessionStart must be chronologically first (read_journal returns sorted).
        assert_eq!(events[0].event_type, JournalEventType::SessionStart);
        // All 8 interruption events must be present with distinct turns 1..=8.
        let mut turns: Vec<u32> = events[1..].iter().map(|e| e.turn.unwrap()).collect();
        turns.sort();
        assert_eq!(turns, (1..=8u32).collect::<Vec<_>>());
    }

    #[test]
    fn read_journal_returns_chronological_order_when_appends_drift() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "read-journal-chronological";
        let writer = JournalWriter::new(sid).unwrap();

        let mut later = JournalEvent::base_public(JournalEventType::ConfigChange, Some(sid));
        later.ts = "2026-01-01T00:00:02Z".to_string();
        let mut earlier = JournalEvent::base_public(JournalEventType::TraceSpan, Some(sid));
        earlier.ts = "2026-01-01T00:00:01Z".to_string();

        writer.append(&later).unwrap();
        writer.append(&earlier).unwrap();

        let events = read_journal(sid).unwrap();
        assert_eq!(events[0].event_type, JournalEventType::SessionStart);
        assert_eq!(events[1].event_type, JournalEventType::TraceSpan);
        assert_eq!(events[2].event_type, JournalEventType::ConfigChange);

        let append_order = read_journal_append_order(sid).unwrap();
        assert_eq!(append_order[0].event_type, JournalEventType::SessionStart);
        assert_eq!(append_order[1].event_type, JournalEventType::ConfigChange);
        assert_eq!(append_order[2].event_type, JournalEventType::TraceSpan);
    }
}

#[cfg(test)]
mod session_start_detection_tests {
    use super::*;

    #[test]
    fn session_start_detection_scenarios() {
        // --- needs session_start when file doesn't exist ---
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        std::fs::create_dir_all(journal_dir()).unwrap();
        let path = journal_dir().join("nonexistent-session.jsonl");
        assert!(journal_needs_session_start_for_path(&path).unwrap());

        // --- needs session_start when file is empty ---
        let path = journal_dir().join("empty-session.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(journal_needs_session_start_for_path(&path).unwrap());

        // --- does NOT need session_start when open session exists ---
        let sid = "open-session";
        let path = journal_dir().join(format!("{sid}.jsonl"));
        let writer = JournalWriter::new(sid).unwrap();
        writer
            .append(&JournalEvent::interruption_recorded(
                Some(sid),
                1,
                serde_json::json!({"kind": "test"}),
            ))
            .unwrap();
        assert!(!journal_needs_session_start_for_path(&path).unwrap());

        // --- needs session_start when last event is session_end ---
        let sid = "ended-session";
        let path = journal_dir().join(format!("{sid}.jsonl"));
        let writer = JournalWriter::new(sid).unwrap();
        writer
            .append(&JournalEvent::base_public(
                JournalEventType::SessionEnd,
                Some(sid),
            ))
            .unwrap();
        assert!(journal_needs_session_start_for_path(&path).unwrap());

        // --- read_last_event_type edge cases ---
        // handles missing trailing newline
        let path = journal_dir().join("no-trailing-nl.jsonl");
        let line1 = serde_json::to_string(&JournalEvent::base_public(
            JournalEventType::SessionStart,
            Some("s"),
        ))
        .unwrap();
        let line2 = serde_json::to_string(&JournalEvent::base_public(
            JournalEventType::Turn,
            Some("s"),
        ))
        .unwrap();
        std::fs::write(&path, format!("{line1}\n{line2}")).unwrap();
        assert_eq!(
            read_last_event_type(&path).unwrap(),
            Some(JournalEventType::Turn)
        );

        // returns None on garbage-only file
        let path = journal_dir().join("garbage.jsonl");
        std::fs::write(&path, "not-json\nalso-not-json\n").unwrap();
        assert_eq!(read_last_event_type(&path).unwrap(), None);

        // stitches event spanning chunk boundary
        let path = journal_dir().join("chunk-boundary.jsonl");
        let mut filler = JournalEvent::base_public(JournalEventType::SessionStart, Some("s"));
        filler.metadata = Some(serde_json::Value::String("x".repeat(8000)));
        let line1 = serde_json::to_string(&filler).unwrap();
        let line2 = serde_json::to_string(&JournalEvent::base_public(
            JournalEventType::Turn,
            Some("s"),
        ))
        .unwrap();
        std::fs::write(&path, format!("{line1}\n{line2}\n")).unwrap();
        assert!(line1.len() > RECOVERY_TAIL_CHUNK_BYTES);
        assert_eq!(
            read_last_event_type(&path).unwrap(),
            Some(JournalEventType::Turn)
        );

        // at exact chunk size
        let path = journal_dir().join("exact-chunk.jsonl");
        let event = JournalEvent::base_public(JournalEventType::Turn, Some("s"));
        let line = serde_json::to_string(&event).unwrap();
        let needed_padding = RECOVERY_TAIL_CHUNK_BYTES - (line.len() + 1);
        let padding = format!("{:width$}\n", "x", width = needed_padding - 1);
        std::fs::write(&path, format!("{padding}{line}\n")).unwrap();
        let actual_size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(actual_size as usize, RECOVERY_TAIL_CHUNK_BYTES);
        assert_eq!(
            read_last_event_type(&path).unwrap(),
            Some(JournalEventType::Turn)
        );
    }

    #[test]
    fn session_start_detection_bounded_io_on_large_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "perf-bounded-large-journal";
        let path = journal_dir().join(format!("{sid}.jsonl"));

        let writer = JournalWriter::new(sid).unwrap();
        writer
            .append(&JournalEvent::base_public(
                JournalEventType::SessionStart,
                Some(sid),
            ))
            .unwrap();
        use std::io::Write as _;
        let mut filler = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        for turn in 1..=30_000u32 {
            let mut e = JournalEvent::interruption_recorded(
                Some(sid),
                turn,
                serde_json::json!({"kind": "filler", "turn": turn}),
            );
            e.ts = format!("2026-01-01T00:{:02}:{:02}Z", (turn / 60) % 60, turn % 60);
            let mut line = serde_json::to_vec(&e).unwrap();
            line.push(b'\n');
            filler.write_all(&line).unwrap();
        }
        filler.sync_data().unwrap();
        drop(filler);

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(
            size > 1_500_000,
            "test prep should produce a multi-MB file, got {size} bytes"
        );

        let scan = read_last_event_type_with_bytes(&path).unwrap();
        assert_eq!(
            scan.event_type,
            Some(JournalEventType::InterruptionRecorded)
        );
        assert!(
            scan.bytes_read <= RECOVERY_TAIL_MAX_BYTES as u64,
            "must not exceed tail window: read {} of {} bytes",
            scan.bytes_read,
            size
        );
        assert!(
            scan.bytes_read <= RECOVERY_TAIL_CHUNK_BYTES as u64,
            "expected ≤ one chunk read on a healthy tail, got {} bytes",
            scan.bytes_read
        );

        let needs = journal_needs_session_start_impl(&path, /*skip_cache=*/ true).unwrap();
        assert!(!needs, "open session must not need another SessionStart");
    }

    #[test]
    fn stabilize_event_order_boundary_semantics() {
        let ts = "2026-01-01T00:00:00Z";
        let mut events: Vec<JournalEvent> = vec![
            {
                let mut e = JournalEvent::base_public(JournalEventType::Turn, Some("s"));
                e.ts = ts.to_string();
                e
            },
            {
                let mut e = JournalEvent::base_public(JournalEventType::SessionEnd, Some("s"));
                e.ts = ts.to_string();
                e
            },
            {
                let mut e = JournalEvent::base_public(JournalEventType::Turn, Some("s"));
                e.ts = ts.to_string();
                e
            },
        ];
        stabilize_event_order(&mut events);
        assert_eq!(
            events.last().unwrap().event_type,
            JournalEventType::SessionEnd
        );

        let mut events: Vec<JournalEvent> = vec![
            {
                let mut e = JournalEvent::base_public(JournalEventType::Turn, Some("s"));
                e.ts = ts.to_string();
                e
            },
            {
                let mut e = JournalEvent::base_public(JournalEventType::SessionEnd, Some("s"));
                e.ts = ts.to_string();
                e
            },
            {
                let mut e = JournalEvent::base_public(JournalEventType::Turn, Some("s"));
                e.ts = ts.to_string();
                e
            },
            {
                let mut e = JournalEvent::base_public(JournalEventType::SessionStart, Some("s"));
                e.ts = ts.to_string();
                e
            },
        ];
        stabilize_event_order(&mut events);
        assert_eq!(events[0].event_type, JournalEventType::SessionStart);
        assert_eq!(
            events.last().unwrap().event_type,
            JournalEventType::SessionEnd
        );
    }

    #[cfg(unix)]
    #[test]
    fn journal_file_permission_contracts() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        std::fs::create_dir_all(journal_dir()).unwrap();

        // Does NOT chmod when file already exists
        let path = journal_dir().join("preexisting.jsonl");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let _file = open_locked_journal_file(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );

        // Chmods to 0o600 on creation
        let path = journal_dir().join("brand-new.jsonl");
        let _file = open_locked_journal_file(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Writer does NOT re-chmod on each append
        let sid = "chmod-hot-path";
        let path = journal_dir().join(format!("{sid}.jsonl"));
        let writer = JournalWriter::new(sid).unwrap();
        writer
            .append(&JournalEvent::base_public(
                JournalEventType::SessionStart,
                Some(sid),
            ))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        for turn in 1..=4 {
            writer
                .append(&JournalEvent::interruption_recorded(
                    Some(sid),
                    turn,
                    serde_json::json!({"kind": "test", "turn": turn}),
                ))
                .unwrap();
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
}

#[cfg(test)]
mod observability_serde_tests {
    use super::*;

    #[test]
    fn journal_event_llm_round_type_round_trip() {
        let mut ev = JournalEvent::base_public(JournalEventType::LlmRound, Some("s1"));
        ev.round = Some(3);
        ev.tool_calls_returned = Some(5);
        ev.offset_ms = Some(12000);
        ev.tokens_in = Some(8000);
        ev.tokens_out = Some(400);

        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"llm_round\""));
        assert!(json.contains("\"round\":3"));
        assert!(json.contains("\"tool_calls_returned\":5"));

        let deser: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.event_type, JournalEventType::LlmRound);
        assert_eq!(deser.round, Some(3));
        assert_eq!(deser.tool_calls_returned, Some(5));
        assert_eq!(deser.offset_ms, Some(12000));
    }

    #[test]
    fn journal_event_turn_with_observability_summary() {
        let mut ev = JournalEvent::turn(
            Some("s1"),
            1,
            Some("gpt-4"),
            "hi",
            "hello",
            3,
            1000,
            200,
            5000,
        );
        ev.llm_rounds = Some(2);
        ev.total_llm_ms = Some(4500);
        ev.total_tool_ms = Some(500);

        let json = serde_json::to_string(&ev).unwrap();
        let deser: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.llm_rounds, Some(2));
        assert_eq!(deser.total_llm_ms, Some(4500));
        assert_eq!(deser.total_tool_ms, Some(500));
    }

    // ── P5: parent_event_id causal lineage ──────────────────────────────

    #[test]
    fn parent_event_id_serde() {
        // --- round-trips through serde ---
        {
            let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
                .with_parent_event_id(Some("evt-session-start-001".to_string()));
            let json = serde_json::to_string(&ev).unwrap();
            assert!(json.contains("parent_event_id"));
            let deser: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(
                deser.parent_event_id.as_deref(),
                Some("evt-session-start-001")
            );
        }

        // --- None omitted from JSON ---
        {
            let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100);
            assert!(ev.parent_event_id.is_none());
            let json = serde_json::to_string(&ev).unwrap();
            assert!(
                !json.contains("parent_event_id"),
                "None parent_event_id must be omitted"
            );
        }

        // --- chaining with other builders ---
        {
            let ev = JournalEvent::turn(Some("s"), 2, Some("m"), "q", "a", 1, 50, 10, 200)
                .with_parent_event_id(Some("parent-123".to_string()))
                .with_agentic_step(Some(3));
            assert_eq!(ev.parent_event_id.as_deref(), Some("parent-123"));
            assert_eq!(ev.agentic_step, Some(3));
        }

        // --- persists through writer round-trip ---
        {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = "test-parent-id-00000000-0000-0000-0000-000000000001";
            let writer = JournalWriter::new(sid).unwrap();
            let ev = JournalEvent::session_start(Some(sid), Some("m"))
                .with_parent_event_id(Some("root".to_string()));
            writer.append(&ev).unwrap();
            let (events, _, _) = read_journal_for_digest(sid).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].parent_event_id.as_deref(), Some("root"));
        }
    }

    // ── P0: git snapshot on Turn events ─────────────────────────────────

    #[test]
    fn git_snapshot_serde() {
        // --- round-trips through serde ---
        {
            let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
                .with_git_snapshot(
                    Some("abc1234".to_string()),
                    Some("feat/my-branch".to_string()),
                );
            let json = serde_json::to_string(&ev).unwrap();
            assert!(json.contains("git_head"));
            assert!(json.contains("git_branch"));
            let deser: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.git_head.as_deref(), Some("abc1234"));
            assert_eq!(deser.git_branch.as_deref(), Some("feat/my-branch"));
        }

        // --- None omitted from JSON ---
        {
            let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100);
            let json = serde_json::to_string(&ev).unwrap();
            assert!(!json.contains("git_head"));
            assert!(!json.contains("git_branch"));
        }

        // --- partial: only head, no branch ---
        {
            let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
                .with_git_snapshot(Some("deadbeef".to_string()), None);
            let json = serde_json::to_string(&ev).unwrap();
            assert!(json.contains("git_head"));
            assert!(!json.contains("git_branch"));
            let deser: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.git_head.as_deref(), Some("deadbeef"));
            assert!(deser.git_branch.is_none());
        }

        // --- detached HEAD ---
        {
            let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
                .with_git_snapshot(Some("f36ae6b1".to_string()), None);
            assert!(ev.git_branch.is_none());
            assert_eq!(ev.git_head.as_deref(), Some("f36ae6b1"));
        }

        // --- persists through writer round-trip ---
        {
            let tmp = tempfile::tempdir().unwrap();
            let _guard = JournalDirGuard::new(tmp.path());
            let sid = "test-git-snap-00000000-0000-0000-0000-000000000001";
            let writer = JournalWriter::new(sid).unwrap();
            let ev = JournalEvent::turn(Some(sid), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
                .with_git_snapshot(Some("abc1234def5678".to_string()), Some("main".to_string()));
            writer.append(&ev).unwrap();
            let (events, _, _) = read_journal_for_digest(sid).unwrap();
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].event_type, JournalEventType::SessionStart);
            assert_eq!(events[1].git_head.as_deref(), Some("abc1234def5678"));
            assert_eq!(events[1].git_branch.as_deref(), Some("main"));
        }

        // --- combined with parent_event_id ---
        {
            let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
                .with_parent_event_id(Some("parent-abc".to_string()))
                .with_git_snapshot(Some("cafe0123".to_string()), Some("dev".to_string()));
            let json = serde_json::to_string(&ev).unwrap();
            let deser: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.parent_event_id.as_deref(), Some("parent-abc"));
            assert_eq!(deser.git_head.as_deref(), Some("cafe0123"));
            assert_eq!(deser.git_branch.as_deref(), Some("dev"));
        }

        // --- on non-turn event ---
        {
            let ev = JournalEvent::base_public(JournalEventType::SyncMarker, Some("s"))
                .with_git_snapshot(Some("1111aaaa".to_string()), Some("release".to_string()));
            let json = serde_json::to_string(&ev).unwrap();
            let deser: JournalEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.git_head.as_deref(), Some("1111aaaa"));
        }
    }

    /// Bounded cache: inserting more than MAX entries evicts the oldest
    /// (FIFO), preventing unbounded memory growth in long-running servers.
    /// FIFO eviction: filling the cache beyond MAX evicts the oldest entries
    /// and never exceeds MAX. Uses a local BoundedSessionCache to avoid
    /// polluting the global SESSION_START_STATE_CACHE.
    #[test]
    fn bounded_cache_fifo_eviction_never_exceeds_max() {
        let mut cache = BoundedSessionCache::new();
        // Insert 2× MAX unique entries.
        for i in 0..(2 * BoundedSessionCache::MAX) {
            cache.insert(PathBuf::from(format!("/tmp/session-{i}.jsonl")), i % 2 == 0);
        }
        // Oldest entries (0..MAX-1) evicted.
        assert!(
            cache
                .get(Path::new(&format!(
                    "/tmp/session-{}.jsonl",
                    BoundedSessionCache::MAX - 1
                )))
                .is_none()
        );
        assert!(cache.get(Path::new("/tmp/session-0.jsonl")).is_none());
        // Most recent MAX entries still present.
        let recent = format!("/tmp/session-{}.jsonl", 2 * BoundedSessionCache::MAX - 1);
        assert!(cache.get(Path::new(&recent)).is_some());
        // Entry at MAX boundary present.
        assert!(
            cache
                .get(Path::new(&format!(
                    "/tmp/session-{}.jsonl",
                    BoundedSessionCache::MAX
                )))
                .is_some()
        );
    }

    /// Direct unit test: insert + get round-trips.
    #[test]
    fn bounded_cache_insert_and_get() {
        let mut cache = BoundedSessionCache::new();
        cache.insert(PathBuf::from("/a"), true);
        cache.insert(PathBuf::from("/b"), false);
        assert_eq!(cache.get(Path::new("/a")), Some(true));
        assert_eq!(cache.get(Path::new("/b")), Some(false));
        assert_eq!(cache.get(Path::new("/c")), None);
        // Both entries present: /b (newest) and /a (oldest but still within MAX).
        assert_eq!(cache.get(Path::new("/a")), Some(true));
        assert_eq!(cache.get(Path::new("/b")), Some(false));
    }

    /// Re-insert updates value without changing FIFO order.
    #[test]
    fn bounded_cache_reinsert_updates_value_preserves_order() {
        let mut cache = BoundedSessionCache::new();
        cache.insert(PathBuf::from("/a"), true);
        cache.insert(PathBuf::from("/b"), false);
        // Re-insert /a with new value.
        cache.insert(PathBuf::from("/a"), false);
        assert_eq!(cache.get(Path::new("/a")), Some(false));
        // Re-insert didn't add a new /a entry.
        assert_eq!(cache.get(Path::new("/b")), Some(false));
        // Fill to capacity — /a is the oldest key and should evict first.
        for i in 0..(BoundedSessionCache::MAX - 2) {
            cache.insert(PathBuf::from(format!("/fill-{i}")), true);
        }
        cache.insert(PathBuf::from("/trigger"), true);
        assert_eq!(
            cache.get(Path::new("/a")),
            None,
            "/a must evict first (oldest)"
        );
        assert_eq!(cache.get(Path::new("/b")), Some(false));
    }

    /// Poisoned mutex: when another thread panics while holding the lock,
    /// with_session_start_state_cache clears stale state and continues.
    /// Uses clear_poison() after the test to un-poison the global mutex.
    #[test]
    #[serial_test::serial(astra_session_start_state_cache)]
    fn poisoned_mutex_recovers_and_cache_is_usable() {
        // Artificially poison the global mutex by panicking while locked.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_session_start_state_cache(|_cache| {
                panic!("simulate panic while holding cache lock");
            });
        }));
        assert!(result.is_err(), "expected panic");

        // The mutex is now poisoned.  with_session_start_state_cache must
        // recover by clearing entries/order and still accept operations.
        with_session_start_state_cache(|cache| {
            // Cache must be cleared after poison recovery: pre-poison entries gone.
            assert!(
                cache.get(Path::new("/any-pre-poison-key")).is_none(),
                "cache must be cleared on poison recovery"
            );
            cache.insert(PathBuf::from("/recovery"), true);
            assert_eq!(cache.get(Path::new("/recovery")), Some(true));
            assert_eq!(cache.get(Path::new("/other")), None);
        });

        // Clean up: un-poison the mutex so subsequent tests see a healthy lock.
        // The .lock() call returns Ok now because the poison was cleared by
        // the recovery path's into_inner(). Actually, into_inner() does NOT
        // clear the poison flag — we must call clear_poison() explicitly.
        SESSION_START_STATE_CACHE.clear_poison();
    }
}
