//! The sync engine: consumes `api::SyncEvent`, watches the mapped save files,
//! reconciles each instance's server state against its file on disk, and acts,
//! both directions, with an offline upload queue.
//!
//! One worker thread owns the `SyncStream`, a debounced filesystem watcher, and
//! all the I/O; `App` only drains [`EngineEvent`]s and sends [`Control`]
//! messages. Bookkeeping goes through the global [`Store`], which is built for
//! exactly this cross-thread use.

mod disk;
mod hash;
mod reconcile;
mod time;

pub use disk::{LocalFile, write_atomic};
pub use hash::sha256_hex;
pub use reconcile::{Action, reconcile};
pub use time::{humanize_since, now_epoch, now_utc_string, parse_utc};

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryIter};
use std::time::{Duration, Instant};

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, RecommendedCache, new_debouncer};

use crate::api::{Client, GameInstance, SaveMeta, SyncEvent, SyncStream, Timestamp};
use crate::config::{Config, PollInterval, UploadTrigger};
use crate::constants::BACKUP_DIR;
use crate::store::{InstanceRecord, QueuedUpload, Store};

type FsWatcher = notify_debouncer_full::Debouncer<notify::RecommendedWatcher, RecommendedCache>;

/// How long the debouncer waits for filesystem quiet before flushing a burst.
const WATCH_DEBOUNCE: Duration = Duration::from_secs(3);
/// Belt-and-braces rescan + queue-drain cadence (catches missed fs events).
const IDLE_RESCAN: Duration = Duration::from_secs(60);

/// A queued upload that has failed this many times running is "stuck", not just
/// briefly offline - worth a one-time `EngineEvent::Stuck`.
const STUCK_AFTER_ATTEMPTS: u32 = 3;

/// Stream connectivity, for a Home status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Online,
    Offline,
}

/// What the engine surfaces to `App`, UI-facing only (the catalog for Home,
/// notifications, status). The filesystem and network work happens on the
/// worker, not here.
pub enum EngineEvent {
    Status(Status),
    /// The full instance list from a hydrate (`?since=None`). `App` keeps this as
    /// the catalog Home renders.
    Hydrated {
        instances: Vec<GameInstance>,
    },
    /// One instance's latest save moved. `App` patches that catalog row.
    SaveAdvanced {
        instance_id: String,
        latest: SaveMeta,
    },
    /// A newer save was downloaded and written to disk.
    Pulled {
        instance_id: String,
    },
    /// A user-chosen older save (server history or a local pre-overwrite backup)
    /// was written to disk and re-uploaded as the newest save.
    Restored {
        instance_id: String,
    },
    /// Local bytes were uploaded to the server (live or from the queue).
    Pushed {
        instance_id: String,
    },
    /// The local file is ahead of the server but `[sync].upload_trigger` is
    /// `manual`, so nothing was uploaded.
    PushPending {
        instance_id: String,
    },
    /// A true conflict was recorded (both sides moved since the last sync). Home
    /// shows the picker.
    Conflict {
        instance_id: String,
    },
    /// One instance's sync is wedged and won't recover on its own: an incoming
    /// update was held back because the local save couldn't be snapshotted
    /// first, or an upload has failed on repeat. Toast-worthy, unlike [`Self::Error`].
    Stuck {
        instance_id: String,
        reason: StuckReason,
    },
    /// Non-fatal; the stream keeps running. Logged only.
    Error(String),
    /// The session is dead (`401`/`403` mid-sync). `App` must sign out + notify.
    SessionExpired,
}

/// Why an instance is [`EngineEvent::Stuck`]. `App` turns this into the toast copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckReason {
    /// A pull was skipped: the current local save couldn't be backed up, so
    /// overwriting it would risk unsaved bytes.
    BackupFailed,
    /// A queued upload has failed several times in a row and is still retrying.
    UploadRetrying,
}

pub enum Control {
    /// Force an immediate `?since=` poll (a "Sync now" button / tray item).
    SyncNow,
    /// Re-fetch the whole instance list (`?since=None`) and re-emit `Hydrated` —
    /// picks up instances created since the engine started (e.g. right after
    /// Home's add-game flow, or one made on the website). A refresh button in
    /// Home sends this.
    Rehydrate,
    /// A Home action changed this instance's binding or pause state, re-check it
    /// against the server now instead of waiting for the next event.
    Recheck { instance_id: String },
    /// Resolve a recorded conflict by keeping one side: upload the local file
    /// (`keep_local`) or download the server's, either way clearing the marker.
    ResolveConflict { instance_id: String, keep_local: bool },
    /// Put an older save back on disk and re-upload it as the newest, so it
    /// sticks (the next reconcile would otherwise pull the newer one back over
    /// it). The current on-disk save is snapshotted first via the overwrite
    /// guard. Ignored unless the instance is mapped to a local file.
    Restore { instance_id: String, source: RestoreSource },
}

