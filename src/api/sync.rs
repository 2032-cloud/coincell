//! Stateful sync orchestration: the connect-stream-then-poll dance from the
//! design doc, turned into a stream of high-level facts.
//!
//! It owns an [`EventStream`] (which handles its own reconnection) and a
//! `?since=` cursor. It does **not** touch the filesystem or any local store —
//! it only tells a consumer *which* instances have a newer save on the server
//! and *what* that save is. Applying that (diffing against disk, downloading,
//! queueing uploads, conflict resolution) is the sync engine's job, downstream.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryIter};
use std::time::{Duration, Instant};

use crate::api::events::{EventStream, StreamEvent};
use crate::api::{Client, Error, GameInstance, SaveMeta};

pub enum SyncEvent {
    /// The realtime stream connected (fired on first connect and each reconnect).
    Connected,
    /// The realtime stream dropped; it will retry on its own.
    Disconnected { reason: String },
    /// A full snapshot of every instance (initial hydrate only). Use it to build
    /// local state from scratch.
    Synced { instances: Vec<GameInstance> },
    /// The server has this save for `instance_id` and the consumer may not.
    Changed { instance_id: String, latest: SaveMeta },
    /// A poll or fetch failed. Not fatal: the stream keeps running.
    Error { message: String },
    /// A poll or fetch got `401`/`403`: the session is dead. The consumer
    /// should log out, which drops this `SyncStream` and stops the retries.
    Unauthorized,
}

/// Forward a client error as the right `SyncEvent`: `Unauthorized` is its own
/// variant so the consumer doesn't have to string-match a dead session.
fn send_err(tx: &Sender<SyncEvent>, e: Error, wake: &impl Fn()) {
    let _ = tx.send(if e.is_unauthorized() { SyncEvent::Unauthorized } else { SyncEvent::Error { message: e.to_string() } });
    wake();
}

pub struct SyncStream {
    rx: Receiver<SyncEvent>,
    poke: Sender<()>,
    stop: Arc<AtomicBool>,
}

impl SyncStream {
    /// `since` seeds the cursor (pass the last value persisted from a previous
    /// run, or `None` for a full hydrate). `fallback` is a belt-and-braces poll
    /// interval for when the stream is quiet; `None` disables it.
    pub fn start(client: Client, since: Option<String>, fallback: Option<Duration>, wake: impl Fn() + Send + Clone + 'static) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (poke, poke_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);

        std::thread::Builder::new().name("api-sync".into()).spawn(move || run(client, since, fallback, tx, poke_rx, &stop_worker, wake)).expect("spawn api-sync thread");

        Self { rx, poke, stop }
    }

    pub fn events(&self) -> TryIter<'_, SyncEvent> {
        self.rx.try_iter()
    }

    /// Ask for an immediate `?since=` poll (a "sync now" button).
    pub fn poll_now(&self) {
        let _ = self.poke.send(());
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for SyncStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run(client: Client, since: Option<String>, fallback: Option<Duration>, tx: Sender<SyncEvent>, poke_rx: Receiver<()>, stop: &AtomicBool, wake: impl Fn() + Send + Clone + 'static) {
    // Stream first, so nothing that lands between "poll returned" and "stream is
    // live" is missed.
    let stream = EventStream::start(client.base(), client.session(), wake.clone());
    let mut cursor = since;
    let mut seen_connect = false;
    let mut last_fallback = Instant::now();

    // Initial full hydrate.
    match client.game_instances(None) {
        Ok(instances) => {
            cursor = advance_cursor(cursor, &instances);
            let _ = tx.send(SyncEvent::Synced { instances });
            wake();
        }
        Err(e) => send_err(&tx, e, &wake),
    }

    while !stop.load(Ordering::Relaxed) {
        let mut want_poll = drain_pokes(&poke_rx);

        match stream.recv_timeout(Duration::from_millis(300)) {
            Some(StreamEvent::Connected) => {
                let _ = tx.send(SyncEvent::Connected);
                wake();
                if seen_connect {
                    want_poll = true; // reconnect: backfill anything missed while down
                }
                seen_connect = true;
            }
            Some(StreamEvent::Disconnected { reason }) => {
                let _ = tx.send(SyncEvent::Disconnected { reason });
                wake();
            }
            Some(StreamEvent::SaveChanged { instance_id }) => {
                let emitted = poll_and_emit(&client, &mut cursor, &tx, &wake);
                backfill_one(&client, &instance_id, &emitted, &tx, &wake);
                want_poll = false;
                last_fallback = Instant::now();
            }
            None => {}
        }

        if fallback.is_some_and(|interval| last_fallback.elapsed() >= interval) {
            want_poll = true;
        }
        if want_poll {
            poll_and_emit(&client, &mut cursor, &tx, &wake);
            last_fallback = Instant::now();
        }
    }

    stream.stop();
}

fn drain_pokes(poke_rx: &Receiver<()>) -> bool {
    let mut any = false;
    while poke_rx.try_recv().is_ok() {
        any = true;
    }
    any
}

/// Poll `?since=cursor`, emit a `Changed` per instance, advance the cursor.
/// Returns the ids it emitted for.
fn poll_and_emit(client: &Client, cursor: &mut Option<String>, tx: &Sender<SyncEvent>, wake: &impl Fn()) -> Vec<String> {
    match client.game_instances(cursor.as_deref()) {
        Ok(instances) => {
            *cursor = advance_cursor(cursor.take(), &instances);
            let mut ids = Vec::with_capacity(instances.len());
            for instance in instances {
                ids.push(instance.id.clone());
                if let Some(latest) = instance.latest_save {
                    let _ = tx.send(SyncEvent::Changed { instance_id: instance.id, latest });
                }
            }
            if !ids.is_empty() {
                wake();
            }
            ids
        }
        Err(e) => {
            send_err(tx, e, wake);
            Vec::new()
        }
    }
}

/// The `?since=` filter is strictly `>`, so a save sharing a second with the
/// cursor can slip through. If the stream named an instance the poll didn't
/// return, fetch that one directly.
fn backfill_one(client: &Client, instance_id: &str, emitted: &[String], tx: &Sender<SyncEvent>, wake: &impl Fn()) {
    if emitted.iter().any(|id| id == instance_id) {
        return;
    }
    match client.saves(instance_id) {
        Ok(mut saves) if !saves.is_empty() => {
            let _ = tx.send(SyncEvent::Changed { instance_id: instance_id.to_owned(), latest: saves.remove(0) });
            wake();
        }
        Ok(_) => {}
        Err(e) => send_err(tx, e, wake),
    }
}

fn advance_cursor(prev: Option<String>, instances: &[GameInstance]) -> Option<String> {
    let mut best = prev;
    for instance in instances {
        let Some(ts) = &instance.last_saved_at else { continue };
        if best.as_deref().is_none_or(|current| ts.as_str() > current) {
            best = Some(ts.as_str().to_owned());
        }
    }
    best
}
