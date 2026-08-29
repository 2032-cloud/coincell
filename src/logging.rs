//! `tracing` setup: a daily-rotated file in the data dir, stderr in debug
//! builds, and a Sentry layer that only ships anything when a DSN is compiled
//! in and the user opted into crash reports.
//!
//! The whole app logs through `tracing` macros; nothing prints directly.

use std::path::PathBuf;

use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::config::{Config, LogLevel};
use crate::constants::DATA_DIR;

/// Sentry ingest URL. Empty disables crash/error reporting entirely; otherwise
/// it still only runs when `[advanced].crash_reports` is `true`.
const SENTRY_DSN: &str = "https://c5ced6f27d238f648e205eedbe4fb178@o4511975490846720.ingest.us.sentry.io/4511990732750848";

/// Live for the process lifetime: flushes the log file on drop and keeps the
/// Sentry client running. `main` holds this.
pub struct Guards {
    _file: Option<WorkerGuard>,
    _sentry: Option<sentry::ClientInitGuard>,
}

/// `DATA_DIR/logs`, where the rotated log files live.
pub fn log_dir() -> PathBuf {
    DATA_DIR.join("logs")
}

/// Install the global subscriber and, if opted in, the Sentry client. Reads
/// `[advanced]` from `Config`, so call this after `Config::init`. Changing the
/// log level or the crash-reports toggle takes effect on the next launch.
pub fn init() -> Guards {
    let (log_level, crash_reports) = Config::get(|c| (c.advanced.log_level, c.advanced.crash_reports));

    // Sentry first, so its hub exists before the layer can emit to it.
    let sentry_guard = init_sentry(crash_reports == Some(true));

    // Our crate at the configured level; noisy dependencies capped at WARN.
    let filter = || Targets::new().with_target(env!("CARGO_PKG_NAME"), level_filter(log_level)).with_default(LevelFilter::WARN);

    let (file_writer, file_guard) = match rolling_appender() {
        Ok(appender) => {
            let (nb, guard) = tracing_appender::non_blocking(appender);
            (Some(nb), Some(guard))
        }
        Err(e) => {
            eprintln!("logging: no log file ({e})");
            (None, None)
        }
    };
    let file_layer = file_writer.map(|w| fmt::layer().with_ansi(false).with_writer(w).with_filter(filter()));

    let stderr_layer = cfg!(debug_assertions).then(|| fmt::layer().with_writer(std::io::stderr).with_filter(filter()));

    tracing_subscriber::registry().with(file_layer).with(stderr_layer).with(sentry::integrations::tracing::layer().with_filter(filter())).init();

    // Log panics before whatever hook was already set (Sentry's, or the default).
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        prev(info);
    }));

    Guards { _file: file_guard, _sentry: sentry_guard }
}

fn rolling_appender() -> Result<RollingFileAppender, tracing_appender::rolling::InitError> {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    RollingFileAppender::builder().rotation(Rotation::DAILY).filename_prefix("coincell").filename_suffix("log").max_log_files(7).build(dir)
}

fn init_sentry(opted_in: bool) -> Option<sentry::ClientInitGuard> {
    if SENTRY_DSN.is_empty() || !opted_in {
        return None;
    }
    let mut options = sentry::ClientOptions::default();
    // TODO: use the resolved build version + channel once build.rs versioning lands.
    options.release = Some(env!("CARGO_PKG_VERSION").into());
    options.send_default_pii = false;
    Some(sentry::init((SENTRY_DSN, options)))
}

fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}
