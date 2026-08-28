use eframe::egui;

use crate::api::Branding;

pub struct HomeApp {}

impl HomeApp {
    pub fn new() -> Self {
        HomeApp {}
    }
    /// Return to a clean default state. Called each time the window is shown as Home.
    pub fn reset(&mut self) {}
    pub fn logic(&mut self, _ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {}
    pub fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame, branding: &Branding) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Home");

            let identity = &branding.identity;
            if !identity.attribution_text.is_empty() {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(6.0);
                    ui.hyperlink_to(egui::RichText::new(&identity.attribution_text).weak().small(), &identity.homepage_url);
                });
            }
        });
    }
}
