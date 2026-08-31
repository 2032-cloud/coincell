//! The user-facing notification queue.
//!
//! Anything in the app can `notice::post(Notice::..)` from any thread. `App`
//! calls [`pump`] once a frame to drain the queue to a [`Sink`]. `post` gates on
//! the `[notifications]` config (master switch plus one flag per kind) and
//! collapses repeats: the same notice inside [`DEDUP_WINDOW`] is dropped, so a
//! burst of pulls or a flapping conflict is one line, not ten.
//!
//! Delivery goes through a [`Sink`]. The default is [`LogSink`] (a `tracing`
//! line); `main` swaps in `crate::toast::ToastSink` (real OS toasts, via
//! notify-rust) through [`set_sink`] with no change to any call site.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::config::{Config, Notifications};

/// Repeats of the same notice inside this window are dropped.
const DEDUP_WINDOW: Duration = Duration::from_secs(10);

/// One user-facing event. One variant per `[notifications]` toggle; the payload
/// is whatever the text needs (game names are resolved before posting, never
/// raw ids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A newer save was pulled from the server and written to disk.
    Pulled { game: String },
    /// A save diverged on both sides; the user has to pick.
    Conflict { game: String },
    /// One instance's sync is wedged (a held-back pull, or an upload that keeps
    /// failing). Posted from `App::drain_sync` on `EngineEvent::Stuck`; `detail`
    /// is the full user-facing sentence, game name included.
    Error { detail: String },
    /// The session died mid sync and the app signed out.
    SessionExpired,
    /// An automatic update check found a newer release (`on_update = notify`).
    UpdateReady { version: String },
    /// A canned notice fired by the Config "Send a test notification" button.
    /// Delivered straight to the sink by [`send_test`], past the gate and dedup.
    Test,
}

impl Notice {
    /// Fixed heading for the notice.
    pub fn title(&self) -> &'static str {
        match self {
            Notice::Pulled { .. } => "Save updated",
            Notice::Conflict { .. } => "Sync conflict",
            Notice::Error { .. } => "Sync problem",
            Notice::SessionExpired => "Signed out",
            Notice::UpdateReady { .. } => "Update available",
            Notice::Test => "CoinCell",
        }
    }

    /// Body line.
    pub fn body(&self) -> String {
        match self {
            Notice::Pulled { game } => format!("{game} picked up a newer save from another device."),
            Notice::Conflict { game } => format!("{game} changed here and on another device. Open coincell to choose which to keep."),
            Notice::Error { detail } => detail.clone(),
            Notice::SessionExpired => "Your session expired. Sign in again to keep syncing.".to_owned(),
            Notice::UpdateReady { version } => format!("CoinCell {version} is ready to install (Config \u{203a} Updates)."),
            Notice::Test => "Notifications are working.".to_owned(),
        }
    }

    /// Key for the dedup window: same key inside [`DEDUP_WINDOW`] is one notice.
    fn dedup_key(&self) -> String {
        match self {
            Notice::Pulled { game } => format!("pull:{game}"),
            Notice::Conflict { game } => format!("conflict:{game}"),
            Notice::Error { detail } => format!("error:{detail}"),
            Notice::SessionExpired => "session-expired".to_owned(),
            Notice::UpdateReady { version } => format!("update:{version}"),
            Notice::Test => "test".to_owned(),
        }
    }

    /// `[notifications]` gate: master switch, and the matching per-kind flag for
    /// the sync notices. `UpdateReady` has no per-kind flag - `[updates].on_update
    /// = notify` is already that opt-in - so it rides the master switch alone.
    fn allowed_by(&self, n: &Notifications) -> bool {
        n.enabled
            && match self {
                Notice::Pulled { .. } => n.on_pull,
                Notice::Conflict { .. } => n.on_conflict,
                Notice::Error { .. } => n.on_error,
                Notice::SessionExpired => n.on_session_expired,
                Notice::UpdateReady { .. } => true,
                Notice::Test => true,
            }
    }
}

/// Where accepted notices go. Implementations must not block (spawn a thread if
/// the platform call is slow); [`pump`] calls this on the UI thread.
pub trait Sink: Send + Sync {
    fn deliver(&self, notice: &Notice);
}

/// The default sink until an OS backend is chosen: a log line.
struct LogSink;

impl Sink for LogSink {
    fn deliver(&self, notice: &Notice) {
        tracing::info!("notify: {} | {}", notice.title(), notice.body());
    }
}

