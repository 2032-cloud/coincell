#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod auth;
mod config;
mod constants;
mod ipc;
mod placement;
mod tray;

pub use constants::*;

use std::rc::Rc;

use app::App;
use ipc::Instance;

fn main() -> anyhow::Result<()> {
    let wake_rx = match ipc::acquire()? {
        Instance::Primary(wake_rx) => wake_rx,
        Instance::Secondary => return Ok(()),
    };

    config::Config::init();
    config::OauthSpec::init();

    let tray = Rc::new(tray::Tray::new(tray::load_icon()));
    let tray_for_ui = Rc::clone(&tray);

    eframe::run_native(
        "My egui App",
        eframe::NativeOptions {
            viewport: placement::viewport(),
            ..Default::default()
        },
        Box::new(move |cc| {
            tray_for_ui.attach();
            Ok(Box::new(App::new(cc.egui_ctx.clone(), wake_rx)))
        }),
    )?;

    Ok(())
}
