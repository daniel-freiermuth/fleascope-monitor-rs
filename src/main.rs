use eframe::egui;
use std::sync::Arc;
use tokio::sync::Mutex;

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
    device_manager: Arc<Mutex<DeviceManager>>,
    plot_area: PlotArea,
    control_panel: ControlPanel,
    notification_manager: NotificationManager,
}

impl FleaScopeApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        // Customize egui here with cc.egui_ctx.set_fonts and cc.egui_ctx.set_visuals.
        // Restore app state using cc.storage (requires the "persistence" feature).
        let mut app = Self::default();
        
        // Add a default device named "scope3" automatically
        if let Ok(mut device_manager) = app.device_manager.try_lock() {
            if let Err(e) = device_manager.add_device("scope3".to_string()) {
                tracing::warn!("Failed to add default device: {}", e);
            } else {
                tracing::info!("Added default device 'scope3' automatically");
            }
        }
        
        app
    }
}

impl eframe::App for FleaScopeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Update notifications (remove expired ones)
        self.notification_manager.update();
        
        // Request repaint for real-time updates
        ctx.request_repaint();

        // Top menu bar
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button("View", |ui| {
                    if ui.button("Reset Layout").clicked() {
                        // Reset to default layout
                    }
                });

                ui.menu_button("Help", |ui| {
                    if ui.button("Demo Notifications").clicked() {
                        self.notification_manager.add_info("This is an info notification");
                        self.notification_manager.add_success("Operation completed successfully!");
                        self.notification_manager.add_error("This is an error notification");
                    }
                    ui.separator();
                    if ui.button("About").clicked() {
                        self.notification_manager.add_info("FleaScope Live Oscilloscope v0.1.0\nBuilt with Rust and egui");
                    }
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("🔬 FleaScope Live Oscilloscope");
                });
            });
        });

        // Status bar
        egui::TopBottomPanel::bottom("status_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Status: Ready");
                ui.separator();

                // Get device count safely
                let device_count = {
                    if let Ok(manager) = self.device_manager.try_lock() {
                        manager.get_devices().len()
                    } else {
                        0
                    }
                };

                ui.label(format!("Devices: {}", device_count));
                ui.separator();
                ui.label(format!("FPS: {:.1}", ctx.input(|i| i.stable_dt).recip()));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("🚀 Rust GUI");
                });
            });
        });

        // Main content area
        egui::CentralPanel::default().show(ctx, |ui| {
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
                        // Use full available height for plots
                        ui.set_min_height(available_rect.height());

                        // Access device manager safely for plotting
                        if let Ok(manager) = self.device_manager.try_lock() {
                            self.plot_area.ui(ui, &manager);
                        } else {
                            ui.label("Loading devices...");
                        }
                    },
                );

                ui.separator();

                // Right side - Control panel (rack-style)
                ui.allocate_ui_with_layout(
                    [control_width, available_rect.height()].into(),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        // Use full available height for control panel
                        ui.set_min_height(available_rect.height());

                        // Access device manager safely for control panel
                        if let Ok(mut manager) = self.device_manager.try_lock() {
                            self.control_panel.ui(ui, &mut manager, &mut self.notification_manager);
                        } else {
                            ui.label("Loading control panel...");
                        }
                    },
                );
            });
        });

        // Render notifications (always last, so they appear on top)
        self.notification_manager.ui(ctx);
    }
}

#[tokio::main]
async fn main() -> eframe::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

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
