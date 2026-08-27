//! System-tray icon management.
//!
//! The mechanics differ per platform: on the BSDs and Linux the icon lives on a
//! dedicated GTK thread, while everywhere else it must be created on the
//! UI/event-loop thread. [`Tray`] hides that split behind [`Tray::new`] followed
//! by [`Tray::attach`] (called from the UI thread).

#[cfg(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd"))]
#[path = "gtk.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd")))]
#[path = "ui_thread.rs"]
mod platform;

use crate::constants::*;
pub use platform::Tray;

/// Decode the embedded PNG into a tray icon.
pub fn load_icon() -> tray_icon::Icon {
    let image = image::load_from_memory(ICON_BYTES).expect("Failed to decode embedded icon").into_rgba8();
    let (width, height) = image.dimensions();
    tray_icon::Icon::from_rgba(image.into_raw(), width, height).expect("Failed to build tray icon")
}
