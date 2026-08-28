use std::path::Path;

use directories::ProjectDirs;
use lazy_static::lazy_static;
use reqwest::blocking::Client;

// MISC CONSTANTS
pub const APP_NAME: &str = "CoinCell";
pub const COMPANY_NAME: &str = "p51";
pub const API_BASE_ROUTE: &str = "https://cr.2032.cloud";

// IPC CONSTANTS
pub const IPC_NAME: &str = "com.p51.coincell"; // IPC Channel Name (auto converted to platform specific paths)
pub const WAKE_WORD: &[u8] = b"WAKE UP"; // IPC Wake Word (keyword sent through channel to wake gui)

// DISPLAY CONSTANTS
pub const ASPECT_RATIO: f32 = 16.0 / 9.0; // Aspect ratio of gui, gui is always half height of display
pub const SCREEN_MARGIN: f32 = 10.0; // Pixel buffer from edge of display

// TRAY CONSTANTS
pub const ICON_BYTES: &[u8] = include_bytes!("../icon_128_128.png"); // Raw bytes of base logo embedded into executables

lazy_static! {
    pub static ref PROJECT_DIRS: ProjectDirs = directories::ProjectDirs::from("com", COMPANY_NAME, APP_NAME).expect("Unable to calculate project dirs");
    pub static ref CONFIG_DIR: &'static Path = PROJECT_DIRS.config_dir();
    pub static ref DATA_DIR: &'static Path = PROJECT_DIRS.data_dir();
    pub static ref API_CLIENT: Client = Client::new();
}
