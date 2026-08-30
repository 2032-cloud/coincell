use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;

use crate::api::{Branding, GameInstance};
use crate::app::mapping::human_size;
use crate::app::{icons, open_path};
use crate::config::{Config, ConflictPolicy, LogLevel, PollInterval, Theme, UpdateAction, UpdateChannel, UploadTrigger};
use crate::constants::BACKUP_DIR;
use crate::store::Store;
use crate::sync::humanize_since;
use crate::theme::homepage_path;

/// What the config screen wants the parent `App` to do after this frame.
pub enum ConfigOutcome {
    /// Nothing to do; stay in the config window.
    Stay,
    /// "Sync now" pressed, poke the sync engine.
    SyncNow,
    /// Restore a local pre-overwrite backup from the Backups section (only sent
    /// for an instance that's currently mapped).
    RestoreBackup { instance_id: String, content_hash: String },
    /// User confirmed logout: revoke + clear the session, return to sign-in.
    LogOut,
    /// User confirmed quit: shut the daemon down entirely.
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Account,
    Sync,
    Startup,
    Notifications,
    Appearance,
    Updates,
    Backups,
    Advanced,
    // Reachable only through a footer action, never the rail. There is always
    // exactly one selected section; there is no "nothing selected" state.
    ConfirmLogout,
    ConfirmQuit,
}

impl Section {
    const DEFAULT: Section = Section::Account;
    const RAIL: [Section; 8] = [Section::Account, Section::Sync, Section::Startup, Section::Notifications, Section::Appearance, Section::Updates, Section::Backups, Section::Advanced];

    fn title(self) -> &'static str {
        match self {
            Section::Account => "Account",
            Section::Sync => "Sync",
            Section::Startup => "Startup",
            Section::Notifications => "Notifications",
            Section::Appearance => "Appearance",
            Section::Updates => "Updates",
            Section::Backups => "Save backups",
            Section::Advanced => "Advanced",
            Section::ConfirmLogout => "Log out",
            Section::ConfirmQuit => "Quit CoinCell",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Section::Account => icons::USER,
            Section::Sync => icons::SYNC,
            Section::Startup => icons::ROCKET,
            Section::Notifications => icons::BELL,
            Section::Appearance => icons::PALETTE,
            Section::Updates => icons::DOWNLOAD,
            Section::Backups => icons::RESTORE,
            Section::Advanced => icons::WRENCH,
            Section::ConfirmLogout => icons::SIGN_OUT,
            Section::ConfirmQuit => icons::POWER,
        }
    }
}

#[derive(Default)]
struct Drafts {
    api_base: String,
}

/// A pending inline confirm in the Backups section, keyed to a `save_backups.id`.
enum BackupConfirm {
    Restore(i64),
    Delete(i64),
}

/// Short gloss for a `save_backups.reason`.
fn reason_gloss(reason: &str) -> &'static str {
    match reason {
        "pull" => "before a download",
        "conflict" => "before a conflict resolve",
        "restore" => "before a restore",
        _ => "before an overwrite",
    }
}

pub struct ConfigApp {
    section: Section,
    drafts: Drafts,
    error: Option<String>,
    backup_confirm: Option<BackupConfirm>,
}

impl ConfigApp {
    pub fn new() -> Self {
        let mut app = Self { section: Section::DEFAULT, drafts: Drafts::default(), error: None, backup_confirm: None };
        app.reset();
        app
    }

    /// Return to a clean default state. Called each time the window is shown as
    /// Config (and after a logout).
    pub fn reset(&mut self) {
        self.section = Section::DEFAULT;
        self.error = None;
        self.backup_confirm = None;
        self.drafts.api_base = Config::get(|c| c.advanced.api_base.to_string());
    }

    pub fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {}

