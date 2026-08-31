mod config;
mod fonts;
mod home;
mod icons;
mod mapping;

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use eframe::egui;
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

use crate::api::{self, Branding, Client, DeviceConfig, DeviceEvent, DeviceFlow, GameInstance, SessionCheck, SessionStatus};
use crate::app::config::{ConfigApp, ConfigOutcome};
use crate::app::home::{HomeApp, HomeOutcome, HomeView};
use crate::app::icons::MINIMISE;
use crate::config::Config;
use crate::constants::CLIENT_NAME;
use crate::notice::{self, Notice};
use crate::sync::{EngineEvent, RestoreSource, SyncEngine};
use crate::theme::{self, Scheme};
use crate::update::{self, Available};

/// Self-update UI state, driven from Config › Updates.
pub(crate) enum Updater {
    Idle,
    Checking(Receiver<Result<Option<Available>, String>>),
    /// Check finished: `Some` = a newer release, `None` = up to date.
    Checked(Option<Available>),
    Installing(Receiver<Result<(), String>>),
    /// Swap done; the window is closing and the new process is relaunching.
    Restarting,
    Error(String),
}

impl Updater {
    /// One-line status for the Config panel.
    pub(crate) fn status(&self) -> String {
        match self {
            Updater::Idle => format!("Version {}, not checked this session.", crate::version::VERSION),
            Updater::Checking(_) => "Checking for updates…".into(),
            Updater::Checked(av) => update::describe(av),
            Updater::Installing(_) => "Downloading and installing…".into(),
            Updater::Restarting => "Restarting…".into(),
            Updater::Error(e) => format!("Update check failed: {e}"),
        }
    }
    /// The release to offer an install for, if any.
    pub(crate) fn offer(&self) -> Option<&Available> {
        match self {
            Updater::Checked(Some(av)) => Some(av),
            _ => None,
        }
    }
    pub(crate) fn busy(&self) -> bool {
        matches!(self, Updater::Checking(_) | Updater::Installing(_) | Updater::Restarting)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    Hidden,
    ShowConfig,
    ShowHome,
}

impl WindowState {
    fn hidden(&self) -> bool {
        matches!(self, WindowState::Hidden)
    }
    fn is_home(&self) -> bool {
        matches!(self, WindowState::ShowHome)
    }
    fn is_config(&self) -> bool {
        matches!(self, WindowState::ShowConfig)
    }
}

enum AuthView {
    LoggedOut { error: Option<Arc<str>> },

    Validating { check: SessionCheck },

    Connecting { flow: DeviceFlow, approval: Option<Approval> },

