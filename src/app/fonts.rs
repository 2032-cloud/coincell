//! The app's font stack, installed once at startup.
//!
//! egui's built-in family covers Latin, Greek and Cyrillic. On top of it we
//! stack fallback fonts, consulted only for glyphs the primary lacks:
//!
//! - **Phosphor**, for the private-use icon codepoints in [`super::icons`].
//!   `assets/Phosphor.ttf` is vendored from the `egui-phosphor` crate
//!   (MIT / Apache-2.0, `assets/Phosphor-LICENSE-MIT`); the icons are Phosphor
//!   Icons, MIT (c) Helena Zhang / Tobias Fried. Vendored because `egui-phosphor`
//!   has no eframe/egui 0.36 release yet.
//! - **Noto Sans CJK** (`assets/NotoSansCJKjp-Regular.otf`, SIL OFL 1.1), for
//!   Japanese / Korean / Chinese game titles that would otherwise render as tofu
//!   boxes. The `jp` regional build already contains every JP + KR + ZH glyph;
//!   the other regional builds (`kr`, `sc`, `tc`, `hk`) carry the same coverage
//!   and only differ in the default shape of locale-specific Han characters, so
//!   there's no point chaining them: egui's fallback stops at the first font
//!   with a glyph for the codepoint, which is always this one.

use eframe::egui;

const PHOSPHOR: &[u8] = include_bytes!("../../assets/Phosphor.ttf");
const CJK: &[u8] = include_bytes!("../../assets/NotoSansCJKjp-Regular.otf");

/// Register the fallback fonts on `ctx`. Call once at startup, before anything
/// draws.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    add_fallback(&mut fonts, "phosphor", PHOSPHOR);
    add_fallback(&mut fonts, "cjk", CJK);
    ctx.set_fonts(fonts);
}

/// Register `bytes` under `name` and append it to both families as the last
/// fallback, so the primary font still wins wherever it has the glyph.
fn add_fallback(fonts: &mut egui::FontDefinitions, name: &str, bytes: &'static [u8]) {
    fonts.font_data.insert(name.to_owned(), egui::FontData::from_static(bytes).into());
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push(name.to_owned());
    }
}
