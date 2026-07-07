//! Append-only disk output streams with byte offsets.
//!
//! Long-running tools and agents can write unbounded output without
//! retaining it all in memory. Consumers resume by asking for bytes
//! after their last observed offset.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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
    #[error("failed to read from output stream '{path}' at offset {offset}: {source}")]
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
}

#[derive(Debug, Clone)]
pub struct OutputStream {
    path: PathBuf,
}

impl OutputStream {
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, OutputStreamError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| OutputStreamError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        // Touch the file so read_from works even before any append.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| OutputStreamError::Open {
                path: path.clone(),
                source,
            })?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
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

    /// After create, read_from on the empty file returns an empty buffer.
    #[test]
    fn read_from_empty_file_after_create() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.log");
        let stream = OutputStream::create(&path).unwrap();
        let data = stream.read_from(0, 1024).unwrap();
        assert!(data.is_empty(), "empty file must yield empty buffer");
    }

    /// create creates the file on disk.
    #[test]
    fn create_touches_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.log");
        assert!(!path.exists());
        OutputStream::create(&path).unwrap();
        assert!(path.exists());
    }

    /// read_from at offset 0 after manual write returns the written bytes.
    #[test]
    fn read_from_after_manual_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.log");
        let stream = OutputStream::create(&path).unwrap();
        std::fs::write(&path, b"hello world").unwrap();
        let data = stream.read_from(0, 1024).unwrap();
        assert_eq!(data, b"hello world");
    }

    /// read_from with an offset inside valid data returns the suffix.
    #[test]
    fn read_from_with_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("offset.log");
        let stream = OutputStream::create(&path).unwrap();
        std::fs::write(&path, b"0123456789").unwrap();
        let data = stream.read_from(5, 1024).unwrap();
        assert_eq!(data, b"56789");
    }

    /// read_from respects max_bytes and truncates the returned buffer.
    #[test]
    fn read_from_respects_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max.log");
        let stream = OutputStream::create(&path).unwrap();
        std::fs::write(&path, b"0123456789").unwrap();
        let data = stream.read_from(0, 4).unwrap();
        assert_eq!(data.len(), 4);
        assert_eq!(data, b"0123");
    }

    /// read_from with offset == len returns empty buffer (not an error).
    #[test]
    fn read_from_offset_equals_len() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eof.log");
        let stream = OutputStream::create(&path).unwrap();
        std::fs::write(&path, b"abc").unwrap();
        let data = stream.read_from(3, 1024).unwrap();
        assert!(data.is_empty());
    }

    /// read_from with offset beyond end returns OffsetBeyondEnd.
    #[test]
    fn read_from_offset_beyond_end_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("beyond.log");
        let stream = OutputStream::create(&path).unwrap();
        std::fs::write(&path, b"abc").unwrap();
        let err = stream.read_from(100, 1024).unwrap_err();
        match err {
            OutputStreamError::OffsetBeyondEnd { offset, len, .. } => {
                assert_eq!(offset, 100);
                assert_eq!(len, 3);
            }
            other => panic!("expected OffsetBeyondEnd, got {other:?}"),
        }
    }

    /// read_from on a non-existent file returns an Open error.
    #[test]
    fn read_from_missing_file_open_error() {
        let stream = OutputStream {
            path: PathBuf::from("/nonexistent/path/output.log"),
        };
        let err = stream.read_from(0, 1024).unwrap_err();
        assert!(
            matches!(err, OutputStreamError::Open { .. }),
            "expected Open error, got {err:?}"
        );
    }

    /// create creates parent directories as needed.
    #[test]
    fn create_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("c").join("stream.log");
        assert!(!nested.parent().unwrap().exists());
        OutputStream::create(&nested).unwrap();
        assert!(nested.exists());
    }
}
