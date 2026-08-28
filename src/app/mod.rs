mod config;
mod home;
mod icons;

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use eframe::egui;
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

use crate::api::{self, Branding, Client, DeviceConfig, DeviceEvent, DeviceFlow, SessionCheck, SessionStatus, SyncEvent, SyncStream};
use crate::app::config::{ConfigApp, ConfigOutcome};
use crate::app::home::HomeApp;
use crate::config::{Config, PollInterval};
use crate::constants::CLIENT_NAME;
use crate::theme::{self, Scheme};

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

pub struct App {
    state: WindowState,
    prev_state: WindowState,
    focus_latch: bool,
    quitting: bool,
    /// Set once when a session first goes Ready and the crash-report preference
    /// has never been answered; drives the one-time first-run prompt.
    ask_crash_reports: bool,
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
    /// Live while a session is Ready. Dropping it stops the stream + poller.
    sync: Option<SyncStream>,
    config_app: ConfigApp,
    home_app: HomeApp,
}

impl App {
    pub fn new(ctx: egui::Context, wake_rx: Receiver<()>, device_config: DeviceConfig, branding: Branding) -> Self {
        icons::install(&ctx);

        // Theme the first frame (the sign-in screen) before anything draws.
        let scheme = theme::resolve(Config::get(|c| c.appearance.theme), None, ctx.system_theme(), &branding);
        theme::apply(&ctx, &branding, scheme);

        let auth = match api_client() {
            Some(client) => AuthView::Validating { check: SessionCheck::start(client, waker(&ctx)) },
            None => AuthView::LoggedOut { error: None },
        };
        Self {
            state: WindowState::ShowConfig,
            prev_state: WindowState::ShowConfig,
            focus_latch: true,
            quitting: false,
            ask_crash_reports: false,
            wake_rx,
            auth,
            device_config,
            branding,
            account_theme: None,
            applied_scheme: Some(scheme),
            sync: None,
            config_app: ConfigApp::new(),
            home_app: HomeApp::new(),
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

    /// Start the realtime + polling sync stream, unless syncing is paused or one
    /// is already running.
    fn start_sync(&mut self, ctx: &egui::Context) {
        if self.sync.is_some() || !Config::get(|c| c.sync.enabled) {
            return;
        }
        let Some(client) = api_client() else { return };
        let fallback = Config::get(|c| match c.sync.poll {
            PollInterval::Auto => Some(Duration::from_secs(300)),
            PollInterval::Off => None,
            PollInterval::Every(d) => Some(d),
        });
        self.sync = Some(SyncStream::start(client, None, fallback, waker(ctx)));
    }

    /// Drain whatever the sync stream has produced. For now this only logs — the
    /// sync engine that consumes these events isn't built yet.
    fn drain_sync(&mut self) {
        let Some(sync) = &self.sync else { return };
        for event in sync.events() {
            match event {
                SyncEvent::Connected => eprintln!("sync: stream connected"),
                SyncEvent::Disconnected { reason } => eprintln!("sync: stream disconnected ({reason})"),
                SyncEvent::Synced { instances } => eprintln!("sync: hydrated {} instance(s)", instances.len()),
                SyncEvent::Changed { instance_id, latest } => {
                    eprintln!("sync: {instance_id} has save {} ({} bytes, hash {})", latest.id, latest.size_bytes, latest.content_hash);
                }
                SyncEvent::Error { message } => eprintln!("sync: {message}"),
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
            WindowState::ShowHome => self.home_app.reset(),
            WindowState::Hidden => {}
        }
        self.prev_state = self.state;
    }

    fn begin_logout(&mut self, ctx: &egui::Context) {
        if let Some(client) = api_client() {
            api::revoke_in_background(client);
        }
        let _ = Config::update(|c| c.clear_session());
        self.sync = None;
        self.auth = AuthView::LoggedOut { error: None };
        self.focus_latch = true;
        self.ask_crash_reports = false;
        self.account_theme = None;
        self.config_app.reset();
        ctx.request_repaint();
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
            self.ask_crash_reports = false;
        }
    }

    fn hide(&mut self) -> egui::ViewportCommand {
        self.state = WindowState::Hidden;
        self.focus_latch = false;
        egui::ViewportCommand::Visible(false)
    }

    /// The shared title bar for every screen (Config, Home, sign-in): screen
    /// name on the left, a minimise-to-tray button on the right. Hidden while a
    /// native dialog would be up is unnecessary — it's just chrome.
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
                    minimise = ui.button(egui::RichText::new("—").size(14.0)).on_hover_text("Minimise to tray (Esc)").clicked();
                });
            });
        });
        if minimise {
            ui.send_viewport_cmd(self.hide());
        }
    }

    fn show_home(&mut self) -> egui::ViewportCommand {
        self.state = WindowState::ShowHome;
        self.focus_latch = true;
        egui::ViewportCommand::Visible(true)
    }

    fn show_config(&mut self) -> egui::ViewportCommand {
        self.state = WindowState::ShowConfig;
        self.focus_latch = true;
        egui::ViewportCommand::Visible(true)
    }

    fn pump_auth(&mut self, ctx: &egui::Context) {
        enum Next {
            Stay,
            Ready,
            /// Session just saved after a device login — bounce through
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
                    eprintln!("auth: couldn't verify stored session, keeping it: {msg}");
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
                if !Config::get(|c| c.crash_reports_answered()) {
                    self.ask_crash_reports = true;
                }
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
                        ctx.send_viewport_cmd(self.show_home());
                    } else {
                        if self.state.is_config() {
                            self.state = WindowState::ShowHome;
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                TrayIconEvent::Click { button: MouseButton::Right, button_state: MouseButtonState::Down, .. } => {
                    if self.state.hidden() {
                        ctx.send_viewport_cmd(self.show_config());
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
                ctx.send_viewport_cmd(self.show_home());
            } else {
                if self.state.is_home() {
                    self.state = WindowState::ShowConfig;
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        self.pump_auth(ctx);
        self.drain_sync();
        self.sync_shown_screen();

        if self.state.is_config() {
            self.config_app.logic(ctx, frame);
        } else if self.state.is_home() {
            self.home_app.logic(ctx, frame);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.state.hidden() {
            ui.request_repaint_after(Duration::from_millis(250));
            return;
        }

        let busy_auth = matches!(self.auth, AuthView::Connecting { .. } | AuthView::Validating { .. });

        let lost_focus = ui.input(|i| {
            if i.focused {
                self.focus_latch = false;
                false
            } else {
                !self.focus_latch
            }
        });
        if lost_focus && !self.quitting && !self.ask_crash_reports && !busy_auth && Config::get(|c| c.window.hide_on_focus_loss) {
            ui.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ui.send_viewport_cmd(self.hide());
        }

        // Shared title bar (carries the minimise button). Esc is handled at the
        // end of the frame so an open popup or the modal claims it first.
        self.header(ui);

        if matches!(self.auth, AuthView::Ready) {
            if self.ask_crash_reports {
                self.crash_reports_prompt(ui.ctx());
            }

            if self.state.is_config() {
                match self.config_app.ui(ui, frame, &self.branding) {
                    ConfigOutcome::Stay => {}
                    ConfigOutcome::LogOut => self.begin_logout(ui.ctx()),
                    ConfigOutcome::Quit => {
                        self.quitting = true;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            } else if self.state.is_home() {
                self.home_app.ui(ui, frame, &self.branding);
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
        if !busy_auth && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            ui.send_viewport_cmd(self.hide());
        }

        ui.request_repaint_after(Duration::from_millis(100));
    }
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
