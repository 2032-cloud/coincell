use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use lazy_static::lazy_static;

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
    /// Snapshots of local save files taken right before the engine overwrote
    /// them with bytes the user had never uploaded. Kept next to `data.sqlite`
    /// (which indexes them) so the two travel together for recovery.
    pub static ref BACKUP_DIR: PathBuf = DATA_DIR.join("save-backups");

    // IDENTITY: resolved once at startup. Empty string means "couldn't tell".
    pub static ref USERNAME: String = whoami::username().unwrap_or_default();
    pub static ref DEVICE_NAME: String = whoami::devicename().unwrap_or_default();
    pub static ref PLATFORM: String = whoami::platform().to_string();
    /// The session name this client reports at bootstrap, e.g.
    /// `CoinCell - ethan@tower - Windows`. Degrades a piece at a time rather than
    /// showing a placeholder: no hostname -> `CoinCell - ethan - Windows`,
    /// no username either -> `CoinCell - Windows`.
    pub static ref CLIENT_NAME: String = build_client_name();
}

fn build_client_name() -> String {
    let (user, host) = (USERNAME.as_str(), DEVICE_NAME.as_str());
    let who = match (user.is_empty(), host.is_empty()) {
        (false, false) => format!("{user}@{host}"),
        (false, true) => user.to_owned(),
        (true, false) => host.to_owned(),
        (true, true) => String::new(),
    };
    if who.is_empty() { format!("{APP_NAME} - {}", PLATFORM.as_str()) } else { format!("{APP_NAME} - {who} - {}", PLATFORM.as_str()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_name_is_well_formed() {
        assert!(CLIENT_NAME.starts_with(&format!("{APP_NAME} - ")), "{}", *CLIENT_NAME);
        assert!(CLIENT_NAME.ends_with(PLATFORM.as_str()), "{}", *CLIENT_NAME);
        assert!(!PLATFORM.is_empty());
    }
}
