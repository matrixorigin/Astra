//! Content-addressed `RuntimeConfig` version store.
//!
//! Every distinct `RuntimeConfig` the user ever runs gets a stable id
//! derived from the SHA-256 of its serialized TOML (first 16 hex chars
//! plus a `cfg_` prefix). Sessions and checkpoints reference the id;
//! cloud mirrors sync the same content-addressed shape. Two machines
//! that independently produce the same config arrive at the same id
//! and dedup for free.
//!
//! The on-disk layout under the store root is:
//!
//! ```text
//! <root>/
//!   cfg_<hex>.toml        ← one blob per unique content hash
//!   index.jsonl           ← append-only, one row per `put` call,
//!                           carries created_at + source_session + parent
//! ```
//!
//! `put` is idempotent at the *blob* layer (same content reuses the
//! file) but NOT at the *index* layer — two sessions that save the
//! same config both appear in the index so we can tell that fact
//! after the fact. `list()` walks the index newest-first.
//!
//! Not included here:
//!   * no deletion — versions are immutable history;
//!   * no GC — volume stays tiny (KBs per version; reference
//!     implementations hold decades of history comfortably);
//!   * no cloud — that's a follow-up wiring commit.

use crate::runtime_config::RuntimeConfig;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

// ─── Public types ────────────────────────────────────────────────────────

/// Content-addressed identifier: `cfg_` + first 16 hex chars of
/// SHA-256(toml_bytes). Stable, human-readable, fits in a log line.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionId(String);

impl VersionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compute from raw TOML bytes. Public so a caller holding the
    /// TOML (e.g. after an overlay merge) can inquire "what id would
    /// this be?" without persisting.
    pub fn from_toml_bytes(toml_bytes: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(toml_bytes);
        let digest = h.finalize();
        let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        VersionId(format!("cfg_{hex}"))
    }

    /// Test-only constructor — takes a pre-shaped string.
    /// Production callers must go through `from_toml_bytes` or
    /// `ConfigVersionStore::put`.
    #[doc(hidden)]
    pub fn from_str_for_test(raw: &str) -> Self {
        VersionId(raw.to_string())
    }
}

impl std::fmt::Display for VersionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Metadata carried into `put` and recorded on its index row.
#[derive(Debug, Clone, Default)]
pub struct PutMetadata {
    /// Session that produced this config (for forensics: "session X
    /// ran under version Y"). None when the config arrived from
    /// outside any session — e.g., the CLI loading user TOML at
    /// startup before any session exists.
    pub source_session: Option<String>,
    /// Version this one was derived from, when `/config` saves an
    /// edit. None for the initial load.
    pub parent: Option<VersionId>,
}

/// One row in the append-only index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: VersionId,
    #[serde(with = "chrono::serde::ts_milliseconds_option")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent: Option<VersionId>,
}

/// Errors the store can surface. `thiserror` at the boundary per
/// astra's error-handling rules — every variant carries enough
/// context to locate the bug without a debugger.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("serialize RuntimeConfig to TOML failed: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("IO on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("corrupt index row: {0}")]
    CorruptIndex(String),
}

// ─── Trait ───────────────────────────────────────────────────────────────

/// What a version store must provide. Keeping this a trait means the
/// cloud-mirror variant in a later commit can implement the same
/// surface over a remote store without rewriting call sites.
pub trait ConfigVersionStore {
    /// Write the config (hashed + deduped) and record an index row.
    /// Returns the id of the stored blob.
    fn put(&self, config: &RuntimeConfig, meta: PutMetadata) -> Result<VersionId, StoreError>;

    /// Fetch the raw TOML by id. Returns `None` for unknown ids;
    /// `Err` only for underlying IO problems.
    fn get_toml(&self, id: &VersionId) -> Result<Option<String>, StoreError>;

    /// All index entries, newest-first by created_at.
    fn list(&self) -> Result<Vec<IndexEntry>, StoreError>;
}

// ─── LocalFileStore ──────────────────────────────────────────────────────

pub struct LocalFileStore {
    root: PathBuf,
}

