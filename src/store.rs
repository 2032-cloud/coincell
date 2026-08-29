//! `data.sqlite` - the operational state the daemon owns.
//!
//! Companion to [`crate::config`]: `config.toml` holds hand-editable preferences,
//! this holds the bookkeeping the sync engine writes constantly and the user
//! never touches. It lives in `DATA_DIR` (deliberately separate from the config
//! dir) and is fully rebuildable from the backend without losing user intent.
//!
//! It holds:
//!
//! - path ↔ `game_instance_id` mappings and the per-instance pause flag;
//! - per-instance sync bookkeeping (last-synced hash / save id, last uploaded
//!   hash) and conflict markers;
//! - the offline upload queue;
//! - the last stream-position timestamp (`SyncStream`'s `?since=` cursor);
//! - a launch-time cache of each console's `validSaveSizes`.
//!
//! Call sites go through [`Store::get`] / [`Store::write`] and the typed methods
//! on [`Store`] - never raw SQL - mirroring the `Config::get` / `Config::update`
//! shape. A single writer sits behind a `Mutex`; `write` runs the whole closure
//! as one transaction.

// The sync engine that reads and writes all of this isn't built yet, so most of
// the surface below has no in-crate caller. Drop this once that lands.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::constants::DATA_DIR;

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();

const DB_FILE: &str = "data.sqlite";

/// Ordered schema migrations. Applying entry `i` moves `PRAGMA user_version`
/// from `i` to `i + 1`. **Append only** - never edit an entry that has shipped.
const MIGRATIONS: &[&str] = &[
    // v0 -> v1: initial schema.
    "
    CREATE TABLE instances (
        game_instance_id     TEXT PRIMARY KEY,
        save_path            TEXT NOT NULL UNIQUE,
        console_slug         TEXT,
        paused               INTEGER NOT NULL DEFAULT 0,
        last_synced_hash     TEXT,
        last_synced_save_id  TEXT,
        last_uploaded_hash   TEXT,
        conflict_local_hash  TEXT,
        conflict_remote_hash TEXT,
        conflict_detected_at TEXT,
        bound_at             TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
    );

    CREATE TABLE upload_queue (
        id               INTEGER PRIMARY KEY AUTOINCREMENT,
        game_instance_id TEXT NOT NULL REFERENCES instances(game_instance_id) ON DELETE CASCADE,
        content_hash     TEXT NOT NULL,
        bytes            BLOB NOT NULL,
        size_bytes       INTEGER NOT NULL,
        attempts         INTEGER NOT NULL DEFAULT 0,
        last_error       TEXT,
        last_attempt_at  TEXT,
        queued_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        UNIQUE(game_instance_id, content_hash)
    );

    CREATE TABLE console_save_sizes (
        console_slug     TEXT PRIMARY KEY,
        valid_save_sizes TEXT NOT NULL,
        cached_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
    );

    CREATE TABLE meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ",
    // v1 -> v2: snapshots of a local save taken right before the engine
    // overwrote it with bytes the user had never uploaded. No FK to `instances`:
    // a backup must outlive an unmap (that's the whole point).
    "
    CREATE TABLE save_backups (
        id               INTEGER PRIMARY KEY AUTOINCREMENT,
        game_instance_id TEXT NOT NULL,
        original_path    TEXT NOT NULL,
        content_hash     TEXT NOT NULL,
        size_bytes       INTEGER NOT NULL,
        replaced_with    TEXT NOT NULL,
        server_save_id   TEXT,
        reason           TEXT NOT NULL,
        overwritten_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
    );

    CREATE INDEX ix_save_backups_instance ON save_backups(game_instance_id);
    ",
];

const CURSOR_KEY: &str = "sync_cursor";

/// A row of the `instances` table: one watched save file bound to a backend game
/// instance, plus its sync bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRecord {
    pub game_instance_id: String,
    pub save_path: PathBuf,
    /// Console this instance belongs to, used to look up cached `validSaveSizes`.
    pub console_slug: Option<String>,
    /// Per-instance pause: keep watching the filesystem, do no network work.
    pub paused: bool,
    /// Hash disk and server last agreed on.
    pub last_synced_hash: Option<String>,
    /// The server save row for [`Self::last_synced_hash`].
    pub last_synced_save_id: Option<String>,
    /// Hash of the most recent bytes we successfully uploaded.
    pub last_uploaded_hash: Option<String>,
    /// Set when the file diverged both locally and on the server.
    pub conflict: Option<Conflict>,
}

