//! Tray icon for GTK-based platforms: the icon is owned by a dedicated GTK thread that runs for the lifetime of the process.

use tray_icon::TrayIconBuilder;
use tray_icon::menu::Menu;

pub struct Tray;

impl Tray {
    pub fn new(icon: tray_icon::Icon) -> Self {
        std::thread::spawn(move || {
            gtk::init().unwrap();
            let _tray_icon = TrayIconBuilder::new().with_menu(Box::new(Menu::new())).with_icon(icon).build().unwrap();
            gtk::main();
        });
        Self
    }

    pub fn attach(&self) {}
}
