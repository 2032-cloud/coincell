//! Tray icon for GTK-based platforms: the icon is owned by a dedicated GTK
//! thread that runs for the lifetime of the process.

use tray_icon::TrayIconBuilder;
use tray_icon::menu::Menu;

/// Handle to the GTK thread hosting the tray icon. Zero-sized; the thread keeps
/// itself and the icon alive.
pub struct Tray;

impl Tray {
    /// Spawn the GTK thread and create the tray icon on it.
    pub fn new(icon: tray_icon::Icon) -> Self {
        std::thread::spawn(move || {
            gtk::init().unwrap();
            let _tray_icon = TrayIconBuilder::new()
                .with_menu(Box::new(Menu::new()))
                .with_icon(icon)
                .build()
                .unwrap();
            gtk::main();
        });
        Self
    }

    /// No-op on this platform — the icon is created in [`Tray::new`].
    pub fn attach(&self) {}
}