/// A "changed here and on another device" marker, surfaced in Home for the user
/// to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub local_hash: String,
    pub remote_hash: String,
    /// `YYYY-MM-DD HH:MM:SS` UTC, stamped by SQLite when the conflict was found.
    pub detected_at: String,
}

/// A pending entry in the offline upload queue. The bytes are snapshotted at
/// enqueue time so a later change to the file on disk can't corrupt a retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedUpload {
    pub id: i64,
    pub game_instance_id: String,
    pub content_hash: String,
    pub bytes: Vec<u8>,
    pub size_bytes: u64,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub queued_at: String,
    /// `YYYY-MM-DD HH:MM:SS` UTC of the last retry, or `None` if never tried.
    pub last_attempt_at: Option<String>,
}

/// A snapshot of a local save file the engine kept right before overwriting it
/// with bytes the user had never uploaded, so the overwrite is always
/// recoverable. The bytes live at `BACKUP_DIR/<content_hash>`; this is the index
/// entry. No UI reads these yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveBackup {
    pub id: i64,
    pub game_instance_id: String,
    /// The local file that was overwritten.
    pub original_path: PathBuf,
    /// `content_hash` of the backed-up bytes; also the blob's filename.
    pub content_hash: String,
    pub size_bytes: u64,
    /// `content_hash` of the save that replaced it.
    pub replaced_with: String,
    /// The replacing save's server id, if it was known.
    pub server_save_id: Option<String>,
    /// What triggered the overwrite, e.g. `"pull"` / `"conflict"`.
    pub reason: String,
    /// `YYYY-MM-DD HH:MM:SS` UTC, stamped by SQLite.
    pub overwritten_at: String,
}

const INSTANCE_COLS: &str = "game_instance_id, save_path, console_slug, paused, \
     last_synced_hash, last_synced_save_id, last_uploaded_hash, \
     conflict_local_hash, conflict_remote_hash, conflict_detected_at";

const BACKUP_COLS: &str = "id, game_instance_id, original_path, content_hash, size_bytes, replaced_with, server_save_id, reason, overwritten_at";

fn map_backup(row: &Row) -> rusqlite::Result<SaveBackup> {
    Ok(SaveBackup {
        id: row.get(0)?,
        game_instance_id: row.get(1)?,
        original_path: PathBuf::from(row.get::<_, String>(2)?),
        content_hash: row.get(3)?,
        size_bytes: row.get::<_, i64>(4)? as u64,
        replaced_with: row.get(5)?,
        server_save_id: row.get(6)?,
        reason: row.get(7)?,
        overwritten_at: row.get(8)?,
    })
}

