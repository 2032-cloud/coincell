use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use lazy_static::lazy_static;

// MISC CONSTANTS
pub const APP_NAME: &str = "CoinCell";
pub const COMPANY_NAME: &str = "p51";
pub const API_BASE_ROUTE: &str = "https://cr.2032.cloud";

/// `ProjectDirs` application component. Debug builds live in a parallel
/// `CoinCell-dbg` tree (config / database / logs / cache / save backups all
/// derive from `PROJECT_DIRS`), so local testing never touches the real
/// install. [`seed_debug_dirs`] clones the real `config.toml` + `data.sqlite`
/// across on the first debug run.
#[cfg(not(debug_assertions))]
const DIRS_APP: &str = APP_NAME;
#[cfg(debug_assertions)]
const DIRS_APP: &str = "CoinCell-dbg";

// IPC CONSTANTS
// Debug builds use a distinct channel so a dev build can run alongside the
// installed one instead of bailing as a secondary instance.
#[cfg(not(debug_assertions))]
pub const IPC_NAME: &str = "com.p51.coincell"; // IPC Channel Name (auto converted to platform specific paths)
#[cfg(debug_assertions)]
pub const IPC_NAME: &str = "com.p51.coincell.dbg";

/// Windows AppUserModelID: what Windows attributes our toast notifications to.
/// Must stay stable across versions and match the value registered under
/// `HKCU\Software\Classes\AppUserModelId\`. Mirrors the `ProjectDirs`
/// qualifier/org/app.
#[cfg(windows)]
pub const APP_USER_MODEL_ID: &str = "com.p51.CoinCell";
pub const WAKE_WORD: &[u8] = b"WAKE UP"; // IPC Wake Word (keyword sent through channel to wake gui)

// DISPLAY CONSTANTS
pub const ASPECT_RATIO: f32 = 16.0 / 9.0; // Aspect ratio of gui, gui is always half height of display
pub const SCREEN_MARGIN: f32 = 10.0; // Pixel buffer from edge of display

// TRAY CONSTANTS
pub const ICON_BYTES: &[u8] = include_bytes!("../assets/icon_128_128.png"); // Raw bytes of base logo embedded into executables

lazy_static! {
    pub static ref PROJECT_DIRS: ProjectDirs = directories::ProjectDirs::from("com", COMPANY_NAME, DIRS_APP).expect("Unable to calculate project dirs");
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

    /// `User-Agent` on every device-API request, e.g.
    /// `CoinCell/0.2.0 (stable; windows)`. Version + channel resolved at build
    /// time (see `crate::version`).
    pub static ref USER_AGENT: String = format!("{APP_NAME}/{} ({}; {})", crate::version::VERSION, crate::version::CHANNEL, std::env::consts::OS);
}

/// Release: nothing to do.
#[cfg(not(debug_assertions))]
pub fn seed_debug_dirs() {}

/// Debug: on the first run, copy the real install's `config.toml` and
/// `data.sqlite` (+ `-wal`) into the `CoinCell-dbg` tree so testing starts from
/// a realistic state. Only ever *reads* the real files. Runs before
/// `Config::init` / `Store::init` / logging, so it reports via `eprintln!`.
#[cfg(debug_assertions)]
pub fn seed_debug_dirs() {
    let Some(prod) = ProjectDirs::from("com", COMPANY_NAME, APP_NAME) else { return };

    let seed = |from: PathBuf, to: PathBuf| {
        if to.exists() || !from.exists() {
            return;
        }
        if let Some(dir) = to.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::copy(&from, &to) {
            Ok(_) => eprintln!("debug: seeded {} from the real install", to.display()),
            Err(e) => eprintln!("debug: couldn't seed {}: {e}", to.display()),
        }
    };

    seed(prod.config_dir().join("config.toml"), CONFIG_DIR.join("config.toml"));
    for name in ["data.sqlite", "data.sqlite-wal"] {
        seed(prod.data_dir().join(name), DATA_DIR.join(name));
    }
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
