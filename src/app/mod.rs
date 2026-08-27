//! The application window itself.
//!
//! It's a small, undecorated, always-on-top panel that hides itself the moment
//! it loses focus, and is re-summoned by clicking the tray icon or by launching
//! the app a second time.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use eframe::egui;
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

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

pub struct App {
    // hidden: bool,
    state: WindowState,
    focus_latch: bool,
    wake_rx: Receiver<()>,
}

impl App {
    pub fn new(wake_rx: Receiver<()>) -> Self {
        Self {
            // hidden: false,
            state: WindowState::ShowConfig,
            focus_latch: true,
            wake_rx,
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
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        let mut wake = false;
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
            wake = true;
        }
        if wake {}
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.state.hidden() {
            ui.request_repaint_after(Duration::from_millis(250));
        } else {
            if ui.input(|i| {
                if i.focused {
                    self.focus_latch = false;
                    false
                } else {
                    !self.focus_latch
                }
            }) {
                ui.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ui.send_viewport_cmd(self.hide());
            };

            egui::CentralPanel::default().show(ui, |ui| {
                ui.heading("My egui Application");
                ui.heading(format!("State: {:?}", self.state));
            });
            ui.request_repaint_after(Duration::from_millis(100));
        }
    }
}