    Ready,
}

struct Approval {
    user_code: Arc<str>,
    verification_uri: Arc<str>,
    verification_uri_complete: Arc<str>,
}

enum LoginIntent {
    None,
    StartFlow,
    ReopenBrowser,
}

/// A native file picker (`rfd`) running on its own thread. While one is live the
/// window won't auto-hide; the chosen path is handed back to `HomeApp` when it
/// lands.
struct PendingPick {
    instance_id: String,
    rx: Receiver<Option<std::path::PathBuf>>,
}

pub struct App {
    state: WindowState,
    prev_state: WindowState,
    focus_latch: bool,
    quitting: bool,
    /// Set once when a session first goes Ready and the crash-report preference
    /// has never been answered; drives the one-time first-run prompt.
    ask_crash_reports: bool,
    /// Set on first Ready when running a loose release build that isn't
    /// installed; drives the one-time "install CoinCell?" prompt.
    ask_install: bool,
    /// Set on first Ready until `[startup].onboarded`; drives the one-time
    /// "CoinCell lives in the tray" explainer (shown after usage data).
    ask_tray_intro: bool,
    wake_rx: Receiver<()>,
    auth: AuthView,
    device_config: DeviceConfig,
    /// Service branding (palette, name, links). Refreshed from `GET /api/branding`
    /// at launch, baked-in copy as the fallback.
    branding: Branding,
    /// The account's theme setting from `GET /api/me` (`Some(true)` = light,
    /// `Some(false)` = dark, `None` = follow system). Outer `None` = not fetched
    /// yet. Drives `Theme::Account`.
    account_theme: Option<Option<bool>>,
    /// Last scheme handed to [`theme::apply`], so we only re-apply on a change.
    applied_scheme: Option<Scheme>,
    /// Live while a session is Ready. Dropping it stops the stream + poller and
    /// the engine worker.
    sync: Option<SyncEngine>,
    /// Every game instance on the account, from the engine's hydrate, what Home
    /// renders. `false` until the first `Hydrated` lands.
    catalog: Vec<GameInstance>,
    catalog_ready: bool,
    /// A save-file picker Home asked for, running off the UI thread. While set,
    /// the window won't auto-hide.
    pending_pick: Option<PendingPick>,
    /// Last `Visible(_)` we told the OS window, so we only re-send on a change.
    visible_cmd: Option<bool>,
    /// Frames left where `reconcile_visibility` re-asserts unconditionally, to
    /// out-last eframe's first-paint `set_visible(true)`.
    visible_settling: u8,
    config_app: ConfigApp,
    home_app: HomeApp,
    updater: Updater,
}

impl App {
    pub fn new(ctx: egui::Context, wake_rx: Receiver<()>, device_config: DeviceConfig, branding: Branding, start_hidden: bool) -> Self {
        fonts::install(&ctx);
        egui_extras::install_image_loaders(&ctx);
        // Persistent disk cache for remote art; must come after the line above so
        // it's tried ahead of egui_extras' network loader.
        crate::asset::install(&ctx);

        // Theme the first frame (the sign-in screen) before anything draws.
        let scheme = theme::resolve(Config::get(|c| c.appearance.theme), None, ctx.system_theme(), &branding);
        theme::apply(&ctx, &branding, scheme);

        let auth = match api_client() {
            Some(client) => AuthView::Validating { check: SessionCheck::start(client, waker(&ctx)) },
            None => AuthView::LoggedOut { error: None },
        };
        // `[startup].start_hidden`: begin in the tray, no window. The tray toggle
        // sends `Visible(true)` from here, so the two stay in step.
        let state = if start_hidden { WindowState::Hidden } else { WindowState::ShowConfig };
        Self {
            state,
            prev_state: state,
            focus_latch: true,
            quitting: false,
            ask_crash_reports: false,
            ask_install: false,
            ask_tray_intro: false,
            wake_rx,
            auth,
            device_config,
            branding,
            account_theme: None,
            applied_scheme: Some(scheme),
            sync: None,
            catalog: Vec::new(),
            catalog_ready: false,
            pending_pick: None,
            visible_cmd: None,
            visible_settling: 0,
            config_app: ConfigApp::new(),
            home_app: HomeApp::new(),
            updater: Updater::Idle,
        }
    }

    /// Re-resolve the theme from the current preference / account / OS and apply
    /// it if it changed. Called every frame, like the zoom-factor sync.
    fn sync_theme(&mut self, ctx: &egui::Context) {
        let want = theme::resolve(Config::get(|c| c.appearance.theme), self.account_theme, ctx.system_theme(), &self.branding);
        if self.applied_scheme != Some(want) {
            theme::apply(ctx, &self.branding, want);
            self.applied_scheme = Some(want);
        }
    }

    /// Start the sync engine (which owns the realtime + polling stream), unless
    /// syncing is paused or one is already running.
    fn start_sync(&mut self, ctx: &egui::Context) {
        if self.sync.is_some() || !Config::get(|c| c.sync.enabled) {
            return;
        }
        let Some(client) = api_client() else { return };
        self.sync = Some(SyncEngine::start(client, waker(ctx)));
    }

