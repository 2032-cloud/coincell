// IPC CONSTANTS
pub const IPC_NAME: &str = "com.p51.coincell"; // IPC Channel Name (auto converted to platform specific paths)
pub const WAKE_WORD: &[u8] = b"WAKE UP"; // IPC Wake Word (keyword sent through channel to wake gui)

// DISPLAY CONSTANTS
pub const ASPECT_RATIO: f32 = 16.0 / 9.0; // Aspect ratio of gui, gui is always half height of display
pub const SCREEN_MARGIN: f32 = 10.0; // Pixel buffer from edge of display

// TRAY CONSTANTS
pub const ICON_BYTES: &[u8] = include_bytes!("../icon.png"); // Raw bytes of base logo embedded into executables