static LOG_SINK: LogSink = LogSink;
static SINK: OnceLock<Box<dyn Sink>> = OnceLock::new();
static QUEUE: OnceLock<Mutex<Queue>> = OnceLock::new();

#[derive(Default)]
struct Queue {
    pending: Vec<Notice>,
    /// dedup key -> when it was last accepted.
    recent: HashMap<String, Instant>,
}

impl Queue {
    /// Apply the config gate and the dedup window; on success push to `pending`.
    /// Split out from [`post`] so the policy is testable without global state.
    fn admit(&mut self, notice: Notice, now: Instant, cfg: &Notifications) -> bool {
        if !notice.allowed_by(cfg) {
            return false;
        }
        let key = notice.dedup_key();
        if let Some(&last) = self.recent.get(&key)
            && now.duration_since(last) < DEDUP_WINDOW
        {
            return false;
        }
        self.recent.insert(key, now);
        self.recent.retain(|_, t| now.duration_since(*t) < DEDUP_WINDOW);
        self.pending.push(notice);
        true
    }
}

fn queue() -> &'static Mutex<Queue> {
    QUEUE.get_or_init(|| Mutex::new(Queue::default()))
}

fn sink() -> &'static dyn Sink {
    match SINK.get() {
        Some(s) => s.as_ref(),
        None => &LOG_SINK,
    }
}

/// Install the real delivery backend. Call once, from `main`, before the UI
/// starts. A second call is ignored with a warning.
pub fn set_sink(backend: Box<dyn Sink>) {
    if SINK.set(backend).is_err() {
        tracing::warn!("notice sink already set");
    }
}

/// Deliver a canned notice straight to the sink, past the config gate and the
/// dedup window. Backs the Config > Notifications "Send a test notification"
/// button: confirms OS delivery independent of the per-kind toggles.
pub fn send_test() {
    sink().deliver(&Notice::Test);
}

/// Queue a notice. Cheap and thread safe: gates on `[notifications]`, dedupes,
/// and returns. Delivery happens in [`pump`].
pub fn post(notice: Notice) {
    let cfg = Config::get(|c| c.notifications.clone());
    let mut q = queue().lock().unwrap_or_else(|e| e.into_inner());
    q.admit(notice, Instant::now(), &cfg);
}

/// Deliver everything queued since the last call. `App::logic` runs this once a
/// frame.
pub fn pump() {
    let due = {
        let mut q = queue().lock().unwrap_or_else(|e| e.into_inner());
        if q.pending.is_empty() {
            return;
        }
        std::mem::take(&mut q.pending)
    };
    let sink = sink();
    for notice in &due {
        sink.deliver(notice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_on(enabled: bool) -> Notifications {
        Notifications { enabled, on_pull: true, on_conflict: true, on_error: true, on_session_expired: true }
    }

    #[test]
    fn test_notice_is_self_describing() {
        assert_eq!(Notice::Test.title(), "CoinCell");
        assert!(!Notice::Test.body().is_empty());
    }

    #[test]
    fn master_switch_gates_every_kind() {
        let mut q = Queue::default();
        assert!(!q.admit(Notice::SessionExpired, Instant::now(), &all_on(false)));
        assert!(q.pending.is_empty());
    }

    #[test]
    fn per_kind_flag_is_respected() {
        let mut q = Queue::default();
        let mut cfg = all_on(true);
        cfg.on_conflict = false;
        let now = Instant::now();
        assert!(!q.admit(Notice::Conflict { game: "Mother 3".into() }, now, &cfg));
        assert!(q.admit(Notice::Pulled { game: "Mother 3".into() }, now, &cfg));
        assert_eq!(q.pending.len(), 1);
    }

    #[test]
    fn identical_notice_is_deduped_inside_the_window() {
        let mut q = Queue::default();
        let cfg = all_on(true);
        let t0 = Instant::now();
        assert!(q.admit(Notice::Conflict { game: "Mother 3".into() }, t0, &cfg));
        // same game, still inside the window: dropped
        assert!(!q.admit(Notice::Conflict { game: "Mother 3".into() }, t0 + Duration::from_secs(3), &cfg));
        // a different game is its own line
        assert!(q.admit(Notice::Conflict { game: "Zelda".into() }, t0 + Duration::from_secs(3), &cfg));
        // past the window it can repeat
        assert!(q.admit(Notice::Conflict { game: "Mother 3".into() }, t0 + DEDUP_WINDOW + Duration::from_secs(1), &cfg));
        assert_eq!(q.pending.len(), 3);
    }
}
