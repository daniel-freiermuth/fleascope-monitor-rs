use crate::device::{DeviceManager, FleaScopeDevice};
use crate::notifications::NotificationManager;
use egui::{Color32, RichText};

#[derive(Default)]
pub struct ControlPanel {
    new_device_hostname: String,
    show_add_device: bool,
}

impl ControlPanel {
    pub fn ui(&mut self, ui: &mut egui::Ui, device_manager: &mut DeviceManager, notifications: &mut NotificationManager) {
        ui.heading("🎛️ Control Panel");

        ui.separator();

        // Add Device Section
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Add Device").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("➕").clicked() {
                        self.show_add_device = !self.show_add_device;
                    }
                });
            });

            if self.show_add_device {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Hostname:");
                    ui.text_edit_singleline(&mut self.new_device_hostname);
                });

                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() && !self.new_device_hostname.is_empty() {
                        match device_manager.add_device(self.new_device_hostname.clone()) {
                            Ok(_) => {
                                notifications.add_success(format!("Connected to device: {}", self.new_device_hostname));
                                self.new_device_hostname.clear();
                                self.show_add_device = false;
                            }
                            Err(e) => {
                                notifications.add_error(format!("Failed to connect to {}: {}", self.new_device_hostname, e));
                                tracing::error!("Failed to add device: {}", e);
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_add_device = false;
                        self.new_device_hostname.clear();
                    }
                });

                ui.separator();
                ui.label("💡 Quick Add:");
                ui.horizontal_wrapped(|ui| {
                    for hostname in ["scope-001", "scope-002", "scope-003", "localhost:8080"] {
                        if ui.small_button(hostname).clicked() {
                            match device_manager.add_device(hostname.to_string()) {
                                Ok(_) => {
                                    notifications.add_success(format!("Connected to device: {}", hostname));
                                }
                                Err(e) => {
                                    notifications.add_error(format!("Failed to connect to {}: {}", hostname, e));
                                    tracing::error!("Failed to add device: {}", e);
                                }
                            }
                        }
                    }
                });
            }
        });

        ui.add_space(10.0);

        // Device Rack Section
        ui.group(|ui| {
            ui.label(RichText::new("Device Rack").strong());
            ui.separator();

            if device_manager.get_devices().is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);
                    ui.label(RichText::new("No devices").weak());
                    ui.label("Click ➕ to add a device");
                    ui.add_space(20.0);
                });
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false]) // Don't shrink, use full available space
                    .max_height(ui.available_height() - 100.0)
                    .show(ui, |ui| {
                        // Set minimum width to prevent clipping
                        ui.set_min_width(ui.available_width());

                        let mut to_remove = None;

                        for (idx, device) in device_manager.get_devices_mut().iter_mut().enumerate() {
                            ui.group(|ui| {
                                self.render_device_rack(ui, device, idx, &mut to_remove, notifications);
                            });
                            ui.add_space(5.0);
                        }

                        if let Some(idx) = to_remove {
                            let device_name = device_manager.get_devices().get(idx).map(|d| d.name.clone()).unwrap_or_else(|| "Unknown".to_string());
                            notifications.add_info(format!("Removed device: {}", device_name));
                            let _ = device_manager.remove_device(idx);
                        }
                    });
            }
        });
    }

    fn render_device_rack(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        to_remove: &mut Option<usize>,
        notifications: &mut NotificationManager,
    ) {
        ui.horizontal(|ui| {
            // Connection status indicator
            let status_color = if device.is_connected() {
                Color32::GREEN
            } else {
                Color32::RED
            };
            ui.colored_label(status_color, "●");

            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&device.name).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("🗑")
                            .on_hover_text("Remove device")
                            .clicked()
                        {
                            *to_remove = Some(idx);
                        }
                    });
                });

                ui.horizontal(|ui| {
                    ui.label(format!("📡 {}", device.hostname));
                    let status_text = if device.is_connected() {
                        "Connected"
                    } else {
                        "Disconnected"
                    };
                    ui.label(RichText::new(status_text).color(status_color).small());
                });
            });
        });

        ui.separator();

        // Channel Configuration
        ui.label(RichText::new("Channel Configuration").small().strong());

        // Analog Channel
        ui.horizontal(|ui| {
            let mut enabled = device.enabled_channels[0];
            if ui.checkbox(&mut enabled, "").clicked() {
                device.enabled_channels[0] = enabled;
                let status = if enabled { "enabled" } else { "disabled" };
                notifications.add_info(format!("Analog channel {} for {}", status, device.name));
            }
            ui.label("📊 Analog (12-bit)");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new("0-4095").small().weak());
            });
        });

        // Digital Channels
        ui.label(RichText::new("Digital Channels").small());
        egui::Grid::new(format!("digital_channels_device_{}", idx))
            .num_columns(3)
            .spacing([10.0, 2.0])
            .show(ui, |ui| {
                for ch in 0..9 {
                    let mut enabled = device.enabled_channels[ch + 1];
                    if ui.checkbox(&mut enabled, "").clicked() {
                        device.enabled_channels[ch + 1] = enabled;
                        let status = if enabled { "enabled" } else { "disabled" };
                        notifications.add_info(format!("Digital channel D{} {} for {}", ch, status, device.name));
                    }
                    ui.label(format!("D{}", ch));

                    if (ch + 1) % 3 == 0 {
                        ui.end_row();
                    }
                }
            });

        ui.separator();

        // Device Configuration
        ui.label(RichText::new("Device Configuration").small().strong());
        
        // Time Frame Control
        ui.horizontal(|ui| {
            ui.label("⏱️ Time Window:");
            let mut time_frame = device.time_frame as f32;
            if ui.add(egui::Slider::new(&mut time_frame, 0.1..=10.0).suffix("s")).changed() {
                device.time_frame = time_frame as f64;
                notifications.add_info(format!("Time window changed to {:.1}s for {}", time_frame, device.name));
            }
        });

        // Pause/Resume Control
        ui.horizontal(|ui| {
            let is_paused = device.is_paused();
            let button_text = if is_paused { "▶️ Resume" } else { "⏸️ Pause" };
            let button_color = if is_paused { egui::Color32::GREEN } else { egui::Color32::YELLOW };
            
            if ui.button(RichText::new(button_text).color(button_color)).clicked() {
                if is_paused {
                    device.resume();
                    notifications.add_success(format!("Resumed data acquisition for {}", device.name));
                } else {
                    device.pause();
                    notifications.add_info(format!("Paused data acquisition for {}", device.name));
                }
            }
            
            ui.label(if is_paused { 
                RichText::new("Paused").color(egui::Color32::YELLOW)
            } else { 
                RichText::new("Running").color(egui::Color32::GREEN)
            });
        });

        // Probe Selection
        ui.horizontal(|ui| {
            ui.label("🔍 Probe:");
            let mut current_probe = device.probe_multiplier;
            egui::ComboBox::from_id_source(format!("probe_selector_device_{}", idx))
                .selected_text(current_probe.as_str())
                .show_ui(ui, |ui| {
                    if ui.selectable_value(&mut current_probe, crate::device::ProbeMultiplier::X1, "x1").clicked() {
                        device.probe_multiplier = current_probe;
                        notifications.add_info(format!("Probe set to x1 for {}", device.name));
                    }
                    if ui.selectable_value(&mut current_probe, crate::device::ProbeMultiplier::X10, "x10").clicked() {
                        device.probe_multiplier = current_probe;
                        notifications.add_info(format!("Probe set to x10 for {}", device.name));
                    }
                });
            ui.label(RichText::new(format!("({}x amplification)", current_probe.get_factor())).small().weak());
        });

        ui.separator();

        // Device Statistics
        if device.is_connected() {
            let data_guard = device.data.lock().unwrap();
            let update_age = data_guard.last_update.elapsed().as_millis();
            drop(data_guard);

            ui.horizontal(|ui| {
                ui.label("📈 Sample Rate:");
                ui.label("1000 Hz");
            });
            ui.horizontal(|ui| {
                ui.label("🔄 Last Update:");
                ui.label(format!("{}ms ago", update_age));
            });
            ui.horizontal(|ui| {
                ui.label("📦 Buffer:");
                ui.label("2000 samples");
            });
        }

        // Trigger Settings (Placeholder)
        egui::CollapsingHeader::new("⚡ Trigger Settings")
            .id_source(format!("trigger_settings_device_{}", idx))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Mode:");
                    let mut trigger_mode = "Auto".to_string();
                    egui::ComboBox::from_id_source(format!("trigger_mode_device_{}", idx))
                        .selected_text(&trigger_mode)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut trigger_mode, "Auto".to_string(), "Auto");
                            ui.selectable_value(&mut trigger_mode, "Normal".to_string(), "Normal");
                            ui.selectable_value(&mut trigger_mode, "Single".to_string(), "Single");
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("Level:");
                    let mut trigger_level = 0.5f32;
                    ui.add(egui::Slider::new(&mut trigger_level, 0.0..=1.0).suffix("V"));
                });

                ui.horizontal(|ui| {
                    ui.label("Slope:");
                    let mut trigger_slope = "Rising".to_string();
                    egui::ComboBox::from_id_source(format!("trigger_slope_device_{}", idx))
                        .selected_text(&trigger_slope)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut trigger_slope, "Rising".to_string(), "Rising");
                            ui.selectable_value(&mut trigger_slope, "Falling".to_string(), "Falling");
                        });
                });
            });
    }
}
