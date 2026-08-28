#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod api;
mod app;
mod config;
mod constants;
mod ipc;
mod placement;
mod store;
mod theme;
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
    store::Store::init();

    // The device API's Auth0 parameters. Fetched once from whatever `api_base`
    // the config points at (the default, or a dev/self-host override).
    let api_base = config::Config::get(|c| c.advanced.api_base.clone());
    let device_config = api::fetch_device_config(&api_base).map_err(|e| anyhow::anyhow!("couldn't reach {api_base}: {e}"))?;

    // Presentation (name, palette, links). Non-fatal: fall back to the copy baked
    // in at build time if the endpoint is unreachable or not deployed yet.
    let branding = api::fetch_branding(&api_base).unwrap_or_else(|e| {
        eprintln!("branding: using the baked-in copy ({e})");
        theme::baked()
    });

    let tray = Rc::new(tray::Tray::new(tray::load_icon()));
    let tray_for_ui = Rc::clone(&tray);

    eframe::run_native(
        "My egui App",
        eframe::NativeOptions { viewport: placement::viewport(), ..Default::default() },
        Box::new(move |cc| {
            tray_for_ui.attach();
            Ok(Box::new(App::new(cc.egui_ctx.clone(), wake_rx, device_config, branding)))
        }),
    )?;

    Ok(())
}
