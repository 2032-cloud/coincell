use eframe::egui;

pub struct HomeApp {}

impl HomeApp {
    pub fn new() -> Self {
        HomeApp {}
    }
    pub fn logic(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {}
    pub fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Home");
        });
    }
}
