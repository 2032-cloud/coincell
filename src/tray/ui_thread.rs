//! Tray icon for platforms (Windows, macOS) where it must be created on the same
//! thread as the event loop. The icon is stashed at construction and turned into
//! a live tray icon by [`Tray::attach`], which the caller runs from the UI
//! thread (inside the eframe creation closure).

use std::cell::RefCell;

use tray_icon::TrayIconBuilder;

/// Owns the tray icon. Created before the event loop starts, activated from
/// within it.
pub struct Tray {
    icon: RefCell<Option<tray_icon::Icon>>,
    handle: RefCell<Option<tray_icon::TrayIcon>>,
}

impl Tray {
    /// Stash the icon until [`Tray::attach`] is called.
    pub fn new(icon: tray_icon::Icon) -> Self {
        Self {
            icon: RefCell::new(Some(icon)),
            handle: RefCell::new(None),
        }
    }

    /// Create the live tray icon. Must be called once from the UI/event-loop
    /// thread; later calls are no-ops.
    pub fn attach(&self) {
        let Some(icon) = self.icon.borrow_mut().take() else {
            return;
        };
        let tray_icon = TrayIconBuilder::new().with_icon(icon).build().expect("Failed to create tray icon");
        self.handle.borrow_mut().replace(tray_icon);
    }
}
