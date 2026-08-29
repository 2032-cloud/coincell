//! The pure heart of the engine: given the file on disk, the server's latest
//! save, and what we last synced, decide what to do. No I/O, every branch is a
//! hash comparison, so it's exhaustively unit-tested here and the worker just
//! executes the verdict.

use std::time::UNIX_EPOCH;

use crate::api::SaveMeta;
use crate::config::ConflictPolicy;
use crate::store::InstanceRecord;
use crate::sync::disk::LocalFile;
use crate::sync::time::parse_utc;

/// What the worker should do for one instance this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Disk and server agree (and the store knows it). Nothing to do.
    Idle,
    /// The server has bytes we don't. Download `remote.id`, write it, record it.
    Pull,
    /// We have bytes the server doesn't. Upload them. (The download-only engine
    /// reports this as pending rather than acting on it.)
    Push,
    /// Disk already equals the remote bytes; only the store's bookkeeping is
    /// behind. `record_synced`, no download.
    MarkSynced,
    /// Local and remote both moved since the last sync point (or there is no sync
    /// point and they differ). Hand to the conflict flow.
    Conflict,
}

/// Decide the [`Action`] for one instance. `remote` is `None` when the server has
/// no save for it yet. `policy` only matters when the raw verdict is
/// [`Action::Conflict`], a `prefer-*` setting resolves it here so it never
/// reaches the store or Home.
pub fn reconcile(local: &LocalFile, remote: Option<&SaveMeta>, book: &InstanceRecord, policy: ConflictPolicy) -> Action {
    let synced = book.last_synced_hash.as_deref();

    let Some(remote) = remote else {
        // Server has nothing. Our file (if any) is the only copy → push it.
        return if local.is_missing() { Action::Idle } else { Action::Push };
    };
    let remote_hash = remote.content_hash.as_str();

    let raw = match local.hash() {
        None => {
            // File gone. If the store says we're already on the remote hash, the
            // user deleted a synced save, leave it (Home has an explicit
            // "restore"). Otherwise pull it down.
            if synced == Some(remote_hash) { Action::Idle } else { Action::Pull }
        }
        Some(local_hash) if local_hash == remote_hash => {
            if synced == Some(remote_hash) {
                Action::Idle
            } else {
                Action::MarkSynced
            }
        }
        Some(local_hash) => {
            if synced == Some(local_hash) {
                Action::Pull // local untouched since sync, remote advanced
            } else if synced == Some(remote_hash) {
                Action::Push // remote untouched since sync, local advanced
            } else {
                Action::Conflict // both advanced, or no common baseline
            }
        }
    };

    if raw != Action::Conflict {
        return raw;
    }
    match policy {
        ConflictPolicy::Ask => Action::Conflict,
        ConflictPolicy::PreferLocal => Action::Push,
        ConflictPolicy::PreferRemote => Action::Pull,
        ConflictPolicy::PreferNewest => match newer(local, remote) {
            Some(Side::Local) => Action::Push,
            Some(Side::Remote) => Action::Pull,
            // Can't tell (no local mtime), don't guess, ask.
            None => Action::Conflict,
        },
    }
}

enum Side {
    Local,
    Remote,
}