impl LocalFileStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Default store location: `~/.astra/config/versions/`.
    /// Returns `None` if the home dir is unresolvable, so callers
    /// can fall back gracefully (e.g., test / containerised env).
    pub fn at_default_root() -> Option<Self> {
        dirs::home_dir().map(|h| Self::new(h.join(".astra/config/versions")))
    }

    fn blob_path(&self, id: &VersionId) -> PathBuf {
        self.root.join(format!("{}.toml", id.as_str()))
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.jsonl")
    }

    fn ensure_root(&self) -> Result<(), StoreError> {
        fs::create_dir_all(&self.root).map_err(|source| StoreError::Io {
            path: self.root.clone(),
            source,
        })
    }

    fn append_index(&self, entry: &IndexEntry) -> Result<(), StoreError> {
        self.ensure_root()?;
        let path = self.index_path();
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        // Atomic single-syscall append. writeln! can internally issue
        // two write calls (body + newline) which concurrent writers
        // on the same O_APPEND file will interleave, producing
        // `{...}{...}\n` that breaks the reader's one-row-per-line
        // contract. Combine into one buffer so the kernel's O_APPEND
        // semantics keep each record whole.
        let mut line =
            serde_json::to_string(entry).map_err(|e| StoreError::CorruptIndex(e.to_string()))?;
        line.push('\n');
        f.write_all(line.as_bytes())
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        Ok(())
    }

    fn read_index(&self) -> Result<Vec<IndexEntry>, StoreError> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let f = fs::File::open(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        let mut out = Vec::new();
        for (i, line) in BufReader::new(f).lines().enumerate() {
            let line = line.map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
            if line.trim().is_empty() {
                continue;
            }
            // Primary path: one JSON object per line.
            if let Ok(entry) = serde_json::from_str::<IndexEntry>(&line) {
                out.push(entry);
                continue;
            }
            // Recovery path: older builds could race writeln!() across
            // processes and produce `{...}{...}\n` on one line. Feed
            // the line through the streaming deserializer and accept
            // whatever valid objects come out — strict parse of a
            // legacy row shouldn't sink a live CLI.
            let mut de = serde_json::Deserializer::from_str(&line).into_iter::<IndexEntry>();
            let mut any = false;
            for item in de.by_ref() {
                match item {
                    Ok(entry) => {
                        out.push(entry);
                        any = true;
                    }
                    Err(_) => break,
                }
            }
            if !any {
                return Err(StoreError::CorruptIndex(format!(
                    "line {}: could not recover any IndexEntry",
                    i + 1
                )));
            }
        }
        Ok(out)
    }
}

impl ConfigVersionStore for LocalFileStore {
    fn put(&self, config: &RuntimeConfig, meta: PutMetadata) -> Result<VersionId, StoreError> {
        self.ensure_root()?;

        // 1. Serialize to TOML — this IS the content we hash. Any
        //    formatting quirk is captured in the id, which is what
        //    we want: two structurally-equal configs that serialize
        //    differently deserve different ids because what we
        //    round-trip is the bytes, not the struct.
        let toml_bytes = toml::to_string_pretty(config)?;
        let id = VersionId::from_toml_bytes(toml_bytes.as_bytes());

        // 2. Write blob iff absent. `create_new` would race two
        //    concurrent puts; we check-then-write on the assumption
        //    that collisions on the same content are fine.
        let blob = self.blob_path(&id);
        if !blob.exists() {
            let mut f = fs::File::create(&blob).map_err(|source| StoreError::Io {
                path: blob.clone(),
                source,
            })?;
            f.write_all(toml_bytes.as_bytes())
                .map_err(|source| StoreError::Io {
                    path: blob.clone(),
                    source,
                })?;
        }

        // 3. Append index row — always, even on duplicate put. The
        //    second session that arrives at the same config IS a
        //    fact worth recording.
        let entry = IndexEntry {
            id: id.clone(),
            created_at: Some(Utc::now()),
            source_session: meta.source_session,
            parent: meta.parent,
        };
        self.append_index(&entry)?;

        Ok(id)
    }

    fn get_toml(&self, id: &VersionId) -> Result<Option<String>, StoreError> {
        let path = self.blob_path(id);
        match fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    fn list(&self) -> Result<Vec<IndexEntry>, StoreError> {
        let mut rows = self.read_index()?;
        // Newest-first. Rows without a timestamp sort to the end
        // (None < Some in std::cmp::Reverse? no — Option's Ord ranks
        // None before Some, so Reverse puts None last correctly).
        rows.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(rows)
    }
}