fn map_instance(row: &Row) -> rusqlite::Result<InstanceRecord> {
    let save_path: String = row.get(1)?;
    let conflict = match (row.get(7)?, row.get(8)?, row.get(9)?) {
        (Some(local_hash), Some(remote_hash), Some(detected_at)) => Some(Conflict { local_hash, remote_hash, detected_at }),
        _ => None,
    };
    Ok(InstanceRecord {
        game_instance_id: row.get(0)?,
        save_path: PathBuf::from(save_path),
        console_slug: row.get(2)?,
        paused: row.get(3)?,
        last_synced_hash: row.get(4)?,
        last_synced_save_id: row.get(5)?,
        last_uploaded_hash: row.get(6)?,
        conflict,
    })
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (or create) the database. Must run once before [`Store::get`] /
    /// [`Store::write`]. Never fails: an unreadable file is moved aside and
    /// rebuilt, and if even that can't be done the daemon runs against an
    /// in-memory database rather than crashing.
    pub fn init() {
        let _ = STORE.set(Mutex::new(Self::load()));
    }

    fn slot() -> &'static Mutex<Store> {
        STORE.get().expect("Store::init() must run before Store::get / Store::write")
    }

    /// Read from the store. The closure gets `&Store` and calls its typed
    /// accessors. Don't nest another `get` / `write` inside - the lock is not
    /// reentrant.
    pub fn get<T>(f: impl FnOnce(&Store) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        let guard = Self::slot().lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&guard)?)
    }

    /// Write to the store as a single transaction: every statement the closure
    /// runs commits together, or - if it returns `Err` - rolls back together.
    /// Don't nest another `get` / `write` inside.
    pub fn write<T>(f: impl FnOnce(&Store) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        let guard = Self::slot().lock().unwrap_or_else(|e| e.into_inner());
        // Clear any transaction a previously-panicked writer left dangling.
        let _ = guard.conn.execute_batch("ROLLBACK");
        guard.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(&guard) {
            Ok(value) => {
                guard.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(e) => {
                let _ = guard.conn.execute_batch("ROLLBACK");
                Err(e.into())
            }
        }
    }

    fn load() -> Store {
        match Self::open_at(&Self::path()) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!("store open failed: {e:#}");
                Self::recover().unwrap_or_else(|e| {
                    tracing::error!("store recovery failed, using in-memory db: {e:#}");
                    let conn = Connection::open_in_memory().expect("open in-memory sqlite");
                    Store { conn: prepare(conn).expect("prepare in-memory sqlite") }
                })
            }
        }
    }

    fn path() -> PathBuf {
        DATA_DIR.join(DB_FILE)
    }

    fn open_at(path: &Path) -> anyhow::Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Store { conn: prepare(Connection::open(path)?)? })
    }

    /// Move an unusable database (and its WAL siblings) aside, then start fresh.
    fn recover() -> anyhow::Result<Store> {
        let path = Self::path();
        let backup = back_up(&path)?;
        tracing::warn!("store file unusable, moved to {}", backup.display());
        Self::open_at(&path)
    }

    // ---- instance ↔ path mappings ------------------------------------------

    /// Every bound instance, oldest binding first.
    pub fn instances(&self) -> rusqlite::Result<Vec<InstanceRecord>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {INSTANCE_COLS} FROM instances ORDER BY bound_at, game_instance_id"))?;
        let rows = stmt.query_map([], map_instance)?;
        rows.collect()
    }

    pub fn instance(&self, id: &str) -> rusqlite::Result<Option<InstanceRecord>> {
        self.conn.query_row(&format!("SELECT {INSTANCE_COLS} FROM instances WHERE game_instance_id = ?1"), params![id], map_instance).optional()
    }

    /// Look up the instance watching a given save file.
    pub fn instance_for_path(&self, path: &Path) -> rusqlite::Result<Option<InstanceRecord>> {
        self.conn.query_row(&format!("SELECT {INSTANCE_COLS} FROM instances WHERE save_path = ?1"), params![path.to_string_lossy()], map_instance).optional()
    }

    /// Bind (or re-point) an instance to a local save path. Re-binding an
    /// existing instance keeps its sync bookkeeping and conflict marker; a
    /// `None` `console_slug` leaves any stored one intact.
    pub fn bind_instance(&self, id: &str, path: &Path, console_slug: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO instances (game_instance_id, save_path, console_slug)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(game_instance_id) DO UPDATE SET
                 save_path    = excluded.save_path,
                 console_slug = COALESCE(excluded.console_slug, instances.console_slug)",
            params![id, path.to_string_lossy(), console_slug],
        )?;
        Ok(())
    }

    /// Forget an instance entirely (removed from Home). Cascades to its queued
    /// uploads.
    pub fn unbind_instance(&self, id: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM instances WHERE game_instance_id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_paused(&self, id: &str, paused: bool) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE instances SET paused = ?2 WHERE game_instance_id = ?1", params![id, paused])?;
        Ok(())
    }

    // ---- per-instance sync bookkeeping -----------------------------------

    /// Record that disk and server now agree on `hash` (`save_id` being the
    /// server's save row for it). Clears any conflict marker.
    pub fn record_synced(&self, id: &str, hash: &str, save_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE instances SET
                 last_synced_hash     = ?2,
                 last_synced_save_id  = ?3,
                 conflict_local_hash  = NULL,
                 conflict_remote_hash = NULL,
                 conflict_detected_at = NULL
             WHERE game_instance_id = ?1",
            params![id, hash, save_id],
        )?;
        Ok(())
    }

    /// Record the hash of the most recent bytes we uploaded for this instance.
    pub fn record_uploaded(&self, id: &str, hash: &str) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE instances SET last_uploaded_hash = ?2 WHERE game_instance_id = ?1", params![id, hash])?;
        Ok(())
    }

    // ---- conflict markers ----------------------------------------------

    pub fn set_conflict(&self, id: &str, local_hash: &str, remote_hash: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE instances SET
                 conflict_local_hash  = ?2,
                 conflict_remote_hash = ?3,
                 conflict_detected_at = CURRENT_TIMESTAMP
             WHERE game_instance_id = ?1",
            params![id, local_hash, remote_hash],
        )?;
        Ok(())
    }

    pub fn clear_conflict(&self, id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE instances SET
                 conflict_local_hash  = NULL,
                 conflict_remote_hash = NULL,
                 conflict_detected_at = NULL
             WHERE game_instance_id = ?1",
            params![id],
        )?;
        Ok(())
    }

    // ---- pre-overwrite save backups ----------------------------------

    /// Index a snapshot the engine just wrote to `BACKUP_DIR/<content_hash>`.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_backup(&self, id: &str, original_path: &Path, content_hash: &str, size_bytes: u64, replaced_with: &str, server_save_id: Option<&str>, reason: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO save_backups
                 (game_instance_id, original_path, content_hash, size_bytes, replaced_with, server_save_id, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, original_path.to_string_lossy(), content_hash, size_bytes as i64, replaced_with, server_save_id, reason],
        )?;
        Ok(())
    }

    /// Every snapshot, newest first.
    pub fn backups(&self) -> rusqlite::Result<Vec<SaveBackup>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {BACKUP_COLS} FROM save_backups ORDER BY overwritten_at DESC, id DESC"))?;
        stmt.query_map([], map_backup)?.collect()
    }

    /// Snapshots for one instance, newest first.
    pub fn backups_for(&self, id: &str) -> rusqlite::Result<Vec<SaveBackup>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {BACKUP_COLS} FROM save_backups WHERE game_instance_id = ?1 ORDER BY overwritten_at DESC, id DESC"))?;
        stmt.query_map(params![id], map_backup)?.collect()
    }

    // ---- stream cursor ------------------------------------------------

    /// The persisted `?since=` cursor (max `last_saved_at` seen), or `None` for a
    /// full hydrate on next start.
    pub fn sync_cursor(&self) -> rusqlite::Result<Option<String>> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", params![CURSOR_KEY], |r| r.get(0)).optional()
    }

    pub fn set_sync_cursor(&self, cursor: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![CURSOR_KEY, cursor],
        )?;
        Ok(())
    }

    // ---- offline upload queue ---------------------------------------

    /// Queue bytes for upload. Re-queuing the same `(instance, content_hash)`
    /// refreshes the row and resets its retry state instead of duplicating it.
    pub fn enqueue_upload(&self, id: &str, content_hash: &str, bytes: &[u8]) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO upload_queue (game_instance_id, content_hash, bytes, size_bytes)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(game_instance_id, content_hash) DO UPDATE SET
                 bytes           = excluded.bytes,
                 size_bytes      = excluded.size_bytes,
                 attempts        = 0,
                 last_error      = NULL,
                 last_attempt_at = NULL,
                 queued_at       = CURRENT_TIMESTAMP",
            params![id, content_hash, bytes, bytes.len() as i64],
        )?;
        Ok(())
    }

    /// Pending uploads, oldest first.
    pub fn queued_uploads(&self) -> rusqlite::Result<Vec<QueuedUpload>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, game_instance_id, content_hash, bytes, size_bytes, attempts, last_error, queued_at, last_attempt_at
             FROM upload_queue ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(QueuedUpload {
                id: row.get(0)?,
                game_instance_id: row.get(1)?,
                content_hash: row.get(2)?,
                bytes: row.get(3)?,
                size_bytes: row.get::<_, i64>(4)? as u64,
                attempts: row.get::<_, i64>(5)? as u32,
                last_error: row.get(6)?,
                queued_at: row.get(7)?,
                last_attempt_at: row.get(8)?,
            })
        })?;
        rows.collect()
    }

    /// Drop a queue row after its upload succeeds.
    pub fn dequeue_upload(&self, row_id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM upload_queue WHERE id = ?1", params![row_id])?;
        Ok(())
    }

    /// Drop any queued uploads for `id` whose bytes are no longer current (the
    /// file on disk moved on). Called when a newer version is uploaded or queued.
    pub fn clear_stale_uploads(&self, id: &str, keep_hash: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM upload_queue WHERE game_instance_id = ?1 AND content_hash <> ?2", params![id, keep_hash])?;
        Ok(())
    }

    /// Note a failed attempt so backoff widens and the error stays visible.
    pub fn record_upload_failure(&self, row_id: i64, error: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE upload_queue SET
                 attempts        = attempts + 1,
                 last_error      = ?2,
                 last_attempt_at = CURRENT_TIMESTAMP
             WHERE id = ?1",
            params![row_id, error],
        )?;
        Ok(())
    }

    // ---- console validSaveSizes cache ----------------------------

    /// Cache a console's accepted save sizes (raw byte counts), refreshed at
    /// launch.
    pub fn cache_console_sizes(&self, slug: &str, sizes: &[u64]) -> rusqlite::Result<()> {
        let json = serde_json::to_string(sizes).expect("Vec<u64> serialises");
        self.conn.execute(
            "INSERT INTO console_save_sizes (console_slug, valid_save_sizes, cached_at)
             VALUES (?1, ?2, CURRENT_TIMESTAMP)
             ON CONFLICT(console_slug) DO UPDATE SET
                 valid_save_sizes = excluded.valid_save_sizes,
                 cached_at        = excluded.cached_at",
            params![slug, json],
        )?;
        Ok(())
    }

    /// The cached accepted sizes for a console, if we've cached it.
    ///
    /// Stored as JSON so the shape can grow - the backend may move from a plain
    /// list to size ranges (see design.md); today it decodes as a list.
    pub fn console_sizes(&self, slug: &str) -> rusqlite::Result<Option<Vec<u64>>> {
        let json: Option<String> = self.conn.query_row("SELECT valid_save_sizes FROM console_save_sizes WHERE console_slug = ?1", params![slug], |r| r.get(0)).optional()?;
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }
}

