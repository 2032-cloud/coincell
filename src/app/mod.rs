mod config;
mod home;

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use eframe::egui;
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

use crate::app::config::ConfigApp;
use crate::app::home::HomeApp;
use crate::auth::{self, AuthEvent, AuthFlow, SessionCheck, SessionStatus};
use crate::config::Config;

#[derive(Debug)]
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

    Connecting { flow: AuthFlow, approval: Option<Approval> },

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
    focus_latch: bool,
    wake_rx: Receiver<()>,
    auth: AuthView,
    config_app: ConfigApp,
    home_app: HomeApp,
}

impl App {
    pub fn new(ctx: egui::Context, wake_rx: Receiver<()>) -> Self {
        let auth = match Config::get(|c| c.session_id()) {
            Some(session_id) => AuthView::Validating {
                check: SessionCheck::start(ctx, session_id),
            },
            None => AuthView::LoggedOut { error: None },
        };
        Self {
            state: WindowState::ShowConfig,
            focus_latch: true,
            wake_rx,
            auth,
            config_app: ConfigApp::new(),
            home_app: HomeApp::new(),
        }
    }

    fn hide(&mut self) -> egui::ViewportCommand {
        self.state = WindowState::Hidden;
        self.focus_latch = false;
        egui::ViewportCommand::Visible(false)
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

    fn pump_auth(&mut self) {
        enum Next {
            Stay,
            Ready,
            LoggedOut(Option<Arc<str>>),
        }

        let next = match &mut self.auth {
            AuthView::Connecting { flow, approval } => {
                let mut done: Option<Arc<str>> = None;
                let mut failed: Option<Arc<str>> = None;
                for event in flow.events() {
                    match event {
                        AuthEvent::AwaitingApproval {
                            user_code,
                            verification_uri,
                            verification_uri_complete,
                        } => {
                            *approval = Some(Approval {
                                user_code,
                                verification_uri,
                                verification_uri_complete,
                            });
                        }
                        AuthEvent::Completed { session_id } => done = Some(session_id),
                        AuthEvent::Failed { message } => failed = Some(message),
                    }
                }

                if let Some(session_id) = done {
                    match Config::update(|c| c.set_session(session_id)) {
                        Ok(()) => Next::Ready,
                        Err(e) => Next::LoggedOut(Some(Arc::from(format!("couldn't save login: {e:#}")))),
                    }
                } else if let Some(message) = failed {
                    Next::LoggedOut(Some(message))
                } else {
                    Next::Stay
                }
            }
            AuthView::Validating { check } => match check.poll() {
                Some(SessionStatus::Valid) => Next::Ready,
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
            }
            Next::LoggedOut(error) => {
                self.auth = AuthView::LoggedOut { error };
                self.focus_latch = true;
            }
        }
    }

    fn login_screen(&self, ui: &mut egui::Ui) -> LoginIntent {
        let mut intent = LoginIntent::None;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.3).max(0.0));
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
        });
        intent
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Down,
                    ..
                } => {
                    if self.state.hidden() {
                        ctx.send_viewport_cmd(self.show_home());
                    } else {
                        if self.state.is_config() {
                            self.state = WindowState::ShowHome;
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                TrayIconEvent::Click {
                    button: MouseButton::Right,
                    button_state: MouseButtonState::Down,
                    ..
                } => {
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

        self.pump_auth();

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

        if ui.input(|i| {
            if i.focused {
                self.focus_latch = false;
                false
            } else {
                !self.focus_latch
            }
        }) && !matches!(self.auth, AuthView::Connecting { .. } | AuthView::Validating { .. })
        {
            ui.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ui.send_viewport_cmd(self.hide());
        };

        if matches!(self.auth, AuthView::Ready) {
            if self.state.is_config() {
                self.config_app.ui(ui, frame);
            } else if self.state.is_home() {
                self.home_app.ui(ui, frame);
            }
        } else {
            match self.login_screen(ui) {
                LoginIntent::StartFlow => {
                    self.auth = AuthView::Connecting {
                        flow: AuthFlow::start(ui.ctx().clone()),
                        approval: None,
                    };
                }
                LoginIntent::ReopenBrowser => {
                    if let AuthView::Connecting { approval: Some(approval), .. } = &self.auth {
                        auth::open_in_browser(&approval.verification_uri_complete);
                    }
                }
                LoginIntent::None => {}
            }
        }

        ui.request_repaint_after(Duration::from_millis(100));
    }
}
