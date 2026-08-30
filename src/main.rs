#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod api;
mod app;
mod asset;
mod config;
mod constants;
mod install;
mod ipc;
mod logging;
mod notice;
mod placement;
mod store;
mod sync;
mod theme;
mod tray;
mod update;
mod version;

pub use constants::*;

use std::rc::Rc;
use std::time::Duration;

use app::App;
use ipc::Instance;

fn main() -> anyhow::Result<()> {
    // Several deps (reqwest, tungstenite, ureq via egui_extras) pull rustls with
    // different crypto providers, so it can't auto-pick one. Choose ring for the
    // whole process before any TLS happens.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    // `coincell --uninstall [--purge]`, invoked by the Add/Remove Programs entry
    // and the Config button. Runs regardless of a running instance.
    if has("--uninstall") {
        config::Config::init();
        let _g = logging::init();
        return install::uninstall(has("--purge"));
    }

    // A fresh self-update spawned us with this flag; wait out the old process's
    // single-instance lock instead of instantly bailing as a secondary.
    let acquired = if has("--relaunched-after-update") { ipc::acquire_wait(Duration::from_secs(8))? } else { ipc::acquire()? };
    let wake_rx = match acquired {
        Instance::Primary(wake_rx) => wake_rx,
        Instance::Secondary => return Ok(()),
    };

    config::Config::init();
    let _log_guards = logging::init();
    tracing::info!("{} {} ({}) starting", constants::APP_NAME, version::VERSION, version::CHANNEL);
    // Sweep a leftover `<exe>.old` from a previous install-over-running / update.
    install::cleanup_stale();
    store::Store::init();
    // Stamp every device-API request with our version.
    api::set_user_agent(USER_AGENT.as_str());
    // User-facing notifications only log for now. A real OS toast sink plugs in
    // here via `notice::set_sink(..)` once a backend is chosen.

    // The device API's Auth0 parameters. Fetched once from whatever `api_base`
    // the config points at (the default, or a dev/self-host override).
    let api_base = config::Config::get(|c| c.advanced.api_base.clone());
    let device_config = api::fetch_device_config(&api_base).map_err(|e| anyhow::anyhow!("couldn't reach {api_base}: {e}"))?;

    // Presentation (name, palette, links). Non-fatal: fall back to the copy baked
    // in at build time if the endpoint is unreachable or not deployed yet.
    let branding = api::fetch_branding(&api_base).unwrap_or_else(|e| {
        tracing::warn!("branding fetch failed, using baked copy: {e}");
        theme::baked()
    });

    let tray = Rc::new(tray::Tray::new(tray::load_icon()));
    let tray_for_ui = Rc::clone(&tray);

    // Start hidden to the tray? `App` starts in its Hidden state and
    // `reconcile_visibility` hides the OS window on the first frames. (eframe
    // ignores `ViewportBuilder::with_visible` and force-shows after the first
    // paint, so the builder flag below is only a hint.)
    let start_hidden = config::Config::get(|c| c.startup.start_hidden);

    eframe::run_native(
        &format!("CoinCell - {}", branding.identity.name),
        eframe::NativeOptions { viewport: placement::viewport().with_visible(!start_hidden), ..Default::default() },
        Box::new(move |cc| {
            tray_for_ui.attach();
            Ok(Box::new(App::new(cc.egui_ctx.clone(), wake_rx, device_config, branding, start_hidden)))
        }),
    )?;

    Ok(())
}