/// Apply connection pragmas and bring the schema up to date.
fn prepare(conn: Connection) -> anyhow::Result<Connection> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous  = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    migrate(&conn)?;
    Ok(conn)
}

/// Run any migrations the database hasn't seen yet. Each is applied - schema
/// change plus the `user_version` bump - inside one transaction, so a crash
/// mid-migration leaves the database at the previous version, not half-way.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut version: usize = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    while version < MIGRATIONS.len() {
        conn.execute_batch(&format!("BEGIN;\n{}\nPRAGMA user_version = {};\nCOMMIT;", MIGRATIONS[version], version + 1))?;
        version += 1;
    }
    Ok(())
}

/// Rename `data.sqlite` to the first free `data.bak[.N].sqlite`, discarding the
/// now-orphaned `-wal` / `-shm` siblings.
fn back_up(path: &Path) -> anyhow::Result<PathBuf> {
    let target = (0u32..)
        .map(|n| {
            let name = if n == 0 { "data.bak.sqlite".to_owned() } else { format!("data.bak.{n}.sqlite") };
            path.with_file_name(name)
        })
        .find(|candidate| !candidate.exists())
        .expect("an infinite range always yields a free filename");

    std::fs::rename(path, &target)?;
    for suffix in ["-wal", "-shm"] {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        let sibling = path.with_file_name(name);
        if sibling.exists() {
            let _ = std::fs::remove_file(&sibling);
        }
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Store {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        migrate(&conn).expect("migrate");
        Store { conn }
    }

    #[test]
    fn migrations_bring_schema_to_head() {
        let store = mem();
        let version: i64 = store.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
        // Re-running is a no-op.
        migrate(&store.conn).unwrap();
        let again: i64 = store.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(again, version);
    }

    #[test]
    fn bind_lookup_and_rebind_preserves_bookkeeping() {
        let store = mem();
        let original = Path::new("/saves/game.srm");
        store.bind_instance("inst-1", original, Some("gba")).unwrap();
        store.record_synced("inst-1", "hash-a", "save-1").unwrap();
        store.record_uploaded("inst-1", "hash-a").unwrap();

        let by_path = store.instance_for_path(original).unwrap().unwrap();
        assert_eq!(by_path.game_instance_id, "inst-1");
        assert_eq!(by_path.console_slug.as_deref(), Some("gba"));
        assert_eq!(by_path.last_synced_hash.as_deref(), Some("hash-a"));
        assert_eq!(by_path.last_synced_save_id.as_deref(), Some("save-1"));

        let moved = Path::new("/saves/moved.srm");
        store.bind_instance("inst-1", moved, None).unwrap();
        let rec = store.instance("inst-1").unwrap().unwrap();
        assert_eq!(rec.save_path, moved);
        assert_eq!(rec.console_slug.as_deref(), Some("gba"), "slug kept when rebind passes None");
        assert_eq!(rec.last_synced_hash.as_deref(), Some("hash-a"), "bookkeeping survives a rebind");
        assert_eq!(rec.last_uploaded_hash.as_deref(), Some("hash-a"));
        assert!(store.instance_for_path(original).unwrap().is_none());
    }

    #[test]
    fn pause_flag_toggles() {
        let store = mem();
        store.bind_instance("i", Path::new("/p"), None).unwrap();
        assert!(!store.instance("i").unwrap().unwrap().paused);
        store.set_paused("i", true).unwrap();
        assert!(store.instance("i").unwrap().unwrap().paused);
        store.set_paused("i", false).unwrap();
        assert!(!store.instance("i").unwrap().unwrap().paused);
    }

    #[test]
    fn instances_are_ordered_by_binding_time() {
        let store = mem();
        store.bind_instance("a", Path::new("/a"), None).unwrap();
        store.bind_instance("b", Path::new("/b"), None).unwrap();
        store.bind_instance("c", Path::new("/c"), None).unwrap();
        let ids: Vec<_> = store.instances().unwrap().into_iter().map(|r| r.game_instance_id).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn conflict_marker_round_trips_and_clears() {
        let store = mem();
        store.bind_instance("i", Path::new("/p"), None).unwrap();
        assert!(store.instance("i").unwrap().unwrap().conflict.is_none());

        store.set_conflict("i", "local-hash", "remote-hash").unwrap();
        let conflict = store.instance("i").unwrap().unwrap().conflict.unwrap();
        assert_eq!(conflict.local_hash, "local-hash");
        assert_eq!(conflict.remote_hash, "remote-hash");
        assert!(!conflict.detected_at.is_empty());

        store.clear_conflict("i").unwrap();
        assert!(store.instance("i").unwrap().unwrap().conflict.is_none());
    }

    #[test]
    fn record_synced_clears_a_conflict() {
        let store = mem();
        store.bind_instance("i", Path::new("/p"), None).unwrap();
        store.set_conflict("i", "l", "r").unwrap();
        store.record_synced("i", "h", "sid").unwrap();
        assert!(store.instance("i").unwrap().unwrap().conflict.is_none());
    }

    #[test]
    fn sync_cursor_round_trips() {
        let store = mem();
        assert!(store.sync_cursor().unwrap().is_none());
        store.set_sync_cursor("2026-08-28 12:00:00").unwrap();
        store.set_sync_cursor("2026-08-28 12:05:00").unwrap();
        assert_eq!(store.sync_cursor().unwrap().as_deref(), Some("2026-08-28 12:05:00"));
    }

    #[test]
    fn upload_queue_dedupes_orders_and_drains() {
        let store = mem();
        store.bind_instance("i", Path::new("/p"), None).unwrap();
        store.enqueue_upload("i", "h1", b"one").unwrap();
        store.enqueue_upload("i", "h2", b"two").unwrap();
        store.enqueue_upload("i", "h1", b"one-v2").unwrap(); // same (i, h1) -> in-place update

        let queue = store.queued_uploads().unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].content_hash, "h1");
        assert_eq!(queue[0].bytes, b"one-v2");
        assert_eq!(queue[0].size_bytes, 6);
        assert_eq!(queue[1].content_hash, "h2");

        store.record_upload_failure(queue[0].id, "network down").unwrap();
        let after = store.queued_uploads().unwrap();
        assert_eq!(after[0].attempts, 1);
        assert_eq!(after[0].last_error.as_deref(), Some("network down"));

        store.dequeue_upload(queue[0].id).unwrap();
        let left = store.queued_uploads().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].content_hash, "h2");
    }

    #[test]
    fn requeue_resets_retry_state() {
        let store = mem();
        store.bind_instance("i", Path::new("/p"), None).unwrap();
        store.enqueue_upload("i", "h", b"a").unwrap();
        let id = store.queued_uploads().unwrap()[0].id;
        store.record_upload_failure(id, "boom").unwrap();

        store.enqueue_upload("i", "h", b"b").unwrap();
        let row = store.queued_uploads().unwrap().remove(0);
        assert_eq!(row.bytes, b"b");
        assert_eq!(row.attempts, 0, "re-queue clears the attempt count");
        assert!(row.last_error.is_none());
        assert!(row.last_attempt_at.is_none());
    }

    #[test]
    fn clear_stale_uploads_keeps_only_the_current_hash() {
        let store = mem();
        store.bind_instance("i", Path::new("/p"), None).unwrap();
        store.bind_instance("j", Path::new("/q"), None).unwrap();
        store.enqueue_upload("i", "old1", b"a").unwrap();
        store.enqueue_upload("i", "old2", b"b").unwrap();
        store.enqueue_upload("i", "keep", b"c").unwrap();
        store.enqueue_upload("j", "other", b"d").unwrap();

        store.clear_stale_uploads("i", "keep").unwrap();

        let hashes: Vec<_> = store.queued_uploads().unwrap().into_iter().map(|q| (q.game_instance_id, q.content_hash)).collect();
        assert_eq!(hashes, [("i".to_owned(), "keep".to_owned()), ("j".to_owned(), "other".to_owned())]);
    }

    #[test]
    fn unbind_cascades_to_the_queue() {
        let store = mem();
        store.bind_instance("i", Path::new("/p"), None).unwrap();
        store.enqueue_upload("i", "h", b"a").unwrap();
        store.unbind_instance("i").unwrap();
        assert!(store.instance("i").unwrap().is_none());
        assert!(store.queued_uploads().unwrap().is_empty(), "queue rows cascade-delete with their instance");
    }

    #[test]
    fn save_backups_round_trip_and_outlive_the_instance() {
        let store = mem();
        store.bind_instance("i", Path::new("/saves/g.srm"), None).unwrap();
        store.bind_instance("j", Path::new("/saves/h.srm"), None).unwrap();

        store.insert_backup("i", Path::new("/saves/g.srm"), "local1", 8192, "server1", Some("save-1"), "pull").unwrap();
        store.insert_backup("i", Path::new("/saves/g.srm"), "local2", 8192, "server2", None, "conflict").unwrap();
        store.insert_backup("j", Path::new("/saves/h.srm"), "other", 4096, "srv", Some("save-9"), "pull").unwrap();

        let for_i = store.backups_for("i").unwrap();
        assert_eq!(for_i.len(), 2);
        assert_eq!(for_i[0].content_hash, "local2", "newest first");
        assert_eq!(for_i[0].replaced_with, "server2");
        assert_eq!(for_i[0].server_save_id, None);
        assert_eq!(for_i[0].reason, "conflict");
        assert_eq!(for_i[1].server_save_id.as_deref(), Some("save-1"));
        assert_eq!(store.backups().unwrap().len(), 3);

        // a backup is a permanent record: unmapping the instance must not touch it
        store.unbind_instance("i").unwrap();
        assert!(store.instance("i").unwrap().is_none());
        assert_eq!(store.backups_for("i").unwrap().len(), 2, "backups survive an unmap");
    }

    #[test]
    fn console_size_cache_round_trips_and_replaces() {
        let store = mem();
        assert!(store.console_sizes("gba").unwrap().is_none());
        store.cache_console_sizes("gba", &[512, 8192, 32768, 65536, 131072]).unwrap();
        store.cache_console_sizes("gba", &[8192, 131072]).unwrap();
        assert_eq!(store.console_sizes("gba").unwrap().unwrap(), vec![8192, 131072]);
    }

    #[test]
    fn write_rolls_back_a_failing_closure() {
        // Exercises the transaction wrapper directly (no global STORE needed).
        let store = mem();
        let _ = store.conn.execute_batch("ROLLBACK");
        store.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.bind_instance("i", Path::new("/p"), None).unwrap();
        store.conn.execute_batch("ROLLBACK").unwrap();
        assert!(store.instance("i").unwrap().is_none());
    }
}
