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

        // Trigger Settings
        egui::CollapsingHeader::new("⚡ Trigger Settings")
            .id_source(format!("trigger_settings_device_{}", idx))
            .show(ui, |ui| {
                self.render_trigger_config(ui, device, idx, notifications);
            });
    }

    fn render_trigger_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        notifications: &mut NotificationManager,
    ) {
        // Trigger Source Selection
        ui.horizontal(|ui| {
            ui.label("Source:");
            let mut source = device.trigger_config.source;
            if ui.radio_value(&mut source, crate::device::TriggerSource::Analog, "Analog").clicked() {
                device.trigger_config.source = source;
                device.trigger_config.analog.enabled = true;
                device.trigger_config.digital.enabled = false;
                notifications.add_info(format!("Trigger source set to Analog for {}", device.name));
            }
            if ui.radio_value(&mut source, crate::device::TriggerSource::Digital, "Digital").clicked() {
                device.trigger_config.source = source;
                device.trigger_config.analog.enabled = false;
                device.trigger_config.digital.enabled = true;
                notifications.add_info(format!("Trigger source set to Digital for {}", device.name));
            }
        });

        ui.separator();

        // Analog Trigger Configuration
        if device.trigger_config.source == crate::device::TriggerSource::Analog {
            self.render_analog_trigger_config(ui, device, idx, notifications);
        }

        // Digital Trigger Configuration
        if device.trigger_config.source == crate::device::TriggerSource::Digital {
            self.render_digital_trigger_config(ui, device, idx, notifications);
        }
    }

    fn render_analog_trigger_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        notifications: &mut NotificationManager,
    ) {
        ui.label(RichText::new("📊 Analog Trigger").strong());

        // Trigger Level
        ui.horizontal(|ui| {
            ui.label("Level:");
            let mut level = device.trigger_config.analog.level as f32;
            if ui.add(egui::Slider::new(&mut level, 0.0..=1.0).suffix("V")).changed() {
                device.trigger_config.analog.level = level as f64;
                notifications.add_info(format!("Analog trigger level set to {:.2}V for {}", level, device.name));
            }
        });

        // Trigger Pattern
        ui.horizontal(|ui| {
            ui.label("Pattern:");
            let mut pattern = device.trigger_config.analog.pattern;
            egui::ComboBox::from_id_source(format!("analog_trigger_pattern_device_{}", idx))
                .selected_text(match pattern {
                    crate::device::AnalogTriggerPattern::Rising => "Rising Edge",
                    crate::device::AnalogTriggerPattern::Falling => "Falling Edge",
                    crate::device::AnalogTriggerPattern::Level => "Level",
                    crate::device::AnalogTriggerPattern::LevelAuto => "Level + Auto",
                })
                .show_ui(ui, |ui| {
                    if ui.selectable_value(&mut pattern, crate::device::AnalogTriggerPattern::Rising, "Rising Edge").clicked() {
                        device.trigger_config.analog.pattern = pattern;
                        notifications.add_info(format!("Analog trigger pattern set to Rising Edge for {}", device.name));
                    }
                    if ui.selectable_value(&mut pattern, crate::device::AnalogTriggerPattern::Falling, "Falling Edge").clicked() {
                        device.trigger_config.analog.pattern = pattern;
                        notifications.add_info(format!("Analog trigger pattern set to Falling Edge for {}", device.name));
                    }
                    if ui.selectable_value(&mut pattern, crate::device::AnalogTriggerPattern::Level, "Level").clicked() {
                        device.trigger_config.analog.pattern = pattern;
                        notifications.add_info(format!("Analog trigger pattern set to Level for {}", device.name));
                    }
                    if ui.selectable_value(&mut pattern, crate::device::AnalogTriggerPattern::LevelAuto, "Level + Auto").clicked() {
                        device.trigger_config.analog.pattern = pattern;
                        notifications.add_info(format!("Analog trigger pattern set to Level + Auto for {}", device.name));
                    }
                });
        });
    }

    fn render_digital_trigger_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        notifications: &mut NotificationManager,
    ) {
        ui.label(RichText::new("💻 Digital Trigger").strong());

        // Trigger Mode
        ui.horizontal(|ui| {
            ui.label("Mode:");
            let mut mode = device.trigger_config.digital.mode;
            egui::ComboBox::from_id_source(format!("digital_trigger_mode_device_{}", idx))
                .selected_text(match mode {
                    crate::device::DigitalTriggerMode::StartMatching => "Start Matching",
                    crate::device::DigitalTriggerMode::StopMatching => "Stop Matching",
                    crate::device::DigitalTriggerMode::WhileMatching => "While Matching",
                    crate::device::DigitalTriggerMode::WhileMatchingAuto => "While Matching + Auto",
                })
                .show_ui(ui, |ui| {
                    if ui.selectable_value(&mut mode, crate::device::DigitalTriggerMode::StartMatching, "Start Matching").clicked() {
                        device.trigger_config.digital.mode = mode;
                        notifications.add_info(format!("Digital trigger mode set to Start Matching for {}", device.name));
                    }
                    if ui.selectable_value(&mut mode, crate::device::DigitalTriggerMode::StopMatching, "Stop Matching").clicked() {
                        device.trigger_config.digital.mode = mode;
                        notifications.add_info(format!("Digital trigger mode set to Stop Matching for {}", device.name));
                    }
                    if ui.selectable_value(&mut mode, crate::device::DigitalTriggerMode::WhileMatching, "While Matching").clicked() {
                        device.trigger_config.digital.mode = mode;
                        notifications.add_info(format!("Digital trigger mode set to While Matching for {}", device.name));
                    }
                    if ui.selectable_value(&mut mode, crate::device::DigitalTriggerMode::WhileMatchingAuto, "While Matching + Auto").clicked() {
                        device.trigger_config.digital.mode = mode;
                        notifications.add_info(format!("Digital trigger mode set to While Matching + Auto for {}", device.name));
                    }
                });
        });

        // Bit Pattern Configuration
        ui.label("Bit Pattern:");
        ui.horizontal(|ui| {
            ui.label("Channel:");
            for ch in 0..9 {
                ui.label(format!("D{}", ch));
            }
        });
        
        ui.horizontal(|ui| {
            ui.label("Pattern:");
            for ch in 0..9 {
                let bit_state = device.trigger_config.digital.bit_pattern[ch];
                let button_color = match bit_state {
                    crate::device::DigitalBitState::DontCare => egui::Color32::GRAY,
                    crate::device::DigitalBitState::Low => egui::Color32::RED,
                    crate::device::DigitalBitState::High => egui::Color32::GREEN,
                };
                
                if ui.button(RichText::new(bit_state.as_str()).color(button_color)).clicked() {
                    device.trigger_config.digital.bit_pattern[ch] = bit_state.cycle();
                    let new_state = device.trigger_config.digital.bit_pattern[ch];
                    notifications.add_info(format!("Digital trigger D{} set to {} for {}", ch, new_state.as_str(), device.name));
                }
            }
        });

        // Pattern Preview
        ui.horizontal(|ui| {
            ui.label("Current pattern:");
            let pattern_str: String = device.trigger_config.digital.bit_pattern.iter()
                .map(|bit| bit.as_str())
                .collect::<Vec<_>>()
                .join("");
            ui.code(pattern_str);
            
            if ui.small_button("Clear All").clicked() {
                device.trigger_config.digital.bit_pattern = [crate::device::DigitalBitState::DontCare; 9];
                notifications.add_info(format!("Digital trigger pattern cleared for {}", device.name));
            }
        });
    }
}
