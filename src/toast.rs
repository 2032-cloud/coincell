//! OS-toast delivery for `notice::Notice`, via `notify-rust`.
//!
//! [`install`] swaps `notice`'s default `LogSink` for a [`ToastSink`]. Each
//! notice becomes one `notify_rust::Notification`, built and shown on a
//! dedicated worker thread - the `notice::Sink` contract forbids blocking
//! `notice::pump` (which runs on the UI thread), and both backends
//! (zbus/D-Bus on Linux, WinRT on Windows) can block briefly.
//!
//! On Windows the toast carries our registered AppUserModelID
//! (`constants::APP_USER_MODEL_ID`) so it renders as "CoinCell" with our icon;
//! `crate::install::ensure_app_id` writes that registration. On Linux it names
//! the themed `coincell` icon `crate::install` drops under `hicolor`.

use std::sync::mpsc::{self, Sender};

use notify_rust::Notification;

use crate::constants::APP_NAME;
use crate::notice::{self, Notice, Sink};

/// Replace the log-only default sink with real OS toasts. Call once, from `main`.
pub fn install() {
    crate::install::ensure_app_id(); // brand Windows toasts even from a loose run
    notice::set_sink(Box::new(ToastSink::spawn()));
}

struct ToastSink {
    tx: Sender<Notice>,
}

impl ToastSink {
    fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Notice>();
        std::thread::Builder::new()
            .name("toast".into())
            .spawn(move || {
                for notice in rx {
                    if let Err(e) = show(&notice) {
                        tracing::warn!("toast delivery failed: {e:#}");
                    }
                }
            })
            .expect("spawn toast thread");
        Self { tx }
    }
}

impl Sink for ToastSink {
    fn deliver(&self, notice: &Notice) {
        let _ = self.tx.send(notice.clone()); // the worker only ends at shutdown
    }
}

fn show(notice: &Notice) -> anyhow::Result<()> {
    let mut n = Notification::new();
    n.summary(notice.title()).body(&notice.body()).appname(APP_NAME);

    #[cfg(target_os = "linux")]
    n.icon("coincell");
    #[cfg(target_os = "windows")]
    n.app_id(crate::constants::APP_USER_MODEL_ID);

    n.show().map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
}