/// Where the bytes for a [`Control::Restore`] come from.
pub enum RestoreSource {
    /// A save still on the server, by its `saves` list id.
    Server { save_id: String },
    /// A local pre-overwrite snapshot, by the `content_hash` naming its blob in
    /// `BACKUP_DIR`.
    Backup { content_hash: String },
}

pub struct SyncEngine {
    rx: Receiver<EngineEvent>,
    control: Sender<Control>,
    stop: Arc<AtomicBool>,
}

impl SyncEngine {
    pub fn start(client: Client, wake: impl Fn() + Send + Clone + 'static) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (control_tx, control_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);

        std::thread::Builder::new().name("sync-engine".into()).spawn(move || Worker::start(client, tx, control_rx, stop_worker, wake).run()).expect("spawn sync-engine thread");

        // A sibling watcher: nudges this engine when an emulator starts / exits.
        // It shares `stop`, so it dies with the engine.
        crate::emulator_watch::spawn(control_tx.clone(), Arc::clone(&stop));

        Self { rx, control: control_tx, stop }
    }

    pub fn events(&self) -> TryIter<'_, EngineEvent> {
        self.rx.try_iter()
    }

    /// Ask for an immediate poll (a "Sync now" button / tray item).
    pub fn sync_now(&self) {
        let _ = self.control.send(Control::SyncNow);
    }

    /// Re-fetch the whole instance list (a Home refresh / after add-game).
    pub fn rehydrate(&self) {
        let _ = self.control.send(Control::Rehydrate);
    }

    /// Re-check one instance now (after a bind / unpause in Home).
    pub fn recheck(&self, instance_id: impl Into<String>) {
        let _ = self.control.send(Control::Recheck { instance_id: instance_id.into() });
    }

    /// Resolve a conflict on one instance: keep the local file or the server's.
    pub fn resolve_conflict(&self, instance_id: impl Into<String>, keep_local: bool) {
        let _ = self.control.send(Control::ResolveConflict { instance_id: instance_id.into(), keep_local });
    }

    /// Restore an older save (server history or a local backup) to disk and
    /// re-upload it as the newest.
    pub fn restore(&self, instance_id: impl Into<String>, source: RestoreSource) {
        let _ = self.control.send(Control::Restore { instance_id: instance_id.into(), source });
    }
}

impl Drop for SyncEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Map `[sync].poll` onto the stream's belt-and-braces poll interval.
fn fallback_interval() -> Option<Duration> {
    Config::get(|c| match c.sync.poll {
        PollInterval::Auto => Some(Duration::from_secs(300)),
        PollInterval::Off => None,
        PollInterval::Every(d) => Some(d),
    })
}

struct Worker<W> {
    client: Client,
    stream: SyncStream,
    tx: Sender<EngineEvent>,
    control_rx: Receiver<Control>,
    stop: Arc<AtomicBool>,
    wake: W,
    /// Highest `uploaded_at` we've persisted as the restart cursor.
    cursor: Option<String>,
    /// Debounced filesystem watcher; `None` if it couldn't start.
    debouncer: Option<FsWatcher>,
    fs_rx: Receiver<DebounceEventResult>,
    /// Directories currently watched (the parents of mapped save paths).
    watched: HashSet<PathBuf>,
    /// Latest known server save per instance, from hydrate / `Changed`. Feeds
    /// the filesystem-triggered reconcile, which has no fresh server data.
    remote: HashMap<String, SaveMeta>,
    last_rescan: Instant,
}