    pub fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame, branding: &Branding, catalog: &[GameInstance]) -> ConfigOutcome {
        let mut outcome = ConfigOutcome::Stay;

        egui::Panel::left("config_rail").resizable(false).exact_size(48.0).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                for section in Section::RAIL {
                    if rail_button(ui, section, self.section == section).clicked() {
                        self.select(section);
                    }
                }
            });
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                if rail_button(ui, Section::ConfirmQuit, self.section == Section::ConfirmQuit).clicked() {
                    self.select(Section::ConfirmQuit);
                }
                if rail_button(ui, Section::ConfirmLogout, self.section == Section::ConfirmLogout).clicked() {
                    self.select(Section::ConfirmLogout);
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(self.section.icon()).size(18.0));
                ui.heading(self.section.title());
            });
            ui.separator();

            if let Some(err) = self.error.clone() {
                ui.colored_label(ui.visuals().error_fg_color, err);
                ui.add_space(4.0);
            }

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(4.0);
                match self.section {
                    Section::Account => self.account(ui, branding),
                    Section::Sync => {
                        if self.sync(ui) {
                            outcome = ConfigOutcome::SyncNow;
                        }
                    }
                    Section::Startup => self.startup(ui),
                    Section::Notifications => self.notifications(ui),
                    Section::Appearance => self.appearance(ui),
                    Section::Updates => self.updates(ui),
                    Section::Backups => {
                        if let Some(o) = self.backups(ui, catalog) {
                            outcome = o;
                        }
                    }
                    Section::Advanced => self.advanced(ui),
                    Section::ConfirmLogout => match confirm(ui, "Log out on this device? Syncing stops until you sign in again.", "Log out") {
                        Some(true) => outcome = ConfigOutcome::LogOut,
                        Some(false) => self.select(Section::DEFAULT),
                        None => {}
                    },
                    Section::ConfirmQuit => match confirm(ui, "Quit CoinCell entirely? The daemon stops and nothing syncs until you start it again.", "Quit") {
                        Some(true) => outcome = ConfigOutcome::Quit,
                        Some(false) => self.select(Section::DEFAULT),
                        None => {}
                    },
                }
            });
        });

        outcome
    }

    fn select(&mut self, section: Section) {
        self.section = section;
        self.error = None;
        self.backup_confirm = None;
    }

    fn note_save(&mut self, r: anyhow::Result<()>) {
        self.error = r.err().map(|e| format!("Couldn't save: {e:#}"));
    }

    // ---- sections -------------------------------------------------------------

    fn account(&mut self, ui: &mut egui::Ui, branding: &Branding) {
        match Config::get(|c| c.session_id()) {
            Some(id) => {
                ui.label("Signed in on this device.");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Session");
                    let short: String = id.chars().take(8).collect();
                    ui.monospace(format!("{short}…"));
                });
            }
            None => {
                ui.label("Not signed in.");
            }
        }
        ui.add_space(12.0);
        ui.hyperlink_to("Manage account & sessions ↗", homepage_path(branding, "settings"));
        ui.add_space(4.0);
        ui.hyperlink_to("Documentation ↗", &branding.identity.docs_url);
        ui.add_space(8.0);
        ui.small(format!("Renaming or revoking other devices, theme, retention and account deletion all live on {}.", branding.identity.name));
        ui.add_space(6.0);
        ui.small("Use the Log out button at the bottom of the rail to sign this device out.");

        if !branding.identity.attribution_text.is_empty() {
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.small(&branding.identity.attribution_text);
                ui.hyperlink_to(egui::RichText::new(&branding.identity.homepage_url).small(), &branding.identity.homepage_url);
            });
        }
    }

    /// Returns `true` if "Sync now" was pressed this frame.
    fn sync(&mut self, ui: &mut egui::Ui) -> bool {
        let mut s = Config::get(|c| c.sync.clone());
        let mut changed = false;

        changed |= ui.checkbox(&mut s.enabled, "Sync enabled").changed();
        ui.add_space(4.0);

        ui.add_enabled_ui(s.enabled, |ui| {
            egui::Grid::new("sync_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Check for changes");
                changed |= combo(ui, "sync_poll", &mut s.poll, POLL_PRESETS);
                ui.end_row();

                ui.label("Upload when");
                changed |= combo(ui, "sync_trigger", &mut s.upload_trigger, UPLOAD_TRIGGERS);
                ui.end_row();

                ui.label("On conflict");
                changed |= combo(ui, "sync_conflict", &mut s.conflict, CONFLICTS);
                ui.end_row();
            });
            ui.add_space(4.0);
            changed |= ui.checkbox(&mut s.pause_on_metered, "Pause on a metered connection").changed();
        });

        if changed {
            let r = Config::update(|c| c.sync = s.clone());
            self.note_save(r);
        }

        ui.add_space(10.0);
        let sync_now = ui.add_enabled(s.enabled, egui::Button::new("Sync now")).on_hover_text("Check the server for changes right now").clicked();
        ui.add_space(4.0);
        ui.small("The engine also runs on the realtime stream and the poll interval above. Upload direction and per-game controls arrive with the Home window.");
        sync_now
    }

    fn startup(&mut self, ui: &mut egui::Ui) {
        let mut s = Config::get(|c| c.startup.clone());
        let mut changed = false;
        changed |= ui.checkbox(&mut s.launch_on_login, "Launch CoinCell at login").changed();
        changed |= ui.checkbox(&mut s.start_hidden, "Start hidden in the tray").changed();
        if changed {
            let r = Config::update(|c| c.startup = s);
            self.note_save(r);
        }
        ui.small("OS auto-start registration isn't wired up yet, this records the preference only.");

        ui.add_space(10.0);

        let mut hide_on_blur = Config::get(|c| c.window.hide_on_focus_loss);
        if ui.checkbox(&mut hide_on_blur, "Hide the window when it loses focus").changed() {
            let r = Config::update(|c| c.window.hide_on_focus_loss = hide_on_blur);
            self.note_save(r);
        }
        ui.small("The minimise button, Esc, and clicking the tray icon hide it regardless.");
    }

    fn notifications(&mut self, ui: &mut egui::Ui) {
        let mut n = Config::get(|c| c.notifications.clone());
        let mut changed = false;
        changed |= ui.checkbox(&mut n.enabled, "Show notifications").changed();
        ui.add_enabled_ui(n.enabled, |ui| {
            ui.indent("notif_kinds", |ui| {
                changed |= ui.checkbox(&mut n.on_pull, "A newer save was pulled down").changed();
                changed |= ui.checkbox(&mut n.on_conflict, "A save conflicted").changed();
                changed |= ui.checkbox(&mut n.on_error, "Sync errors").changed();
                changed |= ui.checkbox(&mut n.on_session_expired, "The session expired").changed();
            });
        });
        if changed {
            let r = Config::update(|c| c.notifications = n);
            self.note_save(r);
        }
    }

    fn appearance(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("appearance_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            let mut a = Config::get(|c| c.appearance.clone());
            ui.label("Theme");
            if combo(ui, "theme", &mut a.theme, THEMES) {
                let r = Config::update(|c| c.appearance = a);
                self.note_save(r);
            }
            ui.end_row();

            let mut scale = Config::get(|c| c.window.ui_scale);
            ui.label("UI scale");
            if scale_combo(ui, &mut scale) {
                let r = Config::update(|c| c.window.ui_scale = scale);
                self.note_save(r);
            }
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.small("Applies immediately. “Follow account” uses the theme set on the website; “Follow system” tracks your OS. Colours come from the service's branding.");
    }

    fn updates(&mut self, ui: &mut egui::Ui) {
        let mut u = Config::get(|c| c.updates.clone());
        let mut changed = false;

        ui.horizontal(|ui| {
            ui.label("Current version");
            ui.monospace(crate::version::VERSION);
        });
        if !crate::version::is_release() {
            ui.small("Development build, auto-update is disabled. Channel and actions below apply to release builds.");
        }
        ui.add_space(6.0);

        egui::Grid::new("updates_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Channel");
            changed |= combo(ui, "upd_channel", &mut u.channel, CHANNELS);
            ui.end_row();

            ui.label("When available");
            changed |= combo(ui, "upd_action", &mut u.on_update, UPDATE_ACTIONS);
            ui.end_row();
        });
        ui.add_space(4.0);
        changed |= ui.checkbox(&mut u.auto_check, "Check automatically").changed();

        if changed {
            let r = Config::update(|c| c.updates = u);
            self.note_save(r);
        }
        ui.add_space(6.0);
        ui.small("The self-updater isn't built yet; these settings are stored for when it is.");
    }

    /// Every pre-overwrite local snapshot the engine has kept, newest first. This
    /// is the catch-all, and the only place backups for an unmapped or deleted
    /// game are reachable. Per-game history (with server saves too) lives on the
    /// game's own page in Home.
    fn backups(&mut self, ui: &mut egui::Ui, catalog: &[GameInstance]) -> Option<ConfigOutcome> {
        let rows = match Store::get(|s| s.backups()) {
            Ok(rows) => rows,
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("Couldn't read the backups: {e:#}"));
                return None;
            }
        };

        ui.small("Snapshots the engine takes before a sync would overwrite unsaved local changes. The bytes live next to the database; restoring one writes it back to disk and re-uploads it as the newest save.");
        ui.add_space(8.0);

        if rows.is_empty() {
            ui.weak("No local backups.");
            return None;
        }

        let bound: HashSet<String> = Store::get(|s| s.instances()).map(|v| v.into_iter().map(|r| r.game_instance_id).collect()).unwrap_or_default();
        let mut outcome = None;

        for b in &rows {
            let name = catalog.iter().find(|g| g.id == b.game_instance_id).map(|g| g.name.as_str());
            let mapped = bound.contains(&b.game_instance_id);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new(name.unwrap_or(&b.game_instance_id)).strong());
                ui.small(egui::RichText::new(b.original_path.display().to_string()).weak());
                ui.small(format!("{} · {} · {}", human_size(b.size_bytes), humanize_since(&b.overwritten_at), reason_gloss(&b.reason)));
                ui.add_space(4.0);

                match &self.backup_confirm {
                    Some(BackupConfirm::Delete(id)) if *id == b.id => {
                        ui.horizontal(|ui| {
                            ui.label("Delete this backup?");
                            if ui.button(egui::RichText::new("Delete").color(ui.visuals().error_fg_color)).clicked() {
                                self.delete_backup(b.id, &b.content_hash);
                            }
                            if ui.button("Keep").clicked() {
                                self.backup_confirm = None;
                            }
                        });
                    }
                    Some(BackupConfirm::Restore(id)) if *id == b.id => {
                        ui.label("Write this to disk and re-upload it as the newest save? Your current save is backed up first.");
                        ui.horizontal(|ui| {
                            if ui.button("Confirm restore").clicked() {
                                outcome = Some(ConfigOutcome::RestoreBackup { instance_id: b.game_instance_id.clone(), content_hash: b.content_hash.clone() });
                                self.backup_confirm = None;
                            }
                            if ui.button("Cancel").clicked() {
                                self.backup_confirm = None;
                            }
                        });
                    }
                    _ => {
                        ui.horizontal(|ui| {
                            if ui.add_enabled(mapped, egui::Button::new("Restore")).on_disabled_hover_text("Map this game to a local save file first, on its page in Home.").clicked() {
                                self.backup_confirm = Some(BackupConfirm::Restore(b.id));
                            }
                            if ui.button("Delete").clicked() {
                                self.backup_confirm = Some(BackupConfirm::Delete(b.id));
                            }
                            if let Some(dir) = b.original_path.parent()
                                && ui.button("Open folder").clicked()
                            {
                                open_path(dir);
                            }
                        });
                    }
                }
            });
            ui.add_space(4.0);
        }

        outcome
    }

    /// Drop a backup index row and, if nothing else references the blob, its file.
    fn delete_backup(&mut self, id: i64, _hash: &str) {
        self.backup_confirm = None;
        match Store::write(|s| s.delete_backup(id)) {
            Ok(Some(orphan)) => {
                let _ = std::fs::remove_file(BACKUP_DIR.join(&orphan));
            }
            Ok(None) => {}
            Err(e) => self.error = Some(format!("Couldn't delete the backup: {e:#}")),
        }
    }

    fn advanced(&mut self, ui: &mut egui::Ui) {
        let mut level = Config::get(|c| c.advanced.log_level);
        ui.horizontal(|ui| {
            ui.label("Log level");
            if combo(ui, "log_level", &mut level, LOG_LEVELS) {
                let r = Config::update(|c| c.advanced.log_level = level);
                self.note_save(r);
            }
        });
        ui.small("Applies on restart.");

        ui.add_space(8.0);

        let current = Config::get(|c| c.advanced.crash_reports);
        let mut on = current.unwrap_or(false);
        if ui.checkbox(&mut on, "Send anonymous crash reports").changed() {
            let r = Config::update(|c| c.set_crash_reports(on));
            self.note_save(r);
        }
        if current.is_none() {
            ui.small("Not answered yet, you'll be asked once after signing in.");
        } else {
            ui.small("Applies on restart.");
        }

        ui.add_space(8.0);

        ui.label("API base URL");
        let resp = ui.text_edit_singleline(&mut self.drafts.api_base);
        if resp.lost_focus() {
            let value = self.drafts.api_base.trim().to_owned();
            let stored = Config::get(|c| c.advanced.api_base.to_string());
            if value.is_empty() {
                self.drafts.api_base = stored;
            } else if value != stored {
                let r = Config::update(|c| c.advanced.api_base = Arc::from(value.as_str()));
                self.note_save(r);
            }
        }
        ui.small("For local dev / self-host. Restart CoinCell after changing.");

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);

        ui.horizontal_wrapped(|ui| {
            if ui.button("Open config folder").clicked() {
                open_path(*crate::constants::CONFIG_DIR);
            }
            if ui.button("Open data folder").clicked() {
                open_path(*crate::constants::DATA_DIR);
            }
            if ui.button("Open logs folder").clicked() {
                open_path(&crate::logging::log_dir());
            }
            if ui.button("Copy diagnostics").clicked() {
                ui.copy_text(diagnostics());
            }
        });
    }
}

