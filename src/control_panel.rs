use crate::device::{DeviceManager, FleaScopeDevice, MIN_TIME_FRAME, MAX_TIME_FRAME};
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
                    for hostname in ["scope1", "scope-002", "scope3", "localhost:8080"] {
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
        // Device Header - Compact
        ui.horizontal(|ui| {
            // let status_color = if device.is_connected() { Color32::GREEN } else { Color32::RED };
            let status_color = Color32::GREEN;
            ui.colored_label(status_color, "●");
            ui.label(RichText::new(&device.name).strong().size(14.0));
            
            // Waveform indicator
            if device.waveform_config.enabled {
                ui.label(RichText::new(device.waveform_config.waveform_type.icon()).size(12.0).color(Color32::LIGHT_BLUE));
            }
            
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Remove").clicked() {
                    *to_remove = Some(idx);
                }
            });
        });

        ui.separator();

        // Channel Controls - Compact Grid
        ui.label(RichText::new("CHANNELS").size(11.0).strong());
        ui.horizontal(|ui| {
            // Analog Channel
            let mut analog_enabled = device.enabled_channels[0];
            if ui.toggle_value(&mut analog_enabled, "A").on_hover_text("Analog Channel").clicked() {
                let mut new_channels = device.enabled_channels;
                new_channels[0] = analog_enabled;
                device.set_enabled_channels(new_channels);
            }
            
            // Digital Channels - Compact
            for ch in 0..9 {
                let mut enabled = device.enabled_channels[ch + 1];
                if ui.toggle_value(&mut enabled, &format!("{}", ch)).on_hover_text(&format!("Digital Channel D{}", ch)).clicked() {
                    let mut new_channels = device.enabled_channels;
                    new_channels[ch + 1] = enabled;
                    device.set_enabled_channels(new_channels);
                }
            }
        });

        // Device Configuration - Compact
        ui.add_space(1.0);
        ui.label(RichText::new("CONFIG").size(11.0).strong());
        
        // Time Window & Controls Row - Uses quadratic scaling for better small value control
        ui.horizontal(|ui| {
            ui.label("⏱");
            
            // Convert actual time to quadratic scale (0.0 to 1.0)
            // This makes it easier to select small time values (122μs - 3.49s range)
            let min_time = MIN_TIME_FRAME; // 122μs
            let max_time = MAX_TIME_FRAME; // 3.49s
            let current_time = device.time_frame.clamp(min_time, max_time);
            let quadratic_value = ((current_time - min_time) / (max_time - min_time)).sqrt() as f32;
            
            let mut slider_value = quadratic_value;
            if ui.add(egui::Slider::new(&mut slider_value, 0.0..=1.0)
                .show_value(true)
                .custom_formatter(|v, _| {
                    // Convert quadratic scale back to time for display
                    let normalized = v * v; // Square to get quadratic scale
                    let time_val = min_time + normalized as f64 * (max_time - min_time);
                    if time_val < 0.001 {
                        format!("{:.0}μs", time_val * 1_000_000.0)
                    } else if time_val < 1.0 {
                        format!("{:.1}ms", time_val * 1000.0)
                    } else {
                        format!("{:.2}s", time_val)
                    }
                })).changed() {
                
                // Convert quadratic slider value back to actual time
                let normalized = slider_value * slider_value; // Square to get quadratic scale
                let new_time = min_time + normalized as f64 * (max_time - min_time);
                device.set_time_frame(new_time);
            }
            
            // Quick time presets
            if ui.small_button("📐").on_hover_text("Time Presets").clicked() {
                // This could open a popup with presets, for now just cycle through common values
                let presets = [MIN_TIME_FRAME, 0.001, 0.01, 0.1, 1.0, MAX_TIME_FRAME]; // 122μs, 1ms, 10ms, 100ms, 1s, 3.49s
                let current_idx = presets.iter().position(|&x| (x - device.time_frame).abs() < 0.0001);
                let next_idx = match current_idx {
                    Some(idx) => (idx + 1) % presets.len(),
                    None => 0,
                };
                device.set_time_frame(presets[next_idx]);
            }
        });

        ui.horizontal(|ui| {
            // Pause/Resume - Toggle Button
            let is_paused = device.is_paused();
            let mut paused_state = is_paused;
            if ui.toggle_value(&mut paused_state, if is_paused { "⏸" } else { "▶" }).on_hover_text(if is_paused { "Resume" } else { "Pause" }).clicked() {
                if paused_state != is_paused {
                    if is_paused {
                        device.resume();
                    } else {
                        device.pause();
                    }
                }
            }

            // Probe Selection - Single Toggle Button
            ui.label("🔍");
            let is_x10 = device.probe_multiplier == crate::device::ProbeMultiplier::X10;
            let probe_text = if is_x10 { "x10" } else { "x1" };
            
            if ui.toggle_value(&mut false, probe_text).clicked() {
                let new_probe = if is_x10 { 
                    crate::device::ProbeMultiplier::X1 
                } else { 
                    crate::device::ProbeMultiplier::X10 
                };
                device.set_probe_multiplier(new_probe);
            }
        });

        // Trigger Settings - Very Compact
        ui.add_space(1.0);
        egui::CollapsingHeader::new(RichText::new("⚡ TRIGGER").size(11.0).strong())
            .id_source(format!("trigger_device_{}", idx))
            .default_open(false)
            .show(ui, |ui| {
                self.render_compact_trigger_config(ui, device, idx, notifications);
            });

        // Waveform Generator - Very Compact
        ui.add_space(1.0);
        egui::CollapsingHeader::new(RichText::new("🌊 WAVEFORM").size(11.0).strong())
            .id_source(format!("waveform_device_{}", idx))
            .default_open(false)
            .show(ui, |ui| {
                self.render_waveform_config(ui, device, idx, notifications);
            });

        // Statistics - Minimal
        ui.add_space(1.0);
        
        // Use ArcSwap load for always-smooth, lock-free data access
        let data = device.data.load();
        let update_age = data.last_update.elapsed().as_millis();
        
        ui.horizontal(|ui| {
            ui.label(RichText::new(&format!("📊 {}ms. {:.2} Hz", update_age, data.update_rate)).size(9.0).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if device.waveform_config.enabled {
                    let freq_str = if device.waveform_config.frequency_hz >= 1000.0 {
                        format!("{:.1}k", device.waveform_config.frequency_hz / 1000.0)
                    } else {
                        format!("{:.0}", device.waveform_config.frequency_hz)
                    };
                    ui.label(RichText::new(&format!("🌊{}", freq_str)).size(9.0).color(Color32::LIGHT_BLUE));
                }
                ui.label(RichText::new("1kHz").size(9.0).weak());
            });
        });
    }

    fn render_compact_trigger_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        notifications: &mut NotificationManager,
    ) {
        // Source Selection - Toggle Buttons
        ui.horizontal(|ui| {
            ui.label("SRC:");
            let is_analog = device.trigger_config.source == crate::device::TriggerSource::Analog;
            let is_digital = device.trigger_config.source == crate::device::TriggerSource::Digital;
            
            if ui.selectable_label(is_analog, "📊").on_hover_text("Analog Trigger").clicked() {
                let mut new_config = device.trigger_config.clone();
                new_config.source = crate::device::TriggerSource::Analog;
                device.set_trigger_config(new_config);
            }
            if ui.selectable_label(is_digital, "💻").on_hover_text("Digital Trigger").clicked() {
                let mut new_config = device.trigger_config.clone();
                new_config.source = crate::device::TriggerSource::Digital;
                device.set_trigger_config(new_config);
            }
        });

        ui.add_space(1.0);

        // Analog Trigger - Compact
        if device.trigger_config.source == crate::device::TriggerSource::Analog {
            // Level Slider
            ui.horizontal(|ui| {
                ui.label("LVL:");
                let mut level = device.trigger_config.analog.level as f32;
                if ui.add(egui::Slider::new(&mut level, -6.6..=6.6).suffix("V").show_value(false).custom_formatter(|v, _| format!("{:.2}V", v))).changed() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.analog.level = level as f64;
                    device.set_trigger_config(new_config);
                    notifications.add_info(format!("Level: {:.2}V - {}", level, device.name));
                }
            });

            // Pattern Buttons
            ui.horizontal(|ui| {
                ui.label("PAT:");
                let pattern = device.trigger_config.analog.pattern;
                
                let is_rising = pattern == crate::device::AnalogTriggerPattern::Rising;
                let is_falling = pattern == crate::device::AnalogTriggerPattern::Falling;
                let is_level = pattern == crate::device::AnalogTriggerPattern::Level;
                let is_auto = pattern == crate::device::AnalogTriggerPattern::LevelAuto;
                
                if ui.selectable_label(is_rising, "↗").on_hover_text("Rising Edge").clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.analog.pattern = crate::device::AnalogTriggerPattern::Rising;
                    device.set_trigger_config(new_config);
                }
                if ui.selectable_label(is_falling, "↘").on_hover_text("Falling Edge").clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.analog.pattern = crate::device::AnalogTriggerPattern::Falling;
                    device.set_trigger_config(new_config);
                }
                if ui.selectable_label(is_level, "─").on_hover_text("Level").clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.analog.pattern = crate::device::AnalogTriggerPattern::Level;
                    device.set_trigger_config(new_config);
                }
                if ui.selectable_label(is_auto, "⟲").on_hover_text("Level + Auto").clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.analog.pattern = crate::device::AnalogTriggerPattern::LevelAuto;
                    device.set_trigger_config(new_config);
                }
            });
        }

        // Digital Trigger - Compact
        if device.trigger_config.source == crate::device::TriggerSource::Digital {
            // Mode Buttons
            ui.horizontal(|ui| {
                ui.label("MOD:");
                let mode = device.trigger_config.digital.mode;
                
                let is_start = mode == crate::device::DigitalTriggerMode::StartMatching;
                let is_stop = mode == crate::device::DigitalTriggerMode::StopMatching;
                let is_while = mode == crate::device::DigitalTriggerMode::WhileMatching;
                let is_auto = mode == crate::device::DigitalTriggerMode::WhileMatchingAuto;
                
                if ui.selectable_label(is_start, "▶").on_hover_text("Start Matching").clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.digital.mode = crate::device::DigitalTriggerMode::StartMatching;
                    device.set_trigger_config(new_config);
                }
                if ui.selectable_label(is_stop, "⏹").on_hover_text("Stop Matching").clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.digital.mode = crate::device::DigitalTriggerMode::StopMatching;
                    device.set_trigger_config(new_config);
                }
                if ui.selectable_label(is_while, "⏸").on_hover_text("While Matching").clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.digital.mode = crate::device::DigitalTriggerMode::WhileMatching;
                    device.set_trigger_config(new_config);
                }
                if ui.selectable_label(is_auto, "⟲").on_hover_text("While + Auto").clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.digital.mode = crate::device::DigitalTriggerMode::WhileMatchingAuto;
                    device.set_trigger_config(new_config);
                }
            });

            // Bit Pattern - Two Rows
            ui.label(RichText::new("BIT:").size(10.0));
            ui.horizontal(|ui| {
                for ch in 0..5 {
                    let bit_state = device.trigger_config.digital.bit_pattern[ch];
                    let (text, color) = match bit_state {
                        crate::device::DigitalBitState::DontCare => ("X", Color32::GRAY),
                        crate::device::DigitalBitState::Low => ("0", Color32::LIGHT_RED),
                        crate::device::DigitalBitState::High => ("1", Color32::LIGHT_GREEN),
                    };
                    
                    if ui.small_button(RichText::new(text).color(color)).on_hover_text(&format!("D{}", ch)).clicked() {
                        let mut new_config = device.trigger_config.clone();
                        new_config.digital.bit_pattern[ch] = bit_state.cycle();
                        device.set_trigger_config(new_config.clone());
                    }
                }
            });
            ui.horizontal(|ui| {
                for ch in 5..9 {
                    let bit_state = device.trigger_config.digital.bit_pattern[ch];
                    let (text, color) = match bit_state {
                        crate::device::DigitalBitState::DontCare => ("X", Color32::GRAY),
                        crate::device::DigitalBitState::Low => ("0", Color32::LIGHT_RED),
                        crate::device::DigitalBitState::High => ("1", Color32::LIGHT_GREEN),
                    };
                    
                    if ui.small_button(RichText::new(text).color(color)).on_hover_text(&format!("D{}", ch)).clicked() {
                        let mut new_config = device.trigger_config.clone();
                        new_config.digital.bit_pattern[ch] = bit_state.cycle();
                        device.set_trigger_config(new_config.clone());
                    }
                }
                // Clear button
                if ui.small_button("CLR").on_hover_text("Clear All").clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.digital.bit_pattern = [crate::device::DigitalBitState::DontCare; 9];
                    device.set_trigger_config(new_config);
                }
            });
        }
    }

    fn render_trigger_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        notifications: &mut NotificationManager,
    ) {
        // Trigger Source Selection - Toggle Buttons
        ui.horizontal(|ui| {
            ui.label("SRC:");
            let is_analog = device.trigger_config.source == crate::device::TriggerSource::Analog;
            let is_digital = device.trigger_config.source == crate::device::TriggerSource::Digital;
            
            if ui.selectable_label(is_analog, "📊").on_hover_text("Analog Trigger").clicked() {
                device.trigger_config.source = crate::device::TriggerSource::Analog;
            }
            if ui.selectable_label(is_digital, "💻").on_hover_text("Digital Trigger").clicked() {
                device.trigger_config.source = crate::device::TriggerSource::Digital;
            }
        });

        ui.add_space(5.0);

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
        ui.label(RichText::new("📊 Analog Trigger").strong().size(12.0));

        // Trigger Level - Compact
        ui.horizontal(|ui| {
            ui.label("LVL:");
            let mut level = device.trigger_config.analog.level as f32;
            if ui.add(egui::Slider::new(&mut level, 0.0..=1.0).suffix("V").show_value(true)).changed() {
                device.trigger_config.analog.level = level as f64;
                notifications.add_info(format!("Analog trigger level set to {:.2}V for {}", level, device.name));
            }
        });

        // Trigger Pattern - Toggle Buttons
        ui.horizontal(|ui| {
            ui.label("PAT:");
            let pattern = device.trigger_config.analog.pattern;
            
            let is_rising = pattern == crate::device::AnalogTriggerPattern::Rising;
            let is_falling = pattern == crate::device::AnalogTriggerPattern::Falling;
            let is_level = pattern == crate::device::AnalogTriggerPattern::Level;
            let is_auto = pattern == crate::device::AnalogTriggerPattern::LevelAuto;
            
            if ui.selectable_label(is_rising, "↗").on_hover_text("Rising Edge").clicked() {
                device.trigger_config.analog.pattern = crate::device::AnalogTriggerPattern::Rising;
            }
            if ui.selectable_label(is_falling, "↘").on_hover_text("Falling Edge").clicked() {
                device.trigger_config.analog.pattern = crate::device::AnalogTriggerPattern::Falling;
            }
            if ui.selectable_label(is_level, "─").on_hover_text("Level").clicked() {
                device.trigger_config.analog.pattern = crate::device::AnalogTriggerPattern::Level;
            }
            if ui.selectable_label(is_auto, "⟲").on_hover_text("Level + Auto").clicked() {
                device.trigger_config.analog.pattern = crate::device::AnalogTriggerPattern::LevelAuto;
            }
        });
    }

    fn render_digital_trigger_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        notifications: &mut NotificationManager,
    ) {
        ui.label(RichText::new("💻 Digital Trigger").strong().size(12.0));

        // Trigger Mode - Toggle Buttons
        ui.horizontal(|ui| {
            ui.label("MOD:");
            let mode = device.trigger_config.digital.mode;
            
            let is_start = mode == crate::device::DigitalTriggerMode::StartMatching;
            let is_stop = mode == crate::device::DigitalTriggerMode::StopMatching;
            let is_while = mode == crate::device::DigitalTriggerMode::WhileMatching;
            let is_auto = mode == crate::device::DigitalTriggerMode::WhileMatchingAuto;
            
            if ui.selectable_label(is_start, "▶").on_hover_text("Start Matching").clicked() {
                device.trigger_config.digital.mode = crate::device::DigitalTriggerMode::StartMatching;
            }
            if ui.selectable_label(is_stop, "⏹").on_hover_text("Stop Matching").clicked() {
                device.trigger_config.digital.mode = crate::device::DigitalTriggerMode::StopMatching;
            }
            if ui.selectable_label(is_while, "⏸").on_hover_text("While Matching").clicked() {
                device.trigger_config.digital.mode = crate::device::DigitalTriggerMode::WhileMatching;
            }
            if ui.selectable_label(is_auto, "⟲").on_hover_text("While + Auto").clicked() {
                device.trigger_config.digital.mode = crate::device::DigitalTriggerMode::WhileMatchingAuto;
            }
        });

        // Bit Pattern Configuration - Compact Layout
        ui.add_space(3.0);
        ui.label("BIT PATTERN:");
        
        // First row: D0-D4
        ui.horizontal(|ui| {
            ui.label("D0-4:");
            for ch in 0..5 {
                let bit_state = device.trigger_config.digital.bit_pattern[ch];
                let (text, color) = match bit_state {
                    crate::device::DigitalBitState::DontCare => ("X", Color32::GRAY),
                    crate::device::DigitalBitState::Low => ("0", Color32::LIGHT_RED),
                    crate::device::DigitalBitState::High => ("1", Color32::LIGHT_GREEN),
                };
                
                if ui.button(RichText::new(text).color(color)).on_hover_text(&format!("D{}", ch)).clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.digital.bit_pattern[ch] = bit_state.cycle();
                    device.set_trigger_config(new_config.clone());
                }
            }
        });
        
        // Second row: D5-D8 + Clear
        ui.horizontal(|ui| {
            ui.label("D5-8:");
            for ch in 5..9 {
                let bit_state = device.trigger_config.digital.bit_pattern[ch];
                let (text, color) = match bit_state {
                    crate::device::DigitalBitState::DontCare => ("X", Color32::GRAY),
                    crate::device::DigitalBitState::Low => ("0", Color32::LIGHT_RED),
                    crate::device::DigitalBitState::High => ("1", Color32::LIGHT_GREEN),
                };
                
                if ui.button(RichText::new(text).color(color)).on_hover_text(&format!("D{}", ch)).clicked() {
                    let mut new_config = device.trigger_config.clone();
                    new_config.digital.bit_pattern[ch] = bit_state.cycle();
                    device.set_trigger_config(new_config.clone());
                }
            }
            
            if ui.small_button("CLR").on_hover_text("Clear All").clicked() {
                let mut new_config = device.trigger_config.clone();
                new_config.digital.bit_pattern = [crate::device::DigitalBitState::DontCare; 9];
                device.set_trigger_config(new_config);
            }
        });

        // Pattern Preview - Compact
        ui.horizontal(|ui| {
            ui.label("PAT:");
            let pattern_str: String = device.trigger_config.digital.bit_pattern.iter()
                .map(|bit| bit.as_str())
                .collect::<Vec<_>>()
                .join("");
            ui.code(RichText::new(pattern_str).size(11.0));
        });
    }

    fn render_waveform_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        notifications: &mut NotificationManager,
    ) {
        // Enable/Disable Toggle
        ui.horizontal(|ui| {
            ui.label("EN:");
            let enabled = device.waveform_config.enabled;
            let button_text = if enabled { "ON" } else { "OFF" };
            let mut new_enabled = enabled;
            if ui.toggle_value(&mut new_enabled, button_text).on_hover_text("Enable Waveform Generator").clicked() {
                device.waveform_config.enabled = new_enabled;
            }
        });

        if device.waveform_config.enabled {
            ui.add_space(1.0);

            // Waveform Type Selection
            ui.horizontal(|ui| {
                ui.label("TYPE:");
                let current_type = device.waveform_config.waveform_type;
                
                let is_sine = current_type == crate::device::WaveformType::Sine;
                let is_square = current_type == crate::device::WaveformType::Square;
                let is_triangle = current_type == crate::device::WaveformType::Triangle;
                let is_ekg = current_type == crate::device::WaveformType::Ekg;
                
                if ui.selectable_label(is_sine, "～").on_hover_text("Sine Wave").clicked() {
                    device.waveform_config.waveform_type = crate::device::WaveformType::Sine;
                }
                if ui.selectable_label(is_square, "⊓").on_hover_text("Square Wave").clicked() {
                    device.waveform_config.waveform_type = crate::device::WaveformType::Square;
                }
                if ui.selectable_label(is_triangle, "△").on_hover_text("Triangle Wave").clicked() {
                    device.waveform_config.waveform_type = crate::device::WaveformType::Triangle;
                }
                if ui.selectable_label(is_ekg, "💓").on_hover_text("EKG Wave").clicked() {
                    device.waveform_config.waveform_type = crate::device::WaveformType::Ekg;
                }
            });

            // Frequency Control
            ui.horizontal(|ui| {
                ui.label("FREQ:");
                let mut freq = device.waveform_config.frequency_hz as f32;
                if ui.add(egui::Slider::new(&mut freq, 10.0..=4000.0)
                    .logarithmic(true)
                    .suffix("Hz")
                    .show_value(false)
                    .custom_formatter(|v, _| {
                        if v >= 1000.0 {
                            format!("{:.1}kHz", v / 1000.0)
                        } else {
                            format!("{:.0}Hz", v)
                        }
                    })).changed() {
                    device.waveform_config.frequency_hz = freq as f64;
                    device.waveform_config.clamp_frequency();
                    let freq_str = if freq >= 1000.0 {
                        format!("{:.1}kHz", freq / 1000.0)
                    } else {
                        format!("{:.0}Hz", freq)
                    };
                    notifications.add_info(format!("Frequency: {} - {}", freq_str, device.name));
                }
            });

            // Quick Frequency Presets
            ui.horizontal(|ui| {
                ui.label("PRESET:");
                for freq in [10.0, 50.0, 100.0, 500.0, 1000.0, 2000.0] {
                    let label = if freq >= 1000.0 {
                        format!("{}k", freq / 1000.0)
                    } else {
                        format!("{}", freq)
                    };
                    if ui.small_button(label).on_hover_text(&format!("{}Hz", freq)).clicked() {
                        device.waveform_config.frequency_hz = freq;
                        let freq_str = if freq >= 1000.0 {
                            format!("{:.1}kHz", freq / 1000.0)
                        } else {
                            format!("{:.0}Hz", freq)
                        };
                        notifications.add_info(format!("Frequency: {} - {}", freq_str, device.name));
                    }
                }
            });
        }
    }
}
