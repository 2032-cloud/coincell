//! Tray icon for platforms (Windows, macOS) where it must be created on the same thread as the event loop.

use std::cell::RefCell;

use tray_icon::TrayIconBuilder;

pub struct Tray {
    icon: RefCell<Option<tray_icon::Icon>>,
    handle: RefCell<Option<tray_icon::TrayIcon>>,
}

impl Tray {
    pub fn new(icon: tray_icon::Icon) -> Self {
        Self { icon: RefCell::new(Some(icon)), handle: RefCell::new(None) }
    }

    pub fn attach(&self) {
        let Some(icon) = self.icon.borrow_mut().take() else {
            return;
        };
        let tray_icon = TrayIconBuilder::new().with_icon(icon).build().expect("Failed to create tray icon");
        self.handle.borrow_mut().replace(tray_icon);
    }
}