// ---- helpers ---------------------------------------------------------------

fn rail_button(ui: &mut egui::Ui, section: Section, selected: bool) -> egui::Response {
    ui.add_space(3.0);
    let glyph = egui::RichText::new(section.icon()).size(20.0);
    ui.selectable_label(selected, glyph).on_hover_text(section.title())
}

/// A combo box driven by a `(label, value)` preset table. Returns `true` if the
/// value changed this frame.
fn combo<T: PartialEq + Copy>(ui: &mut egui::Ui, id: &str, value: &mut T, presets: &[(&str, T)]) -> bool {
    let selected = presets.iter().find(|(_, v)| v == value).map(|(l, _)| *l).unwrap_or("Custom");
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(selected).show_ui(ui, |ui| {
        for (label, v) in presets {
            changed |= ui.selectable_value(value, *v, *label).changed();
        }
    });
    changed
}

/// UI-scale combo. Floats don't survive a TOML round-trip bit-for-bit, so preset
/// matching is by tolerance rather than the generic `combo`'s `==`.
fn scale_combo(ui: &mut egui::Ui, value: &mut f32) -> bool {
    const STEPS: [(&str, f32); 7] = [("75%", 0.75), ("90%", 0.9), ("100%", 1.0), ("110%", 1.1), ("125%", 1.25), ("150%", 1.5), ("175%", 1.75)];
    let selected = STEPS.iter().find(|(_, v)| (v - *value).abs() < 0.01).map(|(l, _)| *l).unwrap_or("Custom");
    let mut changed = false;
    egui::ComboBox::from_id_salt("ui_scale").selected_text(selected).show_ui(ui, |ui| {
        for (label, v) in STEPS {
            if ui.selectable_label((v - *value).abs() < 0.01, label).clicked() {
                *value = v;
                changed = true;
            }
        }
    });
    changed
}