impl<W: Fn() + Send + Clone + 'static> Worker<W> {
    fn start(client: Client, tx: Sender<EngineEvent>, control_rx: Receiver<Control>, stop: Arc<AtomicBool>, wake: W) -> Self {
        let cursor = Store::get(|s| s.sync_cursor()).unwrap_or_default();
        let stream = SyncStream::start(client.clone(), cursor.clone(), fallback_interval(), wake.clone());

        let (fs_tx, fs_rx) = std::sync::mpsc::channel();
        let debouncer = match new_debouncer(WATCH_DEBOUNCE, None, move |res| {
            let _ = fs_tx.send(res);
        }) {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!("filesystem watcher unavailable: {e}");
                None
            }
        };

        let mut worker = Self { client, stream, tx, control_rx, stop, wake, cursor, debouncer, fs_rx, watched: HashSet::new(), remote: HashMap::new(), last_rescan: Instant::now() };
        worker.resync_watches();
        worker
    }

    fn run(mut self) {
        tracing::debug!("sync engine started");
        while !self.stop.load(Ordering::Relaxed) {
            let mut worked = false;

            for event in self.stream.events().collect::<Vec<_>>() {
                self.handle(event);
                worked = true;
            }

            while let Ok(control) = self.control_rx.try_recv() {
                match control {
                    Control::SyncNow => {
                        self.stream.poll_now();
                        self.rescan(true);
                        self.drain_queue();
                    }
                    Control::Rehydrate => self.rehydrate(),
                    Control::Recheck { instance_id } => {
                        self.resync_watches();
                        self.recheck_one(&instance_id);
                    }
                    Control::ResolveConflict { instance_id, keep_local } => self.resolve_conflict(&instance_id, keep_local),
                    Control::Restore { instance_id, source } => self.restore(&instance_id, source),
                }
                worked = true;
            }

            let mut fs_activity = false;
            while let Ok(res) = self.fs_rx.try_recv() {
                match res {
                    Ok(_) => fs_activity = true,
                    Err(errs) => tracing::debug!("fs watch error x{}", errs.len()),
                }
                worked = true;
            }
            if fs_activity {
                self.rescan(false);
            }

            if self.last_rescan.elapsed() >= IDLE_RESCAN {
                self.last_rescan = Instant::now();
                self.rescan(false);
                self.drain_queue();
            }

            if !worked {
                std::thread::sleep(Duration::from_millis(150));
            }
        }
        self.stream.stop();
    }

    fn handle(&mut self, event: SyncEvent) {
        match event {
            SyncEvent::Connected => {
                self.emit(EngineEvent::Status(Status::Online));
                self.drain_queue();
            }
            SyncEvent::Disconnected { reason } => {
                tracing::debug!("stream offline: {reason}");
                self.emit(EngineEvent::Status(Status::Offline));
            }
            SyncEvent::Synced { instances } => self.hydrate(instances),
            SyncEvent::Changed { instance_id, latest } => {
                self.remote.insert(instance_id.clone(), latest.clone());
                self.settle(&instance_id, Some(&latest));
                self.advance_cursor(latest.uploaded_at.as_str());
                self.emit(EngineEvent::SaveAdvanced { instance_id, latest });
            }
            SyncEvent::Error { message } => self.emit(EngineEvent::Error(message)),
            SyncEvent::Unauthorized => self.emit(EngineEvent::SessionExpired),
        }
    }

    /// Full snapshot → the completeness pass (reconcile every locally-bound
    /// instance), refresh the watch set, then hand the list to `App` for Home.
    fn hydrate(&mut self, instances: Vec<GameInstance>) {
        tracing::debug!("hydrated {} instances", instances.len());
        let mut newest: Option<String> = self.cursor.clone();
        for instance in &instances {
            match &instance.latest_save {
                Some(save) => {
                    self.remote.insert(instance.id.clone(), save.clone());
                }
                None => {
                    self.remote.remove(&instance.id);
                }
            }
            if let Some(ts) = &instance.last_saved_at
                && newest.as_deref().is_none_or(|c| ts.as_str() > c)
            {
                newest = Some(ts.as_str().to_owned());
            }

            match Store::get(|s| s.instance(&instance.id)) {
                Ok(Some(book)) if !book.paused => self.act(&book, instance.latest_save.as_ref(), false),
                Ok(_) => {} // unbound on this device, or paused
                Err(e) => self.emit(EngineEvent::Error(format!("store: {e:#}"))),
            }
        }

        if let Some(cursor) = newest {
            self.advance_cursor(&cursor);
        }
        self.resync_watches();
        self.emit(EngineEvent::Hydrated { instances });
    }

    /// Re-fetch the whole list and run [`hydrate`] again.
    fn rehydrate(&mut self) {
        match self.client.game_instances(None) {
            Ok(instances) => self.hydrate(instances),
            Err(e) if e.is_unauthorized() => self.emit(EngineEvent::SessionExpired),
            Err(e) => self.emit(EngineEvent::Error(format!("rehydrate: {e}"))),
        }
    }

    /// One instance changed on the server. `latest` is `None` only for the
    /// same-second backfill path when the instance turned out to have no saves.
    fn settle(&mut self, instance_id: &str, latest: Option<&SaveMeta>) {
        match Store::get(|s| s.instance(instance_id)) {
            Ok(Some(book)) if !book.paused => self.act(&book, latest, false),
            Ok(_) => {}
            Err(e) => self.emit(EngineEvent::Error(format!("store: {e:#}"))),
        }
    }

    fn recheck_one(&mut self, instance_id: &str) {
        let book = match Store::get(|s| s.instance(instance_id)) {
            Ok(Some(book)) if !book.paused => book,
            Ok(_) => return,
            Err(e) => return self.emit(EngineEvent::Error(format!("store: {e:#}"))),
        };
        match self.client.saves(instance_id) {
            Ok(saves) => {
                if let Some(latest) = saves.first() {
                    self.remote.insert(instance_id.to_owned(), latest.clone());
                }
                self.act(&book, saves.first(), false);
            }
            Err(e) if e.is_unauthorized() => self.emit(EngineEvent::SessionExpired),
            Err(e) => self.emit(EngineEvent::Error(format!("saves {instance_id}: {e}"))),
        }
    }

    /// Keep one side of a conflict. Either way, `push` / `pull` call
    /// `record_synced`, which clears the conflict marker.
    fn resolve_conflict(&mut self, instance_id: &str, keep_local: bool) {
        let book = match Store::get(|s| s.instance(instance_id)) {
            Ok(Some(book)) => book,
            Ok(None) => return,
            Err(e) => return self.emit(EngineEvent::Error(format!("store: {e:#}"))),
        };
        if book.conflict.is_none() {
            return; // already resolved elsewhere
        }
        if keep_local {
            self.push(&book, true);
            return;
        }
        // Keep the server's copy: use the cached save, or fetch it.
        if let Some(remote) = self.remote.get(instance_id).cloned() {
            self.pull(&book, &remote, "conflict");
            return;
        }
        match self.client.saves(instance_id) {
            Ok(saves) => {
                if let Some(remote) = saves.first() {
                    self.remote.insert(instance_id.to_owned(), remote.clone());
                    self.pull(&book, remote, "conflict");
                }
            }
            Err(e) if e.is_unauthorized() => self.emit(EngineEvent::SessionExpired),
            Err(e) => self.emit(EngineEvent::Error(format!("saves {instance_id}: {e}"))),
        }
    }

    /// Put an older save back on disk and make it the newest on the server.
    ///
    /// The bytes come from server history or a local backup blob; either way the
    /// current on-disk save is snapshotted first (the overwrite guard), then the
    /// restored bytes are written and re-uploaded. The re-upload is what makes
    /// the restore stick: without a fresh newest-save row, the next reconcile
    /// sees `local != remote`, `synced == local` and pulls the newer save back.
    fn restore(&mut self, instance_id: &str, source: RestoreSource) {
        let book = match Store::get(|s| s.instance(instance_id)) {
            Ok(Some(book)) => book,
            Ok(None) => {
                // The UI disables restore when unmapped; this is just belt-and-braces.
                tracing::warn!("restore {instance_id}: not mapped to a local file, ignoring");
                return;
            }
            Err(e) => return self.emit(EngineEvent::Error(format!("store: {e:#}"))),
        };

        let (bytes, hash, server_save_id) = match &source {
            RestoreSource::Server { save_id } => {
                let bytes = match self.client.download_save(instance_id, save_id) {
                    Ok(bytes) => bytes,
                    Err(e) if e.is_unauthorized() => return self.emit(EngineEvent::SessionExpired),
                    Err(e) => return self.emit(EngineEvent::Error(format!("restore download {instance_id}: {e}"))),
                };
                let hash = sha256_hex(&bytes);
                (bytes, hash, Some(save_id.clone()))
            }
            RestoreSource::Backup { content_hash } => {
                let bytes = match read_backup_blob(&BACKUP_DIR, content_hash) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => return self.emit(EngineEvent::Error(format!("restore {instance_id}: the backup file is missing"))),
                    Err(e) => return self.emit(EngineEvent::Error(format!("restore read backup: {e}"))),
                };
                let hash = sha256_hex(&bytes);
                if &hash != content_hash {
                    return self.emit(EngineEvent::Error(format!("restore {instance_id}: the backup file is corrupt")));
                }
                (bytes, hash, None)
            }
        };

        // Snapshot the current on-disk save before we replace it.
        match self.guard_overwrite(&book, &hash, server_save_id.as_deref(), "restore") {
            Guard::Proceed => {}
            Guard::Deferred => return self.emit(EngineEvent::Error(format!("restore {instance_id}: the save file is in use, try again"))),
            Guard::Aborted => return,
        }

        if let Err(e) = write_atomic(&book.save_path, &bytes) {
            return self.emit(EngineEvent::Error(format!("restore write {}: {e}", book.save_path.display())));
        }

        // Re-upload so the restored bytes become the newest server save.
        let size = bytes.len() as u64;
        match self.client.upload_save(instance_id, bytes.clone()) {
            Ok(outcome) => {
                self.store_write(Store::write(|s| {
                    s.clear_stale_uploads(instance_id, &hash)?;
                    s.record_uploaded(instance_id, &hash)?;
                    s.record_synced(instance_id, &hash, &outcome.id)
                }));
                self.remote.insert(instance_id.to_owned(), synthetic_save(&outcome.id, &hash, size));
            }
            Err(e) if e.is_unauthorized() => return self.emit(EngineEvent::SessionExpired),
            Err(e) => {
                // Disk already holds the restored bytes; queue the upload so it
                // still becomes newest once we're back online.
                tracing::warn!("restore upload {instance_id} failed, queued: {e}");
                self.store_write(Store::write(|s| {
                    s.clear_stale_uploads(instance_id, &hash)?;
                    s.enqueue_upload(instance_id, &hash, &bytes)
                }));
            }
        }
        self.emit(EngineEvent::Restored { instance_id: instance_id.to_owned() });
    }

    /// Re-reconcile every mapped, non-paused instance against its cached server
    /// state. `force` uploads even under `upload_trigger = manual`.
    fn rescan(&mut self, force: bool) {
        let books = match Store::get(|s| s.instances()) {
            Ok(books) => books,
            Err(e) => return self.emit(EngineEvent::Error(format!("store: {e:#}"))),
        };
        for book in books {
            if book.paused {
                continue;
            }
            let remote = self.remote.get(&book.game_instance_id).cloned();
            self.act(&book, remote.as_ref(), force);
        }
    }

    /// Read the file, run [`reconcile`], carry out the verdict.
    fn act(&mut self, book: &InstanceRecord, remote: Option<&SaveMeta>, force_push: bool) {
        let local = match LocalFile::read(&book.save_path) {
            Ok(local) => local,
            Err(e) => return self.emit(EngineEvent::Error(format!("{}: {e}", book.save_path.display()))),
        };
        let policy = Config::get(|c| c.sync.conflict);
        let id = &book.game_instance_id;

        match reconcile(&local, remote, book, policy) {
            Action::Idle => {}
            Action::MarkSynced => {
                if let Some(remote) = remote {
                    self.store_write(Store::write(|s| s.record_synced(id, &remote.content_hash, &remote.id)));
                }
            }
            Action::Pull => self.pull(book, remote.expect("reconcile only returns Pull when there is a remote save"), "pull"),
            Action::Push => self.push(book, force_push),
            Action::Conflict => {
                let local_hash = local.hash().unwrap_or_default().to_owned();
                let remote_hash = remote.map(|r| r.content_hash.clone()).unwrap_or_default();
                self.store_write(Store::write(|s| s.set_conflict(id, &local_hash, &remote_hash)));
                self.emit(EngineEvent::Conflict { instance_id: id.clone() });
            }
        }
    }

    fn pull(&self, book: &InstanceRecord, remote: &SaveMeta, reason: &'static str) {
        let id = &book.game_instance_id;
        let bytes = match self.client.download_save(id, &remote.id) {
            Ok(bytes) => bytes,
            Err(e) if e.is_unauthorized() => return self.emit(EngineEvent::SessionExpired),
            Err(e) => return self.emit(EngineEvent::Error(format!("download {id}: {e}"))),
        };

        // The bytes must hash to what the server advertised, or we'd write a
        // corrupt save and record it as good.
        let got = sha256_hex(&bytes);
        if got != remote.content_hash {
            return self.emit(EngineEvent::Error(format!("{id}: downloaded hash {got} != advertised {}", remote.content_hash)));
        }

        // Never overwrite local bytes the user never uploaded without keeping a copy.
        match self.guard_overwrite(book, &remote.content_hash, Some(&remote.id), reason) {
            Guard::Proceed => {}
            Guard::Deferred => return tracing::debug!("{id}: save file busy, deferring pull"),
            Guard::Aborted => return,
        }

        if let Err(e) = write_atomic(&book.save_path, &bytes) {
            return self.emit(EngineEvent::Error(format!("write {}: {e}", book.save_path.display())));
        }
        self.store_write(Store::write(|s| s.record_synced(id, &remote.content_hash, &remote.id)));
        self.emit(EngineEvent::Pulled { instance_id: id.clone() });
    }

    /// Snapshot the current on-disk save before a pull writes over it, when the
    /// bytes are novel: not what we're about to write, and not the last thing we
    /// uploaded. `last_synced_hash` is deliberately *not* trusted here, since the
    /// map-time "use the server's copy" path seeds it to the local hash for bytes
    /// that were never sent anywhere.
    fn guard_overwrite(&self, book: &InstanceRecord, incoming_hash: &str, server_save_id: Option<&str>, reason: &'static str) -> Guard {
        let bytes = match disk::read_bytes(&book.save_path) {
            Ok(Some(bytes)) if !bytes.is_empty() => bytes,
            Ok(_) => return Guard::Proceed, // nothing on disk to keep
            Err(e) if disk::is_locked(&e) => return Guard::Deferred,
            Err(e) => return self.backup_stuck(book, format!("read {}: {e}", book.save_path.display())),
        };
        let hash = sha256_hex(&bytes);
        if !needs_backup(&hash, incoming_hash, book.last_uploaded_hash.as_deref()) {
            return Guard::Proceed;
        }
        if let Err(e) = write_backup_blob(&BACKUP_DIR, &hash, &bytes) {
            return self.backup_stuck(book, format!("write blob for {}: {e}", book.save_path.display()));
        }
        // Record it, then trim this game's history to `[backups].retain` in the
        // same transaction so a crash can't leave the index over-long.
        let keep = Config::get(|c| c.backups.retain);
        let recorded = Store::write(|s| {
            s.insert_backup(&book.game_instance_id, &book.save_path, &hash, bytes.len() as u64, incoming_hash, server_save_id, reason)?;
            s.prune_backups(&book.game_instance_id, keep)
        });
        match recorded {
            Ok(orphans) => {
                for h in orphans {
                    let _ = fs::remove_file(BACKUP_DIR.join(&h));
                }
            }
            Err(e) => return self.backup_stuck(book, format!("record: {e:#}")),
        }
        tracing::info!("kept a backup of {} before overwrite ({reason})", book.save_path.display());
        Guard::Proceed
    }

    /// The overwrite guard couldn't snapshot the local save, so a pull is being
    /// skipped rather than risk unsaved bytes. Log the cause, tell `App` this
    /// instance is stuck, and hand back [`Guard::Aborted`].
    fn backup_stuck(&self, book: &InstanceRecord, detail: String) -> Guard {
        tracing::warn!("backup {}: {detail}", book.game_instance_id);
        self.emit(EngineEvent::Stuck { instance_id: book.game_instance_id.clone(), reason: StuckReason::BackupFailed });
        Guard::Aborted
    }

    /// Local file is ahead of the server. Upload it (unless `manual` and not
    /// `force`); on failure, park it in the offline queue.
    fn push(&mut self, book: &InstanceRecord, force: bool) {
        let id = &book.game_instance_id;
        if !force && Config::get(|c| c.sync.upload_trigger) == UploadTrigger::Manual {
            return self.emit(EngineEvent::PushPending { instance_id: id.clone() });
        }

        // Read the bytes, retrying briefly if an emulator still holds the handle.
        let bytes = 'read: {
            for attempt in 0..5u32 {
                match disk::read_bytes(&book.save_path) {
                    Ok(Some(bytes)) => break 'read bytes,
                    Ok(None) => return, // vanished since we reconciled
                    Err(e) if disk::is_locked(&e) && attempt < 4 => std::thread::sleep(Duration::from_millis(250)),
                    Err(e) => return self.emit(EngineEvent::Error(format!("read {}: {e}", book.save_path.display()))),
                }
            }
            tracing::debug!("{id}: save still locked, will retry");
            return;
        };

        let hash = sha256_hex(&bytes);
        if book.last_uploaded_hash.as_deref() == Some(hash.as_str()) || self.is_queued(id, &hash) {
            return; // already sent, or the offline queue owns this version
        }
        let size = bytes.len() as u64;

        match self.client.upload_save(id, bytes.clone()) {
            Ok(outcome) => {
                self.store_write(Store::write(|s| {
                    s.clear_stale_uploads(id, &hash)?;
                    s.record_uploaded(id, &hash)?;
                    s.record_synced(id, &hash, &outcome.id)
                }));
                // Disk and server agree on `hash` now; hold a synthetic save
                // until the stream's catch-up delivers the real one.
                self.remote.insert(id.clone(), synthetic_save(&outcome.id, &hash, size));
                self.emit(EngineEvent::Pushed { instance_id: id.clone() });
            }
            Err(e) if e.is_unauthorized() => self.emit(EngineEvent::SessionExpired),
            Err(e) => {
                tracing::warn!("upload {id} failed, queued: {e}");
                self.store_write(Store::write(|s| {
                    s.clear_stale_uploads(id, &hash)?;
                    s.enqueue_upload(id, &hash, &bytes)
                }));
            }
        }
    }

    /// Retry the offline upload queue, oldest first, respecting per-item backoff.
    fn drain_queue(&mut self) {
        let items = match Store::get(|s| s.queued_uploads()) {
            Ok(items) => items,
            Err(e) => return self.emit(EngineEvent::Error(format!("store: {e:#}"))),
        };
        if items.is_empty() {
            return;
        }
        let now = time::now_epoch();
        for item in items {
            if !ready_to_retry(&item, now) {
                continue;
            }
            match self.client.upload_save(&item.game_instance_id, item.bytes.clone()) {
                Ok(_) => {
                    tracing::info!("uploaded queued save for {}", item.game_instance_id);
                    self.store_write(Store::write(|s| {
                        s.dequeue_upload(item.id)?;
                        s.record_uploaded(&item.game_instance_id, &item.content_hash)
                    }));
                    self.emit(EngineEvent::Pushed { instance_id: item.game_instance_id });
                }
                Err(e) if e.is_unauthorized() => return self.emit(EngineEvent::SessionExpired),
                Err(e) => {
                    let attempts = item.attempts + 1;
                    tracing::warn!("queued upload for {} failed (attempt {attempts}): {e}", item.game_instance_id);
                    self.store_write(Store::write(|s| s.record_upload_failure(item.id, &e.to_string())));
                    // Once, when it first crosses the line into "not just a blip".
                    if attempts == STUCK_AFTER_ATTEMPTS {
                        self.emit(EngineEvent::Stuck { instance_id: item.game_instance_id.clone(), reason: StuckReason::UploadRetrying });
                    }
                }
            }
        }
    }

    fn is_queued(&self, id: &str, hash: &str) -> bool {
        Store::get(|s| s.queued_uploads()).map(|q| q.iter().any(|item| item.game_instance_id == id && item.content_hash == hash)).unwrap_or(false)
    }

    /// Watch the parent directory of every mapped save path; drop the rest.
    /// (Watching the file directly is unreliable across write-temp-then-rename.)
    fn resync_watches(&mut self) {
        let Some(mut debouncer) = self.debouncer.take() else { return };

        let wanted: HashSet<PathBuf> = match Store::get(|s| s.instances()) {
            Ok(books) => books.iter().filter_map(|b| b.save_path.parent().map(Path::to_path_buf)).filter(|d| d.is_dir()).collect(),
            Err(e) => {
                tracing::warn!("resync watches: {e:#}");
                self.debouncer = Some(debouncer);
                return;
            }
        };

        for dir in self.watched.difference(&wanted).cloned().collect::<Vec<_>>() {
            let _ = debouncer.unwatch(&dir);
            self.watched.remove(&dir);
        }
        for dir in wanted.difference(&self.watched).cloned().collect::<Vec<_>>() {
            match debouncer.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(dir);
                }
                Err(e) => tracing::debug!("watch {}: {e}", dir.display()),
            }
        }

        self.debouncer = Some(debouncer);
    }

    fn advance_cursor(&mut self, ts: &str) {
        if self.cursor.as_deref().is_none_or(|c| ts > c) {
            self.cursor = Some(ts.to_owned());
            self.store_write(Store::write(|s| s.set_sync_cursor(ts)));
        }
    }

    fn store_write(&self, result: anyhow::Result<()>) {
        if let Err(e) = result {
            self.emit(EngineEvent::Error(format!("store: {e:#}")));
        }
    }

    fn emit(&self, event: EngineEvent) {
        let _ = self.tx.send(event);
        (self.wake)();
    }
}

