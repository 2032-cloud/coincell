//! Window geometry

use std::sync::Arc;

use crate::constants::*;
use eframe::egui::{IconData, ViewportBuilder};

/// The embedded app icon, for the window / taskbar of the *running* app. (The
/// `.exe` file icon shown when it's not running is a build-time resource, see
/// `build.rs`.)
fn app_icon() -> IconData {
    match image::load_from_memory(ICON_BYTES) {
        Ok(img) => {
            let img = img.to_rgba8();
            let (width, height) = img.dimensions();
            IconData { rgba: img.into_raw(), width, height }
        }
        Err(_) => IconData::default(),
    }
}

pub fn viewport() -> ViewportBuilder {
    let displays = display_info::DisplayInfo::all().expect("Could not retrieve display information");
    let display = displays.iter().find(|display| display.is_primary).or_else(|| displays.first()).expect("No displays detected");

    let display_width = display.width as f32;
    let display_height = display.height as f32;
    let display_x = display.x as f32;
    let display_y = display.y as f32;

    let app_height = display_height / 2.0;
    let app_width = app_height * (1.0 / ASPECT_RATIO);

    let app_position: (f32, f32) = (display_width - SCREEN_MARGIN - app_width + display_x, display_height - SCREEN_MARGIN - app_height + display_y);

    ViewportBuilder::default().with_resizable(false).with_position(app_position).with_inner_size((app_width, app_height)).with_always_on_top().with_decorations(false).with_icon(Arc::new(app_icon()))
}