/// Renders an "are you sure?" body. `Some(true)` = confirmed, `Some(false)` =
/// cancelled, `None` = still waiting.
fn confirm(ui: &mut egui::Ui, prompt: &str, affirm: &str) -> Option<bool> {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(icons::WARNING).size(18.0).color(ui.visuals().warn_fg_color));
        ui.label("Are you sure?");
    });
    ui.add_space(4.0);
    ui.label(prompt);
    ui.add_space(12.0);

    let mut result = None;
    ui.horizontal(|ui| {
        if ui.button("Cancel").clicked() {
            result = Some(false);
        }
        if ui.button(egui::RichText::new(affirm).color(ui.visuals().error_fg_color)).clicked() {
            result = Some(true);
        }
    });
    result
}

fn diagnostics() -> String {
    let (signed_in, level) = Config::get(|c| (c.session_id().is_some(), c.advanced.log_level));
    format!(
        "CoinCell {ver} ({channel}, {commit})\nOS: {os} {arch}\nConfig: {cfg}\nData: {data}\nLogs: {logs}\nLog level: {level:?}\nSigned in: {signed_in}",
        ver = crate::version::VERSION,
        channel = crate::version::CHANNEL,
        commit = crate::version::COMMIT,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        cfg = crate::constants::CONFIG_DIR.display(),
        data = crate::constants::DATA_DIR.display(),
        logs = crate::logging::log_dir().display(),
    )
}