/// Outcome of [`Worker::guard_overwrite`].
enum Guard {
    /// Safe to overwrite: nothing worth keeping, or a snapshot is stored.
    Proceed,
    /// The file is locked (an emulator has it); skip and retry next round.
    Deferred,
    /// The snapshot failed; abort rather than lose bytes.
    Aborted,
}

/// Whether the local bytes need snapshotting before an overwrite: novel unless
/// they're what we're about to write, or the last thing we uploaded (either way
/// they're recoverable).
fn needs_backup(local_hash: &str, incoming_hash: &str, last_uploaded: Option<&str>) -> bool {
    local_hash != incoming_hash && last_uploaded != Some(local_hash)
}

/// Write pre-overwrite bytes to `dir/<hash>`. Content-addressed, so identical
/// bytes are stored once; an existing blob is left alone.
fn write_backup_blob(dir: &Path, hash: &str, bytes: &[u8]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(hash);
    if path.exists() {
        return Ok(());
    }
    write_atomic(&path, bytes)
}

/// Read a backup blob written by [`write_backup_blob`]. `Ok(None)` if it isn't
/// there (pruned, or a stale index row).
fn read_backup_blob(dir: &Path, hash: &str) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(dir.join(hash)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// A best-effort `SaveMeta` for a save we just uploaded, before the stream's
/// catch-up hands us the server's real row (with the real `uploaded_at`).
fn synthetic_save(save_id: &str, hash: &str, size: u64) -> SaveMeta {
    SaveMeta { id: save_id.to_owned(), size_bytes: size, uploaded_at: Timestamp(time::now_utc_string()), starred: false, note: None, content_hash: hash.to_owned() }
}

/// Exponential backoff on a queued upload: 5s, 10s, 20s, … capped at 5 min.
fn ready_to_retry(item: &QueuedUpload, now: i64) -> bool {
    if item.attempts == 0 {
        return true;
    }
    let Some(last) = item.last_attempt_at.as_deref().and_then(time::parse_utc) else {
        return true;
    };
    let shift = item.attempts.min(8).saturating_sub(1);
    let wait = (5_i64 << shift).min(300);
    now - last >= wait
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(attempts: u32, last_attempt_at: Option<&str>) -> QueuedUpload {
        QueuedUpload {
            id: 1,
            game_instance_id: "i".into(),
            content_hash: "h".into(),
            bytes: vec![],
            size_bytes: 0,
            attempts,
            last_error: None,
            queued_at: "2026-08-28 00:00:00".into(),
            last_attempt_at: last_attempt_at.map(str::to_owned),
        }
    }

    #[test]
    fn backoff_widens_with_attempts() {
        let now = time::parse_utc("2026-08-28 12:00:00").unwrap();
        // never tried, or no timestamp: always ready
        assert!(ready_to_retry(&queued(0, None), now));
        assert!(ready_to_retry(&queued(3, None), now));

        // 1st retry waits 5s
        assert!(!ready_to_retry(&queued(1, Some("2026-08-28 11:59:57")), now));
        assert!(ready_to_retry(&queued(1, Some("2026-08-28 11:59:55")), now));

        // 4th retry waits 40s
        assert!(!ready_to_retry(&queued(4, Some("2026-08-28 11:59:30")), now));
        assert!(ready_to_retry(&queued(4, Some("2026-08-28 11:59:19")), now));

        // caps at 300s no matter how many attempts
        assert!(!ready_to_retry(&queued(20, Some("2026-08-28 11:56:00")), now));
        assert!(ready_to_retry(&queued(20, Some("2026-08-28 11:54:00")), now));
    }

    #[test]
    fn synthetic_save_carries_the_uploaded_hash() {
        let s = synthetic_save("save-9", "abc123", 4096);
        assert_eq!(s.id, "save-9");
        assert_eq!(s.content_hash, "abc123");
        assert_eq!(s.size_bytes, 4096);
        assert!(!s.starred);
        assert!(time::parse_utc(s.uploaded_at.as_str()).is_some());
    }

    #[test]
    fn needs_backup_only_for_bytes_we_cant_recover() {
        // novel local bytes about to be replaced: keep them
        assert!(needs_backup("local", "incoming", None));
        assert!(needs_backup("local", "incoming", Some("something-else")));
        // already equal to what we're writing: nothing lost
        assert!(!needs_backup("same", "same", None));
        // it's the last thing we uploaded: recoverable from the server
        assert!(!needs_backup("local", "incoming", Some("local")));
    }

    #[test]
    fn write_backup_blob_is_content_addressed_and_idempotent() {
        let dir = std::env::temp_dir().join(format!("coincell-backup-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);

        write_backup_blob(&dir, "hash-a", b"first").unwrap();
        assert_eq!(fs::read(dir.join("hash-a")).unwrap(), b"first");

        // a second call for the same hash leaves the existing blob untouched
        write_backup_blob(&dir, "hash-a", b"different bytes, same name").unwrap();
        assert_eq!(fs::read(dir.join("hash-a")).unwrap(), b"first");

        write_backup_blob(&dir, "hash-b", b"second").unwrap();
        assert_eq!(fs::read(dir.join("hash-b")).unwrap(), b"second");

        let _ = fs::remove_dir_all(&dir);
    }
}
