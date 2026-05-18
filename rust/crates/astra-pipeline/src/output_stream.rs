//! Append-only disk output streams with byte offsets.
//!
//! Long-running tools and agents can write unbounded output without
//! retaining it all in memory. Consumers resume by asking for bytes
//! after their last observed offset.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputAppend {
    pub path: PathBuf,
    pub start_offset: u64,
    pub end_offset: u64,
    pub bytes_written: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum OutputStreamError {
    #[error("failed to create output stream directory '{path}': {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to open output stream '{path}': {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write output stream '{path}': {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read output stream '{path}' at offset {offset}: {source}")]
    Read {
        path: PathBuf,
        offset: u64,
        source: std::io::Error,
    },
    #[error("requested offset {offset} is beyond output stream length {len} for '{path}'")]
    OffsetBeyondEnd {
        path: PathBuf,
        offset: u64,
        len: u64,
    },
    #[error(
        "append would grow output stream '{path}' beyond configured max {max_bytes} bytes (attempted {attempted_end})"
    )]
    MaxBytesExceeded {
        path: PathBuf,
        attempted_end: u64,
        max_bytes: u64,
    },
}

#[derive(Debug, Clone)]
pub struct OutputStream {
    path: PathBuf,
    max_bytes: u64,
    append_lock: Arc<Mutex<()>>,
}

impl OutputStream {
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, OutputStreamError> {
        Self::create_with_max_bytes(path, DEFAULT_MAX_BYTES)
    }

    pub fn create_with_max_bytes(
        path: impl Into<PathBuf>,
        max_bytes: u64,
    ) -> Result<Self, OutputStreamError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| OutputStreamError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| OutputStreamError::Open {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            path,
            max_bytes,
            append_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, bytes: &[u8]) -> Result<OutputAppend, OutputStreamError> {
        let _guard = self
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)
            .map_err(|source| OutputStreamError::Open {
                path: self.path.clone(),
                source,
            })?;
        let start_offset = file
            .metadata()
            .map_err(|source| OutputStreamError::Open {
                path: self.path.clone(),
                source,
            })?
            .len();
        let bytes_written =
            u64::try_from(bytes.len()).expect("usize byte length must fit into u64");
        let attempted_end = start_offset.saturating_add(bytes_written);
        if attempted_end > self.max_bytes {
            return Err(OutputStreamError::MaxBytesExceeded {
                path: self.path.clone(),
                attempted_end,
                max_bytes: self.max_bytes,
            });
        }
        file.write_all(bytes)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_data())
            .map_err(|source| OutputStreamError::Write {
                path: self.path.clone(),
                source,
            })?;
        Ok(OutputAppend {
            path: self.path.clone(),
            start_offset,
            end_offset: attempted_end,
            bytes_written,
        })
    }

    pub fn read_from(&self, offset: u64, max_bytes: usize) -> Result<Vec<u8>, OutputStreamError> {
        let mut file = File::open(&self.path).map_err(|source| OutputStreamError::Open {
            path: self.path.clone(),
            source,
        })?;
        let len = file
            .metadata()
            .map_err(|source| OutputStreamError::Open {
                path: self.path.clone(),
                source,
            })?
            .len();
        if offset > len {
            return Err(OutputStreamError::OffsetBeyondEnd {
                path: self.path.clone(),
                offset,
                len,
            });
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|source| OutputStreamError::Read {
                path: self.path.clone(),
                offset,
                source,
            })?;
        let mut buf = vec![0; max_bytes];
        let n = file
            .read(&mut buf)
            .map_err(|source| OutputStreamError::Read {
                path: self.path.clone(),
                offset,
                source,
            })?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_reports_offsets_and_resume_reads_tail() {
        let dir = tempfile::tempdir().unwrap();
        let stream = OutputStream::create(dir.path().join("agent.out")).unwrap();

        let first = stream.append(b"hello\n").unwrap();
        let second = stream.append(b"world\n").unwrap();

        assert_eq!(first.start_offset, 0);
        assert_eq!(first.end_offset, 6);
        assert_eq!(second.start_offset, 6);
        assert_eq!(
            stream.read_from(first.end_offset, 1024).unwrap(),
            b"world\n"
        );
    }

    #[test]
    fn large_output_does_not_require_large_read() {
        let dir = tempfile::tempdir().unwrap();
        let stream = OutputStream::create(dir.path().join("large.out")).unwrap();
        for _ in 0..100_000 {
            stream.append(b"line\n").unwrap();
        }

        assert_eq!(stream.read_from(0, 5).unwrap(), b"line\n");
        assert_eq!(stream.read_from(499_995, 10).unwrap(), b"line\n");
    }

    #[test]
    fn offset_beyond_end_is_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let stream = OutputStream::create(dir.path().join("agent.out")).unwrap();
        stream.append(b"short").unwrap();

        let err = stream.read_from(99, 10).unwrap_err();
        assert!(matches!(err, OutputStreamError::OffsetBeyondEnd { .. }));
    }

    #[test]
    fn concurrent_appends_keep_offsets_and_payloads_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let stream = OutputStream::create(dir.path().join("concurrent.out")).unwrap();
        let mut handles = Vec::new();
        for i in 0..8 {
            let stream = stream.clone();
            handles.push(std::thread::spawn(move || {
                for j in 0..50 {
                    let line = format!("thread-{i}-entry-{j}\n");
                    let append = stream.append(line.as_bytes()).unwrap();
                    assert_eq!(append.bytes_written as usize, line.len());
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let len = std::fs::metadata(stream.path()).unwrap().len() as usize;
        let content = String::from_utf8(stream.read_from(0, len).unwrap()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 400);
        for i in 0..8 {
            for j in 0..50 {
                let expected = format!("thread-{i}-entry-{j}");
                assert!(
                    lines.iter().any(|line| *line == expected),
                    "missing line {expected}"
                );
            }
        }
    }

    #[test]
    fn append_rejects_writes_that_exceed_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let stream =
            OutputStream::create_with_max_bytes(dir.path().join("bounded.out"), 8).unwrap();
        let first = stream.append(b"1234").unwrap();
        assert_eq!(first.end_offset, 4);

        let err = stream.append(b"56789").unwrap_err();
        assert!(matches!(
            err,
            OutputStreamError::MaxBytesExceeded {
                attempted_end: 9,
                max_bytes: 8,
                ..
            }
        ));
        assert_eq!(std::fs::read_to_string(stream.path()).unwrap(), "1234");
    }

    #[test]
    fn append_allows_exact_max_bytes_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let stream = OutputStream::create_with_max_bytes(dir.path().join("exact.out"), 8).unwrap();
        stream.append(b"1234").unwrap();
        let second = stream.append(b"5678").unwrap();
        assert_eq!(second.end_offset, 8);
        assert_eq!(std::fs::read_to_string(stream.path()).unwrap(), "12345678");
    }
}
