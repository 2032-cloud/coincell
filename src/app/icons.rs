//! A tiny slice of the [Phosphor](https://phosphoricons.com) icon font.
//!
//! The `Phosphor.ttf` in `assets/` and the codepoints below are vendored from
//! the `egui-phosphor` crate (MIT / Apache-2.0, see `assets/Phosphor-LICENSE-MIT`);
//! the icons themselves are Phosphor Icons, MIT © Helena Zhang / Tobias Fried.
//! Vendored rather than depended on because `egui-phosphor` has no release for
//! eframe/egui 0.36 yet.

use eframe::egui;

const FONT: &[u8] = include_bytes!("../../assets/Phosphor.ttf");

/// Register the icon glyphs as a fallback family on the egui context. Call once
/// at startup. Text keeps using the default font; only the private-use
/// codepoints below resolve to Phosphor.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("phosphor".to_owned(), egui::FontData::from_static(FONT).into());
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("phosphor".to_owned());
    }
    ctx.set_fonts(fonts);
}

// Codepoints (Phosphor "regular" variant). Add more as needed from
// https://phosphoricons.com — the value is the char in the "Regular" weight.
pub const USER: &str = "\u{E4C2}";
pub const SYNC: &str = "\u{E094}"; // arrows-clockwise
pub const ROCKET: &str = "\u{E3FE}"; // rocket-launch
pub const BELL: &str = "\u{E0CE}";
pub const PALETTE: &str = "\u{E6C8}";
pub const DOWNLOAD: &str = "\u{E20C}"; // download-simple
pub const WRENCH: &str = "\u{E5D4}";
pub const SIGN_OUT: &str = "\u{E42A}";
pub const POWER: &str = "\u{E3DA}";
pub const WARNING: &str = "\u{E4E0}";