    /// Drain what the engine has surfaced. Download/write already happened on the
    /// worker; this keeps the catalog current and reacts to session expiry.
    fn drain_sync(&mut self, ctx: &egui::Context) {
        let Some(engine) = &self.sync else { return };
        let mut session_expired = false;
        for event in engine.events() {
            match event {
                EngineEvent::Hydrated { instances } => {
                    self.catalog = instances;
                    self.catalog_ready = true;
                }
                EngineEvent::SaveAdvanced { instance_id, latest } => {
                    if let Some(row) = self.catalog.iter_mut().find(|g| g.id == instance_id) {
                        row.last_saved_at = Some(latest.uploaded_at.clone());
                        row.latest_save = Some(latest);
                    }
                }
                EngineEvent::Status(status) => tracing::debug!("stream {status:?}"),
                EngineEvent::Pulled { instance_id } => {
                    tracing::info!("pulled newer save for {instance_id}");
                    notice::post(Notice::Pulled { game: self.game_label(&instance_id) });
                }
                EngineEvent::Pushed { instance_id } => tracing::info!("pushed save for {instance_id}"),
                EngineEvent::Restored { instance_id } => {
                    tracing::info!("restored a save for {instance_id}");
                    self.home_app.note_restored(&instance_id);
                }
                EngineEvent::PushPending { instance_id } => tracing::debug!("{instance_id}: local change waiting (manual upload)"),
                EngineEvent::Conflict { instance_id } => {
                    tracing::warn!("{instance_id}: conflict, resolve in Home");
                    notice::post(Notice::Conflict { game: self.game_label(&instance_id) });
                }
                // TODO(notice): decide which sync errors are toast-worthy before wiring `Notice::Error` ([notifications].on_error already exists).
                EngineEvent::Error(message) => tracing::warn!("sync: {message}"),
                EngineEvent::SessionExpired => session_expired = true,
            }
        }
        if session_expired {
            self.handle_session_expired(ctx);
        }
    }

    /// A human name for an instance id, for notice text; the id itself if the
    /// catalog doesn't have it (e.g. it was deleted server side).
    fn game_label(&self, instance_id: &str) -> String {
        self.catalog.iter().find(|g| g.id == instance_id).map(|g| g.name.clone()).unwrap_or_else(|| instance_id.to_owned())
    }

    /// Spawn a native picker on its own thread (RFD inits COM per call, so any
    /// thread is fine). The window stops auto-hiding until it resolves.
    fn open_save_dialog(&mut self, ctx: &egui::Context, instance_id: String, title: String) {
        if self.pending_pick.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("save-file-dialog".into())
            .spawn(move || {
                let _ = tx.send(mapping::pick_save_file(&title));
                ctx.request_repaint();
            })
            .expect("spawn save-file-dialog thread");
        self.pending_pick = Some(PendingPick { instance_id, rx });
    }

    /// Hand a finished pick back to Home (`None` means the user cancelled). The
    /// focus latch is re-armed: the native dialog held focus, and the OS can be
    /// a frame or two returning it, which would otherwise read as a focus-loss
    /// auto-hide.
    fn drain_pending_pick(&mut self) {
        let Some(pending) = &self.pending_pick else { return };
        let Ok(result) = pending.rx.try_recv() else { return };
        let instance_id = pending.instance_id.clone();
        self.pending_pick = None;
        self.focus_latch = true;
        self.home_app.deliver_save_pick(&instance_id, result);
    }