fn newer(local: &LocalFile, remote: &SaveMeta) -> Option<Side> {
    let LocalFile::Present { modified: Some(modified), .. } = local else { return None };
    let local_secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let remote_secs = parse_utc(remote.uploaded_at.as_str())?;
    Some(if local_secs >= remote_secs { Side::Local } else { Side::Remote })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::api::Timestamp;

    fn book(synced: Option<&str>) -> InstanceRecord {
        InstanceRecord {
            game_instance_id: "inst".into(),
            save_path: PathBuf::from("/saves/g.srm"),
            console_slug: None,
            paused: false,
            last_synced_hash: synced.map(str::to_owned),
            last_synced_save_id: synced.map(|_| "save-x".to_owned()),
            last_uploaded_hash: None,
            conflict: None,
        }
    }

    fn save(hash: &str) -> SaveMeta {
        SaveMeta { id: format!("save-{hash}"), size_bytes: 4, uploaded_at: Timestamp("2026-08-28 12:00:00".into()), starred: false, note: None, content_hash: hash.into() }
    }

    fn present(hash: &str) -> LocalFile {
        LocalFile::Present { hash: hash.into(), len: 4, modified: None }
    }
    fn present_at(hash: &str, unix: u64) -> LocalFile {
        LocalFile::Present { hash: hash.into(), len: 4, modified: Some(UNIX_EPOCH + Duration::from_secs(unix)) }
    }

    const ASK: ConflictPolicy = ConflictPolicy::Ask;

    #[test]
    fn everything_agrees_is_idle() {
        assert_eq!(reconcile(&present("a"), Some(&save("a")), &book(Some("a")), ASK), Action::Idle);
    }

    #[test]
    fn missing_local_pulls_unless_recorded_synced() {
        assert_eq!(reconcile(&LocalFile::Missing, Some(&save("a")), &book(None), ASK), Action::Pull);
        assert_eq!(reconcile(&LocalFile::Missing, Some(&save("a")), &book(Some("b")), ASK), Action::Pull);
        // deleted a synced save → leave it
        assert_eq!(reconcile(&LocalFile::Missing, Some(&save("a")), &book(Some("a")), ASK), Action::Idle);
    }

    #[test]
    fn bytes_match_remote_but_store_is_behind_marks_synced() {
        assert_eq!(reconcile(&present("a"), Some(&save("a")), &book(None), ASK), Action::MarkSynced);
        assert_eq!(reconcile(&present("a"), Some(&save("a")), &book(Some("old")), ASK), Action::MarkSynced);
    }

    #[test]
    fn remote_advanced_local_untouched_pulls() {
        assert_eq!(reconcile(&present("a"), Some(&save("b")), &book(Some("a")), ASK), Action::Pull);
    }

    #[test]
    fn local_advanced_remote_untouched_pushes() {
        assert_eq!(reconcile(&present("b"), Some(&save("a")), &book(Some("a")), ASK), Action::Push);
    }

    #[test]
    fn both_advanced_is_conflict_under_ask() {
        assert_eq!(reconcile(&present("b"), Some(&save("c")), &book(Some("a")), ASK), Action::Conflict);
    }

    #[test]
    fn no_baseline_and_differ_is_conflict() {
        assert_eq!(reconcile(&present("b"), Some(&save("c")), &book(None), ASK), Action::Conflict);
    }

    #[test]
    fn remote_absent_pushes_local_only() {
        assert_eq!(reconcile(&present("a"), None, &book(None), ASK), Action::Push);
        assert_eq!(reconcile(&LocalFile::Missing, None, &book(None), ASK), Action::Idle);
    }

    #[test]
    fn prefer_policies_resolve_a_conflict() {
        let (l, r, b) = (present("b"), save("c"), book(Some("a")));
        assert_eq!(reconcile(&l, Some(&r), &b, ConflictPolicy::PreferLocal), Action::Push);
        assert_eq!(reconcile(&l, Some(&r), &b, ConflictPolicy::PreferRemote), Action::Pull);
        // no mtime → prefer-newest can't decide → falls back to Conflict
        assert_eq!(reconcile(&l, Some(&r), &b, ConflictPolicy::PreferNewest), Action::Conflict);
    }

    #[test]
    fn prefer_newest_uses_mtime_vs_uploaded_at() {
        let r = save("c");
        let b = book(Some("a"));
        let uploaded = parse_utc(r.uploaded_at.as_str()).unwrap() as u64;
        let local_older = present_at("b", uploaded - 1000);
        let local_newer = present_at("b", uploaded + 1000);
        assert_eq!(reconcile(&local_newer, Some(&r), &b, ConflictPolicy::PreferNewest), Action::Push);
        assert_eq!(reconcile(&local_older, Some(&r), &b, ConflictPolicy::PreferNewest), Action::Pull);
    }
}