const POLL_PRESETS: &[(&str, PollInterval)] = &[
    ("Automatic", PollInterval::Auto),
    ("Off", PollInterval::Off),
    ("Every 30s", PollInterval::Every(Duration::from_secs(30))),
    ("Every 2m", PollInterval::Every(Duration::from_secs(120))),
    ("Every 5m", PollInterval::Every(Duration::from_secs(300))),
    ("Every 15m", PollInterval::Every(Duration::from_secs(900))),
    ("Every hour", PollInterval::Every(Duration::from_secs(3600))),
];

const UPLOAD_TRIGGERS: &[(&str, UploadTrigger)] = &[("A file changes", UploadTrigger::OnChange), ("The emulator exits", UploadTrigger::OnEmulatorExit), ("Only manually", UploadTrigger::Manual)];

const CONFLICTS: &[(&str, ConflictPolicy)] =
    &[("Ask me", ConflictPolicy::Ask), ("Keep local", ConflictPolicy::PreferLocal), ("Keep remote", ConflictPolicy::PreferRemote), ("Keep newest", ConflictPolicy::PreferNewest)];

const THEMES: &[(&str, Theme)] = &[("Follow account", Theme::Account), ("Follow system", Theme::Auto), ("Light", Theme::Light), ("Dark", Theme::Dark)];

const CHANNELS: &[(&str, UpdateChannel)] = &[("Stable", UpdateChannel::Stable), ("Pre-release", UpdateChannel::Prerelease)];

const UPDATE_ACTIONS: &[(&str, UpdateAction)] = &[("Notify me", UpdateAction::Notify), ("Download only", UpdateAction::Download), ("Install automatically", UpdateAction::Install)];

const LOG_LEVELS: &[(&str, LogLevel)] = &[("Error", LogLevel::Error), ("Warn", LogLevel::Warn), ("Info", LogLevel::Info), ("Debug", LogLevel::Debug), ("Trace", LogLevel::Trace)];
