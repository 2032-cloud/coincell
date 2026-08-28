use eframe::egui;

pub struct ConfigApp {}

impl ConfigApp {
    pub fn new() -> Self {
        ConfigApp {}
    }
    pub fn logic(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {}
    pub fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Config");
        });
    }
}
