//! Picking an existing local save file and binding it to a game instance.
//! Shared by the add-game flow and the instance detail page.
//!
//! We only map files that already exist (the user saves once in their emulator
//! first, which guarantees the right name / location / format), so there's no
//! folder-plus-name guessing. When the picked file differs from what's already
//! on the server, the caller asks which one wins.
//!
//! The `rfd` dialog is native and modal; `App` runs it on its own thread (RFD
//! inits COM per call) and hands the chosen `PathBuf` back.

use std::path::PathBuf;

use crate::sync::sha256_hex;

/// An existing file the user chose, hashed and size-checked.
#[derive(Debug, Clone)]
pub struct PickedSave {
    pub path: PathBuf,
    pub size: u64,
    /// `content_hash` of the current bytes, for comparing against the server.
    pub hash: String,
    /// `true` if the size is in the console's accepted set (or the set is empty
    /// / unknown, in which case we don't second-guess).
    pub size_ok: bool,
}

impl PickedSave {
    /// Read, hash and size-check `path`. `None` if the file can't be read (it
    /// vanished, or an emulator has it locked).
    pub fn inspect(path: PathBuf, valid_sizes: &[u64]) -> Option<Self> {
        let bytes = std::fs::read(&path).ok()?;
        let size = bytes.len() as u64;
        let size_ok = valid_sizes.is_empty() || valid_sizes.contains(&size);
        Some(Self { path, size, hash: sha256_hex(&bytes), size_ok })
    }
}

/// Native "choose a file" dialog. `None` if cancelled. Blocking.
pub fn pick_save_file(title: &str) -> Option<PathBuf> {
    // TODO: once the backend serves expected save-file extensions per console /
    // game, pass them as a filter:
    //   dialog = dialog.add_filter("Save files", &["srm", "sav", "dsv", ...]);
    rfd::FileDialog::new().set_title(title).pick_file()
}

/// `4096` → `"4 KB"`, `8320` → `"8.1 KB"`, `2_100_000` → `"2.00 MB"`.
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        let kb = b / KB;
        if kb.fract() < 0.05 { format!("{kb:.0} KB") } else { format!("{kb:.1} KB") }
    } else {
        format!("{:.2} MB", b / MB)
    }
}

/// A short "usually 8 KB or 32 KB" phrase for the size-mismatch warning.
pub fn describe_sizes(sizes: &[u64]) -> String {
    match sizes {
        [] => "an unknown size".to_owned(),
        [one] => human_size(*one),
        [rest @ .., last] => {
            let head = rest.iter().map(|s| human_size(*s)).collect::<Vec<_>>().join(", ");
            format!("{head} or {}", human_size(*last))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_reads_naturally() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(4096), "4 KB");
        assert_eq!(human_size(8320), "8.1 KB");
        assert_eq!(human_size(2_097_152), "2.00 MB");
    }

    #[test]
    fn describe_sizes_lists_them() {
        assert_eq!(describe_sizes(&[]), "an unknown size");
        assert_eq!(describe_sizes(&[8192]), "8 KB");
        assert_eq!(describe_sizes(&[8192, 32768]), "8 KB or 32 KB");
        assert_eq!(describe_sizes(&[512, 8192, 32768]), "512 B, 8 KB or 32 KB");
    }

    #[test]
    fn inspect_hashes_and_size_checks() {
        let dir = std::env::temp_dir().join(format!("coincell-inspect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("save.srm");
        std::fs::write(&path, b"abc").unwrap();

        let ok = PickedSave::inspect(path.clone(), &[3]).unwrap();
        assert_eq!(ok.size, 3);
        assert_eq!(ok.hash, sha256_hex(b"abc"));
        assert!(ok.size_ok);

        assert!(!PickedSave::inspect(path, &[8192]).unwrap().size_ok);
        assert!(PickedSave::inspect(dir.join("missing.srm"), &[]).is_none());
    }
}
