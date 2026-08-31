//! Watches the process list for known emulators and nudges the sync engine when
//! one **starts** (force-pull: catch anything from another device that landed
//! while we were idle, before the emulator loads a stale save) or **exits**
//! (force-push, which is what makes `[sync].upload_trigger = "on-emulator-exit"`
//! actually mean something).
//!
//! There is nothing here about ROMs or launch commands - which save file belongs
//! to which game already comes from the path <-> instance mapping. This is a
//! coarse "an emulator's state changed, re-check everything" signal.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::config::Config;
use crate::sync::Control;

const POLL: Duration = Duration::from_secs(4);

/// Start the watcher thread. It shares `stop` with the engine, so it ends when
/// the engine is dropped.
pub fn spawn(control: Sender<Control>, stop: Arc<AtomicBool>) {
    std::thread::Builder::new().name("emulator-watch".into()).spawn(move || run(&control, &stop)).expect("spawn emulator-watch thread");
}

fn run(control: &Sender<Control>, stop: &AtomicBool) {
    let mut sys = System::new();
    // Seed from what's already running: an emulator open when CoinCell launches
    // must not fire a "just started" pull, but its later exit still fires a push.
    let mut running = scan(&mut sys);
    tracing::debug!("emulator watch started; already running: {running:?}");

    while !stop.load(Ordering::Relaxed) {
        sleep_with_stop(POLL, stop);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !Config::get(|c| c.sync.enabled && c.sync.watch_emulators) {
            running.clear();
            continue;
        }
        let now = scan(&mut sys);
        if now != running {
            tracing::info!("emulator watch: {running:?} -> {now:?}; nudging sync");
            let _ = control.send(Control::SyncNow);
            running = now;
        }
    }
}

/// The subset of `[sync].emulators` whose process is running right now.
fn scan(sys: &mut System) -> HashSet<String> {
    let wanted: Vec<String> = Config::get(|c| c.sync.emulators.iter().map(|e| stem(e)).collect());
    if wanted.is_empty() {
        return HashSet::new();
    }
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::Always));

    let mut hits = HashSet::new();
    for proc in sys.processes().values() {
        let name = stem(&proc.name().to_string_lossy());
        let exe = proc.exe().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().to_lowercase());
        for w in &wanted {
            if name == *w || exe.as_deref() == Some(w.as_str()) {
                hits.insert(w.clone());
            }
        }
    }
    hits
}

/// Lowercase, trailing extension stripped: `RetroArch.exe` -> `retroarch`,
/// `pcsx2-qt` -> `pcsx2-qt`.
fn stem(s: &str) -> String {
    let s = s.trim().to_lowercase();
    match s.rsplit_once('.') {
        Some((base, ext)) if !base.is_empty() && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()) => base.to_owned(),
        _ => s,
    }
}

fn sleep_with_stop(total: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(200);
    let mut slept = Duration::ZERO;
    while slept < total && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(step);
        slept += step;
    }
}

#[cfg(test)]
mod tests {
    use super::stem;

    #[test]
    fn stem_normalises() {
        assert_eq!(stem("RetroArch.exe"), "retroarch");
        assert_eq!(stem("pcsx2-qt"), "pcsx2-qt");
        assert_eq!(stem("  Dolphin.EXE "), "dolphin");
        assert_eq!(stem("melonDS"), "melonds");
        assert_eq!(stem("x64sc"), "x64sc");
    }
}
