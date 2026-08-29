//! Reading and writing watched save files. The engine only ever touches a save
//! path through here.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::SystemTime;

use crate::sync::hash::sha256_hex;

/// A watched save file as it currently is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalFile {
    /// No file at the path (never created, or the user deleted it).
    Missing,
    Present {
        /// `content_hash` of the bytes, comparable to `SaveMeta::content_hash`.
        hash: String,
        len: u64,
        /// Last-modified time, for the `prefer-newest` conflict policy. `None` if
        /// the platform / filesystem doesn't report it.
        modified: Option<SystemTime>,
    },
}

impl LocalFile {
    /// Read and hash the file at `path`. A missing file is [`LocalFile::Missing`],
    /// not an error; anything else (permissions, a directory in the way) is.
    pub fn read(path: &Path) -> io::Result<LocalFile> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LocalFile::Missing),
            Err(e) => return Err(e),
        };
        let modified = fs::metadata(path).and_then(|m| m.modified()).ok();
        Ok(LocalFile::Present { hash: sha256_hex(&bytes), len: bytes.len() as u64, modified })
    }

    pub fn hash(&self) -> Option<&str> {
        match self {
            LocalFile::Present { hash, .. } => Some(hash),
            LocalFile::Missing => None,
        }
    }

    pub fn is_missing(&self) -> bool {
        matches!(self, LocalFile::Missing)
    }
}

/// Read the raw bytes for upload. `Ok(None)` = the file isn't there;
/// [`is_locked`] tells a caller a returned `Err` is worth retrying.
pub fn read_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// `true` when the error looks like "an emulator still has the file open"
/// (Windows sharing violation, or a POSIX advisory lock).
pub fn is_locked(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock) || e.raw_os_error() == Some(32)
}

/// Write `bytes` to `path` atomically: a temp file in the same directory, flushed
/// and `fsync`ed, then renamed over `path`. A crash mid-write leaves the old save
/// intact, never a half-written one. Creates parent directories as needed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("save");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));

    let result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("coincell-disk-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_reads_as_missing() {
        let path = tmpdir().join("nope.srm");
        assert_eq!(LocalFile::read(&path).unwrap(), LocalFile::Missing);
    }

    #[test]
    fn present_file_reports_hash_and_len() {
        let dir = tmpdir();
        let path = dir.join("game.srm");
        write_atomic(&path, b"abc").unwrap();

        let local = LocalFile::read(&path).unwrap();
        match local {
            LocalFile::Present { hash, len, .. } => {
                assert_eq!(len, 3);
                assert_eq!(hash, sha256_hex(b"abc"));
            }
            LocalFile::Missing => panic!("just wrote it"),
        }
    }

    #[test]
    fn write_atomic_creates_parents_and_overwrites() {
        let path = tmpdir().join("nested/deep/save.dat");
        write_atomic(&path, b"one").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"one");
        write_atomic(&path, b"two-longer").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two-longer");
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains(".tmp")).collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }
}
