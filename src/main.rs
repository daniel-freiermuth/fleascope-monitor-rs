use eframe::egui;

mod control_panel;
mod device;
mod device_worker;
mod notifications;
mod plot_area;
mod worker_interface;

use control_panel::ControlPanel;
use device::DeviceManager;
use notifications::NotificationManager;
use plot_area::PlotArea;

#[derive(Default)]
pub struct FleaScopeApp {
    device_manager: DeviceManager,
    plot_area: PlotArea,
    control_panel: ControlPanel,
    notification_manager: NotificationManager,
}

impl FleaScopeApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {

        // Customize egui here with cc.egui_ctx.set_fonts and cc.egui_ctx.set_visuals.
        // Restore app state using cc.storage (requires the "persistence" feature).
        Self::default()
    }
}

impl eframe::App for FleaScopeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        profiling::finish_frame!();

        // Update notifications (remove expired ones)
        profiling::scope!("notification_update");
        self.notification_manager.update();

        // Request repaint for real-time updates
        ui.ctx().request_repaint();

        // Top menu bar
        egui::Panel::top("top_panel").show_inside(ui, |ui| {
            profiling::scope!("top_menu_bar");
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Exit").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button("View", |ui| {
                    if ui.button("Reset Layout").clicked() {
                        // Reset to default layout
                    }
                });

                ui.menu_button("Help", |ui| {
                    if ui.button("Demo Notifications").clicked() {
                        self.notification_manager
                            .add_info("This is an info notification");
                        self.notification_manager
                            .add_success("Operation completed successfully!");
                        self.notification_manager
                            .add_error("This is an error notification");
                    }
                    ui.separator();
                    if ui.button("About").clicked() {
                        self.notification_manager.add_info(
                            "FleaScope Live Oscilloscope v0.1.0\nBuilt with Rust and egui",
                        );
                    }
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("🔬 FleaScope Live Oscilloscope");
                });
            });
        });

        // Status bar
        egui::Panel::bottom("status_panel").show_inside(ui, |ui| {
            profiling::scope!("status_bar");
            ui.horizontal(|ui| {
                ui.label("Status: Ready");
                ui.separator();

                ui.label(format!("Devices: {}", self.device_manager.get_devices().len()));
                ui.separator();
                ui.label(format!("FPS: {:.1}", ui.input(|i| i.stable_dt).recip()));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("🚀 Rust GUI");
                });
            });
        });

        // Main content area
        egui::CentralPanel::default().show_inside(ui, |ui| {
            profiling::scope!("main_content");
            // Use available space more efficiently
            let available_rect = ui.available_rect_before_wrap();
            let control_width = 300.0;
            let plot_width = available_rect.width() - control_width;

            ui.horizontal(|ui| {
                // Left side - Plot area (takes most of the space)
                ui.allocate_ui_with_layout(
                    [plot_width, available_rect.height()].into(),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        profiling::scope!("plot_area");
                        // Use full available height for plots
                        ui.set_min_height(available_rect.height());

                        self.plot_area.ui(ui, &mut self.device_manager);
                    },
                );

                ui.separator();

                // Right side - Control panel (rack-style)
                ui.allocate_ui_with_layout(
                    [control_width, available_rect.height()].into(),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        profiling::scope!("control_panel");
                        // Use full available height for control panel
                        ui.set_min_height(available_rect.height());

                        self.control_panel.ui(
                            ui,
                            &mut self.device_manager,
                            &mut self.notification_manager,
                        );
                    },
                );
            });
        });

        // Render notifications (always last, so they appear on top)
        profiling::scope!("notifications_render");
        self.notification_manager.ui(ui.ctx());
    }
}

#[tokio::main]
async fn main() -> eframe::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Register main thread for profiling
    profiling::register_thread!("Main Thread");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("FleaScope Live Oscilloscope")
            .with_icon(eframe::icon_data::from_png_bytes(&[]).unwrap_or_default()),
        ..Default::default()
    };

    eframe::run_native(
        "FleaScope Live Oscilloscope",
        options,
        Box::new(|cc| {
            // This gives us image support:
            egui_extras::install_image_loaders(&cc.egui_ctx);

            Ok(Box::new(FleaScopeApp::new(cc)))
        }),
    )
}