    /// Kick off a GitHub Releases check on a worker thread.
    fn start_update_check(&mut self, ctx: &egui::Context) {
        if self.updater.busy() {
            return;
        }
        let allow_prerelease = Config::get(|c| c.updates.channel) == crate::config::UpdateChannel::Prerelease;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                let _ = tx.send(update::check(allow_prerelease).map_err(|e| format!("{e:#}")));
                ctx.request_repaint();
            })
            .expect("spawn update-check thread");
        self.updater = Updater::Checking(rx);
    }

    /// Download + verify + swap the pending update on a worker thread.
    fn start_update_install(&mut self, ctx: &egui::Context) {
        let Updater::Checked(Some(av)) = std::mem::replace(&mut self.updater, Updater::Idle) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("update-apply".into())
            .spawn(move || {
                let _ = tx.send(update::apply(&av).map_err(|e| format!("{e:#}")));
                ctx.request_repaint();
            })
            .expect("spawn update-apply thread");
        self.updater = Updater::Installing(rx);
    }

    /// Advance the updater state machine; on a finished install, shut down so the
    /// relaunched process can take over.
    fn drain_updater(&mut self, ctx: &egui::Context) {
        let next = match &self.updater {
            Updater::Checking(rx) => match rx.try_recv() {
                Ok(Ok(av)) => Some(Updater::Checked(av)),
                Ok(Err(e)) => Some(Updater::Error(e)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Updater::Error("the check thread stopped unexpectedly".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            Updater::Installing(rx) => match rx.try_recv() {
                Ok(Ok(())) => Some(Updater::Restarting),
                Ok(Err(e)) => Some(Updater::Error(e)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Updater::Error("the update thread stopped unexpectedly".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            _ => None,
        };
        if let Some(state) = next {
            let restarting = matches!(state, Updater::Restarting);
            self.updater = state;
            if restarting {
                tracing::info!("update applied; closing so the new build can relaunch");
                self.sync = None;
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Reset the sub-screen to its default whenever the visible window changes,
    /// so reopening the tray always lands on a clean state.
    fn sync_shown_screen(&mut self) {
        if self.state == self.prev_state {
            return;
        }
        match self.state {
            WindowState::ShowConfig => self.config_app.reset(),
            WindowState::ShowHome => self.home_app.on_reopen(),
            WindowState::Hidden => {}
        }
        self.prev_state = self.state;
    }

    /// Tear down every trace of the current session and return to the sign-in
    /// screen, showing `error` there if one is given.
    fn clear_session_state(&mut self, error: Option<Arc<str>>) {
        let _ = Config::update(|c| c.clear_session());
        self.sync = None;
        self.catalog = Vec::new();
        self.catalog_ready = false;
        self.pending_pick = None;
        self.auth = AuthView::LoggedOut { error };
        self.focus_latch = true;
        self.ask_crash_reports = false;
        self.ask_install = false;
        self.ask_tray_intro = false;
        self.account_theme = None;
        self.config_app.reset();
        self.home_app.reset();
    }

    /// User-initiated logout: also tell the server to revoke this session.
    fn begin_logout(&mut self, ctx: &egui::Context) {
        if let Some(client) = api_client() {
            api::revoke_in_background(client);
        }
        self.clear_session_state(None);
        ctx.request_repaint();
    }

    /// The engine hit a `401`/`403` mid-sync. The session is already dead, so no
    /// revoke call, just drop back to sign-in with a note.
    fn handle_session_expired(&mut self, ctx: &egui::Context) {
        if matches!(self.auth, AuthView::LoggedOut { .. }) {
            return; // already handled this round
        }
        tracing::warn!("session expired mid-sync, signing out");
        self.clear_session_state(Some(Arc::from("Your session expired. Please sign in again.")));
        notice::post(Notice::SessionExpired);
        ctx.request_repaint();
    }

    /// Pick the next pending first-run modal, in order: install → usage data →
    /// tray explainer. Called on first `Ready` and after each one resolves; when
    /// nothing is pending all three flags stay `false`.
    fn advance_first_run_prompts(&mut self) {
        self.ask_install = false;
        self.ask_crash_reports = false;
        self.ask_tray_intro = false;
        if crate::install::needs_first_run_prompt() {
            self.ask_install = true;
        } else if !Config::get(|c| c.crash_reports_answered()) {
            self.ask_crash_reports = true;
        } else if !Config::get(|c| c.startup.onboarded) {
            self.ask_tray_intro = true;
        }
    }

    /// One-time modal shown right after the first sign-in when the crash-report
    /// preference has never been set. Either choice is recorded; a backdrop /
    /// Esc dismiss counts as "no".
    fn crash_reports_prompt(&mut self, ctx: &egui::Context) {
        let resp = egui::Modal::new(egui::Id::new("first_run_crash_reports")).show(ctx, |ui| {
            ui.set_max_width(260.0);
            ui.heading("Crash reports");
            ui.add_space(6.0);
            ui.label("Send anonymous crash and error reports to help fix bugs? Save contents and personal data are never included. You can change this any time under Advanced.");
            ui.add_space(12.0);
            let mut pick = None;
            ui.horizontal(|ui| {
                if ui.button("No thanks").clicked() {
                    pick = Some(false);
                }
                if ui.button("Send reports").clicked() {
                    pick = Some(true);
                }
            });
            pick
        });

        if let Some(enabled) = resp.inner.or_else(|| resp.should_close().then_some(false)) {
            let _ = Config::update(|c| c.set_crash_reports(enabled));
            self.advance_first_run_prompts();
        }
    }

    /// One-time modal after the usage-data prompt: explain that CoinCell lives
    /// in the tray so a fresh install that starts hidden isn't a mystery. "Got
    /// it" sets `[startup].onboarded`, which also re-enables `start_hidden` and
    /// focus-loss auto-hide from here on.
    fn tray_intro_prompt(&mut self, ctx: &egui::Context) {
        let resp = egui::Modal::new(egui::Id::new("first_run_tray_intro")).show(ctx, |ui| {
            ui.set_max_width(280.0);
            ui.heading("CoinCell runs in the tray");
            ui.add_space(6.0);
            ui.label("Closing this window doesn't quit CoinCell, it keeps syncing from the system tray.");
            ui.add_space(6.0);
            ui.label("\u{2022}  Left-click the tray icon for your games");
            ui.label("\u{2022}  Right-click it for settings");
            ui.label("\u{2022}  The close button (top right) or Esc sends this window back to the tray");
            ui.add_space(12.0);
            ui.vertical_centered(|ui| ui.button("Got it").clicked()).inner
        });

        if resp.inner || resp.should_close() {
            let _ = Config::update(|c| c.startup.onboarded = true);
            self.advance_first_run_prompts();
        }
    }

    /// One-time modal on the first sign-in from a loose release build: offer to
    /// install into a stable location (which also enables self-update) and hand
    /// off to the installed copy. "Not now" is remembered.
    fn install_prompt(&mut self, ctx: &egui::Context) {
        let dst = crate::install::canonical_exe().ok();
        let resp = egui::Modal::new(egui::Id::new("first_run_install")).show(ctx, |ui| {
            ui.set_max_width(280.0);
            ui.heading("Install CoinCell?");
            ui.add_space(6.0);
            ui.label("You're running CoinCell from wherever you unzipped it. Installing copies it to a stable location, adds a Start Menu / launcher entry, and lets it keep itself up to date.");
            if let Some(dst) = &dst {
                ui.add_space(4.0);
                ui.small(dst.display().to_string());
            }
            ui.add_space(12.0);
            let mut pick = None;
            ui.horizontal(|ui| {
                if ui.button("Not now").clicked() {
                    pick = Some(false);
                }
                if ui.button("Install").clicked() {
                    pick = Some(true);
                }
            });
            pick
        });

        let Some(install) = resp.inner.or_else(|| resp.should_close().then_some(false)) else {
            return;
        };
        if install {
            match crate::install::install() {
                Ok(path) => return self.relaunch_from(ctx, path),
                Err(e) => tracing::error!("first-run install failed: {e:#}"),
            }
        }
        // Declined, dismissed, or it failed: don't nag again (the Config button
        // stays), and move on to the next first-run prompt.
        let _ = Config::update(|c| c.startup.skip_install_prompt = true);
        self.advance_first_run_prompts();
    }

    /// A self-install / update-in-place wrote the binary at `path`; spawn it
    /// (with the post-update flag so it waits out our single-instance lock) and
    /// close this process.
    fn relaunch_from(&mut self, ctx: &egui::Context, path: std::path::PathBuf) {
        tracing::info!("handing off to {}", path.display());
        self.sync = None;
        self.quitting = true;
        if let Err(e) = std::process::Command::new(&path).arg("--relaunched-after-update").spawn() {
            tracing::error!("couldn't spawn {}: {e}", path.display());
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Set desired window state. The OS window is brought in line by
    /// [`Self::reconcile_visibility`] on the next `logic()` tick, not here.
    fn hide(&mut self) {
        self.state = WindowState::Hidden;
        self.focus_latch = false;
    }

    /// Command the OS window to match `state` (visible unless `Hidden`). eframe
    /// force-shows the window once after the first paint (its anti-flash hack)
    /// and `ViewportBuilder::with_visible` is ignored, so this is the only thing
    /// that reliably keeps a `start_hidden` launch hidden. For the first few
    /// frames it re-asserts unconditionally (and repaints) to out-last that
    /// one-shot show; after that it only sends on a change.
    fn reconcile_visibility(&mut self, ctx: &egui::Context) {
        let want_visible = !self.state.hidden();
        let settling = self.visible_settling < 3;
        if settling {
            self.visible_settling += 1;
            ctx.request_repaint();
        }
        if settling || self.visible_cmd != Some(want_visible) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(want_visible));
            self.visible_cmd = Some(want_visible);
        }
    }

    /// The shared title bar for every screen (Config, Home, sign-in): screen
    /// name on the left, a minimise-to-tray button on the right. Hidden while a
    /// native dialog would be up is unnecessary, it's just chrome.
    fn header(&mut self, ui: &mut egui::Ui) {
        let title = if !matches!(self.auth, AuthView::Ready) {
            "CoinCell"
        } else if self.state.is_home() {
            "Home"
        } else {
            "Config"
        };

        let mut minimise = false;
        egui::Panel::top("app_header").exact_size(26.0).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(egui::RichText::new(title).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    minimise = ui.button(egui::RichText::new(MINIMISE).size(14.0)).on_hover_text("Minimise to tray (Esc)").clicked();
                });
            });
        });
        if minimise {
            self.hide();
        }
    }

    fn show_home(&mut self) {
        self.state = WindowState::ShowHome;
        self.focus_latch = true;
    }

    fn show_config(&mut self) {
        self.state = WindowState::ShowConfig;
        self.focus_latch = true;
    }

    fn pump_auth(&mut self, ctx: &egui::Context) {
        enum Next {
            Stay,
            Ready,
            /// Session just saved after a device login, bounce through
            /// validation once to confirm it and pick up the account theme.
            Revalidate,
            LoggedOut(Option<Arc<str>>),
        }

        let next = match &mut self.auth {
            AuthView::Connecting { flow, approval } => {
                let mut done: Option<Arc<str>> = None;
                let mut failed: Option<Arc<str>> = None;
                for event in flow.events() {
                    match event {
                        DeviceEvent::AwaitingApproval { user_code, verification_uri, verification_uri_complete } => {
                            *approval = Some(Approval { user_code, verification_uri, verification_uri_complete });
                        }
                        DeviceEvent::Completed { session_id } => done = Some(session_id),
                        DeviceEvent::Failed { message } => failed = Some(message),
                    }
                }

                if let Some(session_id) = done {
                    match Config::update(|c| c.set_session(session_id)) {
                        Ok(()) => Next::Revalidate,
                        Err(e) => Next::LoggedOut(Some(Arc::from(format!("couldn't save login: {e:#}")))),
                    }
                } else if let Some(message) = failed {
                    Next::LoggedOut(Some(message))
                } else {
                    Next::Stay
                }
            }
            AuthView::Validating { check } => match check.poll() {
                Some(SessionStatus::Valid(me)) => {
                    self.account_theme = Some(me.theme);
                    Next::Ready
                }
                Some(SessionStatus::Invalid) => Next::LoggedOut(Some(Arc::from("Your session has expired. Please sign in again."))),
                Some(SessionStatus::Unknown(msg)) => {
                    tracing::info!("couldn't verify stored session, keeping it: {msg}");
                    Next::Ready
                }
                None => Next::Stay,
            },
            _ => Next::Stay,
        };

        match next {
            Next::Stay => {}
            Next::Ready => {
                self.auth = AuthView::Ready;
                self.focus_latch = true;
                self.advance_first_run_prompts();
                self.start_sync(ctx);
            }
            Next::Revalidate => match api_client() {
                Some(client) => self.auth = AuthView::Validating { check: SessionCheck::start(client, waker(ctx)) },
                None => self.auth = AuthView::Ready,
            },
            Next::LoggedOut(error) => {
                self.auth = AuthView::LoggedOut { error };
                self.focus_latch = true;
                self.sync = None;
            }
        }
    }

    fn login_screen(&self, ui: &mut egui::Ui) -> LoginIntent {
        let mut intent = LoginIntent::None;
        let identity = &self.branding.identity;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.22).max(0.0));
                ui.heading(crate::constants::APP_NAME);
                if !identity.tagline.is_empty() {
                    ui.add_space(2.0);
                    ui.weak(&identity.tagline);
                }
                ui.add_space(18.0);
                match &self.auth {
                    AuthView::Validating { .. } => {
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label("Checking your session…");
                    }
                    AuthView::LoggedOut { error } => {
                        if ui.button("Retrieve login code").clicked() {
                            intent = LoginIntent::StartFlow;
                        }
                        if let Some(error) = error {
                            ui.add_space(12.0);
                            ui.colored_label(egui::Color32::LIGHT_RED, error.as_ref());
                        }
                    }
                    AuthView::Connecting { approval: None, .. } => {
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label("Requesting a login code…");
                    }
                    AuthView::Connecting { approval: Some(approval), .. } => {
                        ui.label("Enter this code when your browser asks for it:");
                        ui.add_space(4.0);
                        if ui.button(approval.user_code.as_ref()).clicked() {
                            ui.copy_text((*approval.user_code).to_string());
                        };
                        ui.add_space(8.0);
                        ui.hyperlink(approval.verification_uri.as_ref());
                        ui.add_space(4.0);
                        if ui.button("Open browser again").clicked() {
                            intent = LoginIntent::ReopenBrowser;
                        }

                        ui.add_space(16.0);
                        ui.separator();
                        ui.add_space(16.0);

                        ui.label("You can also navigate to settings -> Link a Device and input the code manually");

                        ui.add_space(16.0);

                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label("Waiting for approval…");
                    }
                    AuthView::Ready => {}
                }
            });

            if !identity.attribution_text.is_empty() {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    ui.hyperlink_to(egui::RichText::new(&identity.attribution_text).weak().small(), &identity.homepage_url);
                });
            }
        });
        intent
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        let scale = Config::get(|c| c.ui_scale());
        if (ctx.zoom_factor() - scale).abs() > f32::EPSILON {
            ctx.set_zoom_factor(scale);
        }

        self.sync_theme(ctx);

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Down, .. } => {
                    if self.state.hidden() {
                        self.show_home();
                    } else {
                        if self.state.is_config() {
                            self.state = WindowState::ShowHome;
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                TrayIconEvent::Click { button: MouseButton::Right, button_state: MouseButtonState::Down, .. } => {
                    if self.state.hidden() {
                        self.show_config();
                    } else {
                        if self.state.is_home() {
                            self.state = WindowState::ShowConfig;
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                _ => {}
            }
        }
        while self.wake_rx.try_recv().is_ok() {
            if self.state.hidden() {
                self.show_home();
            } else {
                if self.state.is_home() {
                    self.state = WindowState::ShowConfig;
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        self.pump_auth(ctx);
        self.drain_sync(ctx);
        notice::pump();
        self.drain_pending_pick();
        self.drain_updater(ctx);
        self.sync_shown_screen();
        self.reconcile_visibility(ctx);

        if self.state.is_config() {
            self.config_app.logic(ctx, frame);
        } else if self.state.is_home() {
            self.home_app.logic(ctx, frame);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.state.hidden() {
            // Paint a valid (blank, themed) frame so a window that's momentarily
            // shown, e.g. during the first-frame settle, is never just black.
            egui::CentralPanel::default().show(ui, |_| {});
            ui.ctx().request_repaint_after(Duration::from_millis(250));
            return;
        }

        let busy_auth = matches!(self.auth, AuthView::Connecting { .. } | AuthView::Validating { .. });
        // Anything that hands focus to a native window we don't own, plus the
        // first-run modals: losing focus to one of those must not minimise us.
        let modal_active = busy_auth || self.pending_pick.is_some() || self.ask_install || self.ask_crash_reports || self.ask_tray_intro;

        let lost_focus = ui.input(|i| {
            if i.focused {
                self.focus_latch = false;
                false
            } else {
                !self.focus_latch
            }
        });
        // Never auto-hide before onboarding (even if the user enabled it), nor
        // while a modal is up.
        if lost_focus && !self.quitting && !modal_active && Config::get(|c| c.startup.onboarded && c.window.hide_on_focus_loss) {
            ui.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide();
        }

        // Shared title bar (carries the minimise button). Esc is handled at the
        // end of the frame so an open popup or the modal claims it first.
        self.header(ui);

        if matches!(self.auth, AuthView::Ready) {
            if self.ask_install {
                self.install_prompt(ui.ctx());
            } else if self.ask_crash_reports {
                self.crash_reports_prompt(ui.ctx());
            } else if self.ask_tray_intro {
                self.tray_intro_prompt(ui.ctx());
            }

            if self.state.is_config() {
                match self.config_app.ui(ui, frame, &self.branding, &self.catalog, &self.updater) {
                    ConfigOutcome::Stay => {}
                    ConfigOutcome::SyncNow => {
                        if let Some(engine) = &self.sync {
                            engine.sync_now();
                        }
                    }
                    ConfigOutcome::RestoreBackup { instance_id, content_hash } => {
                        if let Some(engine) = &self.sync {
                            engine.restore(&instance_id, RestoreSource::Backup { content_hash });
                        }
                    }
                    ConfigOutcome::CheckForUpdate => self.start_update_check(ui.ctx()),
                    ConfigOutcome::InstallUpdate => self.start_update_install(ui.ctx()),
                    ConfigOutcome::RelaunchFrom(path) => self.relaunch_from(ui.ctx(), path),
                    ConfigOutcome::LogOut => self.begin_logout(ui.ctx()),
                    ConfigOutcome::Quit => {
                        self.quitting = true;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            } else if self.state.is_home()
                && let Some(client) = api_client()
            {
                let home = HomeView { catalog: &self.catalog, ready: self.catalog_ready, api_base: &self.device_config.api_base, branding: &self.branding, client: &client };
                match self.home_app.ui(ui, frame, home) {
                    HomeOutcome::Stay => {}
                    HomeOutcome::Refresh => {
                        if let Some(engine) = &self.sync {
                            engine.rehydrate();
                        }
                    }
                    HomeOutcome::MappedInstance { instance_id } => {
                        if let Some(engine) = &self.sync {
                            engine.recheck(&instance_id);
                            engine.rehydrate();
                        }
                    }
                    HomeOutcome::RecheckInstance { instance_id } => {
                        if let Some(engine) = &self.sync {
                            engine.recheck(&instance_id);
                        }
                    }
                    HomeOutcome::ResolveConflict { instance_id, keep_local } => {
                        if let Some(engine) = &self.sync {
                            engine.resolve_conflict(&instance_id, keep_local);
                        }
                    }
                    HomeOutcome::OpenSaveDialog { instance_id, title } => {
                        self.open_save_dialog(ui.ctx(), instance_id, title);
                    }
                    HomeOutcome::Restore { instance_id, source } => {
                        if let Some(engine) = &self.sync {
                            engine.restore(&instance_id, source);
                        }
                    }
                }
            }
        } else {
            match self.login_screen(ui) {
                LoginIntent::StartFlow => {
                    self.auth = AuthView::Connecting { flow: DeviceFlow::start(self.device_config.clone(), CLIENT_NAME.as_str(), waker(ui.ctx())), approval: None };
                }
                LoginIntent::ReopenBrowser => {
                    if let AuthView::Connecting { approval: Some(approval), .. } = &self.auth {
                        api::open_in_browser(&approval.verification_uri_complete);
                    }
                }
                LoginIntent::None => {}
            }
        }

        // Esc minimises, but only if nothing this frame (an open combo, the
        // first-run modal, a text edit) already claimed the key.
        if !modal_active && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.hide();
        }

        // A `hide()` above takes effect on the next `logic()` tick; nudge one.
        if self.state.hidden() {
            ui.ctx().request_repaint();
        }
        ui.request_repaint_after(Duration::from_millis(100));
    }
}

/// Open a file or folder in the OS file manager.
fn open_path(path: &std::path::Path) {
    let cmd = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

/// A `Client` for the current session, or `None` when logged out.
fn api_client() -> Option<Client> {
    let (base, session) = Config::get(|c| (c.advanced.api_base.clone(), c.session_id()));
    session.map(|session| Client::new(base, session))
}

/// A repaint callback for the API's background workers, so their events surface
/// promptly instead of waiting for the next timed repaint.
fn waker(ctx: &egui::Context) -> impl Fn() + Send + Clone + 'static {
    let ctx = ctx.clone();
    move || ctx.request_repaint()
}
