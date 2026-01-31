use crate::device::{
    cycle_bitstate, waveform_to_icon, DeviceManager, Notification, MAX_TIME_FRAME, MIN_TIME_FRAME,
};
use crate::notifications::NotificationManager;
use crate::worker_interface::{CaptureModeFlat, FleaScopeDevice};
use egui::{Color32, RichText};
use fleascope_rs::{
    AnalogTriggerBehavior, BitState, DigitalTriggerBehavior, FleaConnector, Waveform,
};

#[derive(Default)]
pub struct ControlPanel {
    available_devices: Vec<String>,
}

/// Custom dial widget with optional label and value display
fn dial_widget(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    size: f32,
    label: Option<&str>,
    unit: Option<&str>,
) -> egui::Response {
    #[cfg(feature = "puffin")]
    puffin::profile_function!();
    let desired_size = egui::vec2(size, size);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click_and_drag());

    // Handle interaction FIRST (before drawing anything)
    if response.clicked() || response.dragged() {
        if let Some(pointer_pos) = response.interact_pointer_pos() {
            let center = rect.center();
            let delta = pointer_pos - center;
            let angle = delta.y.atan2(delta.x) + std::f32::consts::PI * 0.75;
            let normalized = (angle / (std::f32::consts::PI * 1.5)).clamp(0.0, 1.0);
            let new_value = range.start() + normalized * (range.end() - range.start());
            if (*value - new_value).abs() > 0.001 {
                // Only update if there's a meaningful change
                *value = new_value;
                response.mark_changed();
            }
        }
    }

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let center = rect.center();
        let radius = rect.width().min(rect.height()) * 0.35;

        // Draw dial background circle
        painter.circle_stroke(center, radius, egui::Stroke::new(2.0, Color32::DARK_GRAY));

        // Draw tick marks
        for i in 0..12 {
            let angle = i as f32 * std::f32::consts::PI / 6.0 - std::f32::consts::PI / 2.0;
            let inner_radius = radius * 0.85;
            let outer_radius = radius * 0.95;
            let start = center + egui::vec2(angle.cos(), angle.sin()) * inner_radius;
            let end = center + egui::vec2(angle.cos(), angle.sin()) * outer_radius;
            painter.line_segment([start, end], egui::Stroke::new(1.0, Color32::GRAY));
        }

        // Calculate angle from value (270° range, starting from top-left)
        let normalized = (*value - range.start()) / (range.end() - range.start());
        let angle = -std::f32::consts::PI * 0.75 + normalized * std::f32::consts::PI * 1.5;

        // Draw pointer
        let pointer_start = center + egui::vec2(angle.cos(), angle.sin()) * radius * 0.3;
        let pointer_end = center + egui::vec2(angle.cos(), angle.sin()) * radius;
        painter.line_segment(
            [pointer_start, pointer_end],
            egui::Stroke::new(3.0, Color32::LIGHT_BLUE),
        );

        // Draw optional label in top-left corner (outside the interactive area)
        if let Some(label_text) = label {
            let label_pos = rect.min + egui::vec2(1.0, 1.0);
            painter.text(
                label_pos,
                egui::Align2::LEFT_TOP,
                label_text,
                egui::FontId::proportional(8.0),
                Color32::LIGHT_GRAY,
            );
        }

        // Draw current value in bottom-right corner (outside the interactive area)
        let value_text = if let Some(unit_text) = unit {
            if *value >= 1000.0 && unit == Some("Hz") {
                format!("{:.1}k{}", *value / 1000.0, unit_text)
            } else {
                format!("{:.1}{}", value, unit_text)
            }
        } else {
            format!("{:.1}", value)
        };
        let value_pos = rect.max - egui::vec2(1.0, 1.0);
        painter.text(
            value_pos,
            egui::Align2::RIGHT_BOTTOM,
            &value_text,
            egui::FontId::proportional(8.0),
            Color32::WHITE,
        );
    }

    response
}

/// Custom exponential dial widget for logarithmic ranges (like time scales)
fn exponential_dial_widget(
    ui: &mut egui::Ui,
    value: &mut f64,
    min_value: f64,
    max_value: f64,
    size: f32,
    label: Option<&str>,
    unit: Option<&str>,
) -> egui::Response {
    #[cfg(feature = "puffin")]
    puffin::profile_function!();
    // Convert current value to 0.0-1.0 exponential scale
    let clamped_value = value.clamp(min_value, max_value);
    let log_ratio = (clamped_value / min_value).ln() / (max_value / min_value).ln();
    let dial_position = log_ratio as f32;

    let desired_size = egui::vec2(size, size);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click_and_drag());

    // Handle interaction FIRST (before drawing anything)
    if response.clicked() || response.dragged() {
        if let Some(pointer_pos) = response.interact_pointer_pos() {
            let center = rect.center();
            let delta = pointer_pos - center;
            let angle = delta.y.atan2(delta.x) + std::f32::consts::PI * 0.75;
            let normalized = (angle / (std::f32::consts::PI * 1.5)).clamp(0.0, 1.0);

            // Convert back from exponential scale to actual value
            let new_value = min_value * ((max_value / min_value).powf(normalized as f64));
            if (*value - new_value).abs() > min_value * 0.001 {
                // Only update if there's a meaningful change
                *value = new_value;
                response.mark_changed();
            }
        }
    }

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let center = rect.center();
        let radius = rect.width().min(rect.height()) * 0.35;

        // Draw dial background circle
        painter.circle_stroke(center, radius, egui::Stroke::new(2.0, Color32::DARK_GRAY));

        // Draw tick marks
        for i in 0..12 {
            let angle = i as f32 * std::f32::consts::PI / 6.0 - std::f32::consts::PI / 2.0;
            let inner_radius = radius * 0.85;
            let outer_radius = radius * 0.95;
            let start = center + egui::vec2(angle.cos(), angle.sin()) * inner_radius;
            let end = center + egui::vec2(angle.cos(), angle.sin()) * outer_radius;
            painter.line_segment([start, end], egui::Stroke::new(1.0, Color32::GRAY));
        }

        // Calculate angle from dial position (270° range, starting from top-left)
        let angle = -std::f32::consts::PI * 0.75 + dial_position * std::f32::consts::PI * 1.5;

        // Draw pointer
        let pointer_start = center + egui::vec2(angle.cos(), angle.sin()) * radius * 0.3;
        let pointer_end = center + egui::vec2(angle.cos(), angle.sin()) * radius;
        painter.line_segment(
            [pointer_start, pointer_end],
            egui::Stroke::new(3.0, Color32::LIGHT_BLUE),
        );

        // Draw optional label in top-left corner
        if let Some(label_text) = label {
            let label_pos = rect.min + egui::vec2(1.0, 1.0);
            painter.text(
                label_pos,
                egui::Align2::LEFT_TOP,
                label_text,
                egui::FontId::proportional(8.0),
                Color32::LIGHT_GRAY,
            );
        }

        // Draw current value with appropriate formatting
        let value_text = pretty_print_number(*value, unit, 2);
        let value_pos = rect.max - egui::vec2(1.0, 1.0);
        painter.text(
            value_pos,
            egui::Align2::RIGHT_BOTTOM,
            &value_text,
            egui::FontId::proportional(8.0),
            Color32::WHITE,
        );
    }

    response
}

fn pretty_print_number(value: f64, unit: Option<&str>, significant_digits: usize) -> String {
    if value == 0.0 {
        return format!("0{}", unit.unwrap_or(""));
    }

    let abs_value = value.abs();
    const MAGNITUDES: &[(f64, &str)] = &[
        (1e9, "G"),
        (1e6, "M"),
        (1e3, "k"),
        (1.0, ""),
        (1e-3, "m"),
        (1e-6, "μ"),
        (1e-9, "n"),
    ];

    let (factor, prefix) = MAGNITUDES
        .iter()
        .find(|&&(f, _)| abs_value >= f)
        .copied()
        .unwrap_or((1.0, ""));

    let scaled = value / factor;
    let abs_scaled = scaled.abs();

    // Calculate decimal places needed to show the requested significant digits
    let decimal_places = if abs_scaled >= 100.0 {
        // For 100+ : show as integer (e.g., "123k" not "123.k")
        (significant_digits.saturating_sub(3)).max(0)
    } else if abs_scaled >= 10.0 {
        // For 10-99: show 1 less decimal place (e.g., "12.3k" for 3 sig digits)
        (significant_digits.saturating_sub(2)).max(0)
    } else {
        // For 1-9.99: show full decimal places (e.g., "1.23k" for 3 sig digits)
        (significant_digits.saturating_sub(1)).max(0)
    };

    format!(
        "{:.*}{}{}",
        decimal_places,
        scaled,
        prefix,
        unit.unwrap_or("")
    )
}

impl ControlPanel {
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        device_manager: &mut DeviceManager,
        notifications: &mut NotificationManager,
    ) {
        #[cfg(feature = "puffin")]
        puffin::profile_function!();

        ui.heading("🎛️ Control Panel");

        ui.separator();

        // Add Device Section
        ui.group(|ui| {
            ui.label("Connect:");
            if ui.button("Refresh devices").clicked() {
                match FleaConnector::get_available_devices(None) {
                    Ok(it) => self.available_devices = it.map(|d| d.name).collect(),
                    Err(e) => {
                        notifications.add_error(format!("Failed to load devices: {}", e));
                        tracing::error!("Failed to load devices: {}", e);
                    }
                }
            }
            ui.horizontal_wrapped(|ui| {
                for hostname in &self.available_devices {
                    if device_manager
                        .get_devices()
                        .iter()
                        .any(|d| d.name == *hostname)
                    {
                        continue;
                    }
                    if ui.small_button(hostname).clicked() {
                        match device_manager.add_device(hostname.to_string()) {
                            Ok(_) => {
                                notifications
                                    .add_success(format!("Connected to device: {}", hostname));
                            }
                            Err(e) => {
                                notifications
                                    .add_error(format!("Failed to connect to {}: {}", hostname, e));
                                tracing::error!("Failed to add device: {}", e);
                            }
                        }
                    }
                }
            });
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

                        for (idx, device) in device_manager.get_devices_mut().iter_mut().enumerate()
                        {
                            ui.group(|ui| {
                                self.render_device_rack(
                                    ui,
                                    device,
                                    idx,
                                    &mut to_remove,
                                    notifications,
                                );
                            });
                            ui.add_space(5.0);
                        }

                        if let Some(idx) = to_remove {
                            let device_name = device_manager
                                .get_devices()
                                .get(idx)
                                .map(|d| d.name.clone())
                                .unwrap_or_else(|| "Unknown".to_string());
                            notifications.add_info(format!("Removed device: {}", device_name));
                            tracing::info!("Removing device: {}", device_name);
                            device_manager.remove_device(idx);
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
        #[cfg(feature = "puffin")]
        puffin::profile_function!();
        // Check for calibration results at the beginning of each frame
        device
            .notification_rx
            .try_recv()
            .map(|notification| match notification {
                Notification::Success(msg) => notifications.add_success(msg),
                Notification::Error(msg) => notifications.add_error(msg),
            })
            .ok();

        // Device Header - Retro Style with LED Status
        ui.horizontal(|ui| {
            // Large power LED with classic styling
            let status_color = if device.data.load().connected {
                Color32::GREEN
            } else {
                Color32::RED
            };
            ui.add_space(2.0);
            ui.colored_label(status_color, "●");
            ui.add_space(2.0);

            // Device name with retro font styling
            ui.label(
                RichText::new(&device.name)
                    .strong()
                    .size(14.0)
                    .color(Color32::LIGHT_YELLOW),
            );

            // Active waveform indicator with classic scope styling
            if device.get_waveform_config().enabled {
                ui.add_space(5.0);
                ui.colored_label(Color32::from_rgb(0, 255, 100), "●");
                ui.label(RichText::new("GEN").size(8.0).color(Color32::LIGHT_GRAY));
                ui.label(
                    RichText::new(waveform_to_icon(device.get_waveform_config().waveform_type))
                        .size(12.0)
                        .color(Color32::LIGHT_BLUE),
                );
            }

            // Hardware-style model indicator
            ui.add_space(10.0);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Classic red power button
                if ui
                    .add_sized(
                        [25.0, 20.0],
                        egui::Button::new(RichText::new("⏻").size(12.0).color(Color32::RED)),
                    )
                    .on_hover_text("Disconnect Device")
                    .clicked()
                {
                    *to_remove = Some(idx);
                }
            });
        });

        ui.separator();

        // Retro Channel Panel - Dense Grid Layout
        ui.group(|ui| {
            ui.label(
                RichText::new("CHANNEL INPUT")
                    .size(10.0)
                    .strong()
                    .color(Color32::YELLOW),
            );

            egui::Grid::new(format!("channels_grid_{}", idx))
                .num_columns(5)
                .spacing([3.0, 3.0])
                .show(ui, |ui| {
                    // Row 1: Analog channel with larger toggle
                    ui.label(RichText::new("ANALOG").size(8.0).color(Color32::LIGHT_GRAY));
                    let mut analog_enabled = device.enabled_channels[0];
                    if ui
                        .add_sized(
                            [30.0, 20.0],
                            egui::Button::new(
                                RichText::new(if analog_enabled { "ON" } else { "OFF" })
                                    .size(8.0)
                                    .color(if analog_enabled {
                                        Color32::GREEN
                                    } else {
                                        Color32::RED
                                    }),
                            ),
                        )
                        .clicked()
                    {
                        analog_enabled = !analog_enabled;
                        let mut new_channels = device.enabled_channels;
                        new_channels[0] = analog_enabled;
                        device.set_enabled_channels(new_channels);
                    }

                    // Add probe multiplier controls
                    ui.label(RichText::new("PROBE").size(8.0).color(Color32::LIGHT_GRAY));
                    let is_x10 =
                        device.get_probe_multiplier() == fleascope_rs::flea_scope::ProbeType::X10;
                    if ui
                        .add_sized(
                            [25.0, 20.0],
                            egui::Button::new(
                                RichText::new(if is_x10 { "×10" } else { "×1" })
                                    .size(8.0)
                                    .color(if is_x10 {
                                        Color32::YELLOW
                                    } else {
                                        Color32::WHITE
                                    }),
                            ),
                        )
                        .clicked()
                    {
                        let new_probe = if is_x10 {
                            fleascope_rs::flea_scope::ProbeType::X1
                        } else {
                            fleascope_rs::flea_scope::ProbeType::X10
                        };
                        device.set_probe_multiplier(new_probe);
                    }

                    ui.end_row();

                    // Row 2: Digital channels header
                    ui.label(
                        RichText::new("DIGITAL")
                            .size(8.0)
                            .color(Color32::LIGHT_GRAY),
                    );
                    ui.label(RichText::new("D0-D3").size(7.0).color(Color32::GRAY));
                    ui.label(RichText::new("D4-D7").size(7.0).color(Color32::GRAY));
                    ui.label(RichText::new("D8").size(7.0).color(Color32::GRAY));
                    ui.label(RichText::new("ALL").size(7.0).color(Color32::GRAY));
                    ui.end_row();

                    // Row 3: Digital channel toggles D0-D3
                    ui.label(""); // Empty label instead of add_space
                    for ch in 0..4 {
                        let mut enabled = device.enabled_channels[ch + 1];
                        if ui
                            .add_sized(
                                [15.0, 15.0],
                                egui::Button::new(
                                    RichText::new(format!("{}", ch))
                                        .size(7.0)
                                        .color(if enabled {
                                            Color32::GREEN
                                        } else {
                                            Color32::DARK_GRAY
                                        }),
                                ),
                            )
                            .clicked()
                        {
                            enabled = !enabled;
                            let mut new_channels = device.enabled_channels;
                            new_channels[ch + 1] = enabled;
                            device.set_enabled_channels(new_channels);
                        }
                    }

                    // All digital toggle
                    let all_digital_on = device.enabled_channels[1..].iter().all(|&x| x);
                    if ui
                        .add_sized(
                            [20.0, 15.0],
                            egui::Button::new(
                                RichText::new(if all_digital_on { "CLR" } else { "ALL" })
                                    .size(7.0)
                                    .color(if all_digital_on {
                                        Color32::RED
                                    } else {
                                        Color32::GREEN
                                    }),
                            ),
                        )
                        .clicked()
                    {
                        let mut new_channels = device.enabled_channels;
                        let new_state = !all_digital_on;
                        for ch in new_channels.iter_mut().skip(1) {
                            *ch = new_state;
                        }
                        device.set_enabled_channels(new_channels);
                    }
                    ui.end_row();

                    // Row 4: Digital channel toggles D4-D8
                    ui.label(""); // Empty label instead of add_space
                    for ch in 4..8 {
                        let mut enabled = device.enabled_channels[ch + 1];
                        if ui
                            .add_sized(
                                [15.0, 15.0],
                                egui::Button::new(
                                    RichText::new(format!("{}", ch))
                                        .size(7.0)
                                        .color(if enabled {
                                            Color32::GREEN
                                        } else {
                                            Color32::DARK_GRAY
                                        }),
                                ),
                            )
                            .clicked()
                        {
                            enabled = !enabled;
                            let mut new_channels = device.enabled_channels;
                            new_channels[ch + 1] = enabled;
                            device.set_enabled_channels(new_channels);
                        }
                    }

                    // D8 channel
                    let mut enabled_d8 = device.enabled_channels[9];
                    if ui
                        .add_sized(
                            [15.0, 15.0],
                            egui::Button::new(RichText::new("8").size(7.0).color(if enabled_d8 {
                                Color32::GREEN
                            } else {
                                Color32::DARK_GRAY
                            })),
                        )
                        .clicked()
                    {
                        enabled_d8 = !enabled_d8;
                        let mut new_channels = device.enabled_channels;
                        new_channels[9] = enabled_d8;
                        device.set_enabled_channels(new_channels);
                    }

                    ui.label(""); // Empty label instead of add_space
                    ui.end_row();
                });
        });

        // Retro Timebase Control Panel
        ui.add_space(3.0);
        ui.group(|ui| {
            ui.label(
                RichText::new("TIME BASE")
                    .size(10.0)
                    .strong()
                    .color(Color32::YELLOW),
            );

            egui::Grid::new(format!("timebase_grid_{}", idx))
                .num_columns(4)
                .spacing([4.0, 4.0])
                .show(ui, |ui| {
                    // Row 1: Main time dial
                    ui.label(
                        RichText::new("SEC/DIV")
                            .size(8.0)
                            .color(Color32::LIGHT_GRAY),
                    );

                    // Convert actual time to exponential scale for the dial
                    let mut current_time = device.get_triggered_config().time_frame;

                    if exponential_dial_widget(
                        ui,
                        &mut current_time,
                        MIN_TIME_FRAME,
                        MAX_TIME_FRAME,
                        45.0,
                        Some("TIME"),
                        Some("s"),
                    )
                    .changed()
                    {
                        device.set_time_frame(current_time);
                    }

                    ui.end_row();

                    // Row 3: Control buttons
                    ui.label(
                        RichText::new("CONTROL")
                            .size(8.0)
                            .color(Color32::LIGHT_GRAY),
                    );

                    // Pause/Resume button
                    let is_paused = !device.data.load().running;
                    if ui
                        .add_sized(
                            [25.0, 20.0],
                            egui::Button::new(
                                RichText::new(if is_paused { "RUN" } else { "STOP" })
                                    .size(8.0)
                                    .color(if is_paused {
                                        Color32::GREEN
                                    } else {
                                        Color32::RED
                                    }),
                            ),
                        )
                        .clicked()
                    {
                        if is_paused {
                            device.resume();
                        } else {
                            device.pause();
                        }
                    }

                    ui.end_row();
                });
        });

        // Retro Calibration & Utility Panel
        ui.add_space(3.0);
        ui.group(|ui| {
            ui.label(
                RichText::new("CALIBRATION & UTIL")
                    .size(10.0)
                    .strong()
                    .color(Color32::YELLOW),
            );

            egui::Grid::new(format!("cal_grid_{}", idx))
                .num_columns(4)
                .spacing([3.0, 3.0])
                .show(ui, |ui| {
                    // Row 1: Calibration controls
                    ui.label(
                        RichText::new("CAL REF")
                            .size(8.0)
                            .color(Color32::LIGHT_GRAY),
                    );

                    if ui
                        .add_sized(
                            [22.0, 18.0],
                            egui::Button::new(
                                RichText::new("0V").size(8.0).color(Color32::LIGHT_BLUE),
                            ),
                        )
                        .clicked()
                    {
                        match device.start_calibrate_0v() {
                            Ok(()) => {
                                notifications.add_info(format!("0V cal started - {}", device.name))
                            }
                            Err(e) => notifications
                                .add_error(format!("0V cal failed - {}: {}", device.name, e)),
                        }
                    }

                    if ui
                        .add_sized(
                            [22.0, 18.0],
                            egui::Button::new(
                                RichText::new("3V").size(8.0).color(Color32::LIGHT_BLUE),
                            ),
                        )
                        .clicked()
                    {
                        match device.start_calibrate_3v() {
                            Ok(()) => {
                                notifications.add_info(format!("3V cal started - {}", device.name))
                            }
                            Err(e) => notifications
                                .add_error(format!("3V cal failed - {}: {}", device.name, e)),
                        }
                    }

                    if ui
                        .add_sized(
                            [25.0, 18.0],
                            egui::Button::new(
                                RichText::new("STORE").size(7.0).color(Color32::YELLOW),
                            ),
                        )
                        .clicked()
                    {
                        match device.start_store_calibration() {
                            Ok(()) => {
                                notifications.add_info(format!("Cal stored - {}", device.name))
                            }
                            Err(e) => notifications
                                .add_error(format!("Store failed - {}: {}", device.name, e)),
                        }
                    }
                    ui.end_row();
                });
        });

        // Retro Capture Mode Panel
        ui.add_space(3.0);
        egui::CollapsingHeader::new(
            RichText::new("📊 CAPTURE MODE")
                .size(10.0)
                .strong()
                .color(Color32::YELLOW),
        )
        .id_salt(format!("capture_mode_device_{}", idx))
        .default_open(true)
        .show(ui, |ui| {
            self.render_retro_capture_mode_config(ui, device, idx, notifications);
        });

        // Retro Trigger Control Panel - Only show in triggered mode
        if matches!(device.get_capture_mode(), CaptureModeFlat::Triggered) {
            ui.add_space(3.0);
            egui::CollapsingHeader::new(
                RichText::new("⚡ TRIGGER CONTROLS")
                    .size(10.0)
                    .strong()
                    .color(Color32::YELLOW),
            )
            .id_salt(format!("trigger_device_{}", idx))
            .default_open(true)
            .show(ui, |ui| {
                self.render_retro_trigger_config(ui, device, idx, notifications);
            });
        }

        // Retro Waveform Generator Panel
        ui.add_space(3.0);
        egui::CollapsingHeader::new(
            RichText::new("🌊 SIGNAL GENERATOR")
                .size(10.0)
                .strong()
                .color(Color32::YELLOW),
        )
        .id_salt(format!("waveform_device_{}", idx))
        .default_open(true)
        .show(ui, |ui| {
            self.render_retro_waveform_config(ui, device, idx, notifications);
        });

        // Retro System Status Panel - Even more compact
        ui.add_space(3.0);
        ui.group(|ui| {
            ui.label(
                RichText::new("SYSTEM STATUS")
                    .size(10.0)
                    .strong()
                    .color(Color32::YELLOW),
            );

            // Use ArcSwap load for data access
            let data = device.data.load();
            let update_age = data.last_update.elapsed().as_millis();

            egui::Grid::new(format!("status_grid_{}", idx))
                .num_columns(6)
                .spacing([2.0, 2.0])
                .show(ui, |ui| {
                    // Row 2: Compact statistics
                    ui.label(RichText::new("STATS").size(7.0).color(Color32::LIGHT_GRAY));
                    ui.label(
                        RichText::new(format!("{:.1}Hz", data.update_rate))
                            .size(6.0)
                            .color(Color32::WHITE),
                    );
                    ui.label(RichText::new("RATE").size(6.0).color(Color32::LIGHT_GRAY));
                    ui.label(
                        RichText::new(format!("{}ms", update_age))
                            .size(6.0)
                            .color(Color32::WHITE),
                    );
                    ui.label(RichText::new("AGE").size(6.0).color(Color32::LIGHT_GRAY));
                    ui.label(""); // Empty label instead of add_space
                    ui.end_row();
                });
        });

        // Hardware-Style Footer with Model Info and Calibration Status
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(5.0);

            // Model info in classic oscilloscope style
            ui.label(
                RichText::new("FleaScope")
                    .size(8.0)
                    .color(Color32::LIGHT_YELLOW)
                    .family(egui::FontFamily::Monospace),
            );
            ui.label(RichText::new("•").size(6.0).color(Color32::DARK_GRAY));
            ui.label(
                RichText::new("v2.1")
                    .size(7.0)
                    .color(Color32::DARK_GRAY)
                    .family(egui::FontFamily::Monospace),
            );

            ui.add_space(10.0);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Temperature indicator (classic scope feature)
                ui.add_space(5.0);

                // Active waveform frequency display
                if device.get_waveform_config().enabled {
                    let freq_str = if device.get_waveform_config().frequency_hz >= 1000 {
                        format!(
                            "{:.1}kHz",
                            device.get_waveform_config().frequency_hz as f32 / 1000.0
                        )
                    } else {
                        format!("{}Hz", device.get_waveform_config().frequency_hz)
                    };
                    ui.label(RichText::new("GEN:").size(7.0).color(Color32::LIGHT_GRAY));
                    ui.label(
                        RichText::new(&freq_str)
                            .size(8.0)
                            .color(Color32::LIGHT_BLUE)
                            .family(egui::FontFamily::Monospace),
                    );
                }
            });
        });
    }

    fn render_retro_trigger_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        _notifications: &mut NotificationManager,
    ) {
        ui.group(|ui| {
            egui::Grid::new(format!("retro_trigger_{}", idx))
                .num_columns(5)
                .spacing([4.0, 4.0])
                .show(ui, |ui| {
                    // Row 1: Source selection with LED-style indicators
                    ui.label(RichText::new("SOURCE").size(8.0).color(Color32::LIGHT_GRAY));

                    let is_analog = device.get_triggered_config().trigger_config.source
                        == crate::device::TriggerSource::Analog;
                    if ui
                        .add_sized(
                            [30.0, 22.0],
                            egui::Button::new(RichText::new("ANALOG").size(8.0).color(
                                if is_analog {
                                    Color32::GREEN
                                } else {
                                    Color32::DARK_GRAY
                                },
                            )),
                        )
                        .clicked()
                    {
                        let mut new_config = device.get_triggered_config().trigger_config.clone();
                        new_config.source = crate::device::TriggerSource::Analog;
                        device.set_trigger_config(new_config);
                    }

                    let is_digital = device.get_triggered_config().trigger_config.source
                        == crate::device::TriggerSource::Digital;
                    if ui
                        .add_sized(
                            [35.0, 22.0],
                            egui::Button::new(RichText::new("DIGITAL").size(8.0).color(
                                if is_digital {
                                    Color32::GREEN
                                } else {
                                    Color32::DARK_GRAY
                                },
                            )),
                        )
                        .clicked()
                    {
                        let mut new_config = device.get_triggered_config().trigger_config.clone();
                        new_config.source = crate::device::TriggerSource::Digital;
                        device.set_trigger_config(new_config);
                    }

                    ui.label(""); // Empty labels instead of add_space
                    ui.label("");
                    ui.end_row();

                    // Row 2: Analog trigger controls
                    if is_analog {
                        ui.label(RichText::new("LEVEL").size(8.0).color(Color32::LIGHT_GRAY));

                        let mut level =
                            device.get_triggered_config().trigger_config.analog.volts as f32;
                        if dial_widget(ui, &mut level, -6.6..=6.6, 40.0, Some("LVL"), Some("V"))
                            .changed()
                        {
                            let mut new_config =
                                device.get_triggered_config().trigger_config.clone();
                            new_config.analog.volts = level as f64;
                            device.set_trigger_config(new_config);
                        }

                        ui.label(RichText::new("SLOPE").size(8.0).color(Color32::LIGHT_GRAY));

                        let pattern = device.get_triggered_config().trigger_config.analog.behavior;
                        let behaviors = [
                            (AnalogTriggerBehavior::Rising, "↗", "RISE"),
                            (AnalogTriggerBehavior::Falling, "↘", "FALL"),
                            (AnalogTriggerBehavior::Level, "─", "LEVEL"),
                            (AnalogTriggerBehavior::Auto, "⟲", "AUTO"),
                        ];

                        for (behavior, _icon, label) in behaviors {
                            let is_selected = pattern == behavior;
                            if ui
                                .add_sized(
                                    [25.0, 18.0],
                                    egui::Button::new(RichText::new(label).size(7.0).color(
                                        if is_selected {
                                            Color32::YELLOW
                                        } else {
                                            Color32::LIGHT_GRAY
                                        },
                                    )),
                                )
                                .clicked()
                            {
                                let mut new_config =
                                    device.get_triggered_config().trigger_config.clone();
                                new_config.analog.behavior = behavior;
                                device.set_trigger_config(new_config);
                            }
                        }
                        ui.end_row();
                    }

                    // Digital trigger controls
                    if is_digital {
                        ui.label(RichText::new("MODE").size(8.0).color(Color32::LIGHT_GRAY));

                        let mode = device
                            .get_triggered_config()
                            .trigger_config
                            .digital
                            .behavior;
                        let modes = [
                            (DigitalTriggerBehavior::Start, "START"),
                            (DigitalTriggerBehavior::Stop, "STOP"),
                            (DigitalTriggerBehavior::While, "WHILE"),
                            (DigitalTriggerBehavior::Auto, "AUTO"),
                        ];

                        for (behavior, label) in modes {
                            let is_selected = mode == behavior;
                            if ui
                                .add_sized(
                                    [25.0, 18.0],
                                    egui::Button::new(RichText::new(label).size(7.0).color(
                                        if is_selected {
                                            Color32::YELLOW
                                        } else {
                                            Color32::LIGHT_GRAY
                                        },
                                    )),
                                )
                                .clicked()
                            {
                                let mut new_config =
                                    device.get_triggered_config().trigger_config.clone();
                                new_config.digital.behavior = behavior;
                                device.set_trigger_config(new_config);
                            }
                        }
                        ui.end_row();

                        // Digital bit pattern in retro style
                        ui.label(
                            RichText::new("PATTERN")
                                .size(8.0)
                                .color(Color32::LIGHT_GRAY),
                        );

                        // D0-D4 buttons
                        for ch in 0..5 {
                            let bit_state = device
                                .get_triggered_config()
                                .trigger_config
                                .digital
                                .bit_states[ch];
                            let (text, color) = match bit_state {
                                BitState::DontCare => ("X", Color32::GRAY),
                                BitState::Low => ("0", Color32::RED),
                                BitState::High => ("1", Color32::GREEN),
                            };

                            if ui
                                .add_sized(
                                    [15.0, 15.0],
                                    egui::Button::new(RichText::new(text).size(8.0).color(color)),
                                )
                                .clicked()
                            {
                                let mut new_config =
                                    device.get_triggered_config().trigger_config.clone();
                                new_config.digital.bit_states[ch] = cycle_bitstate(bit_state);
                                device.set_trigger_config(new_config);
                            }
                        }
                        ui.end_row();

                        // Second row for D5-D8 + Clear
                        ui.label(""); // Empty label instead of add_space
                        for ch in 5..9 {
                            let bit_state = device
                                .get_triggered_config()
                                .trigger_config
                                .digital
                                .bit_states[ch];
                            let (text, color) = match bit_state {
                                BitState::DontCare => ("X", Color32::GRAY),
                                BitState::Low => ("0", Color32::RED),
                                BitState::High => ("1", Color32::GREEN),
                            };

                            if ui
                                .add_sized(
                                    [15.0, 15.0],
                                    egui::Button::new(RichText::new(text).size(8.0).color(color)),
                                )
                                .clicked()
                            {
                                let mut new_config =
                                    device.get_triggered_config().trigger_config.clone();
                                new_config.digital.bit_states[ch] = cycle_bitstate(bit_state);
                                device.set_trigger_config(new_config);
                            }
                        }

                        if ui
                            .add_sized(
                                [25.0, 15.0],
                                egui::Button::new(
                                    RichText::new("CLEAR").size(7.0).color(Color32::RED),
                                ),
                            )
                            .clicked()
                        {
                            let mut new_config =
                                device.get_triggered_config().trigger_config.clone();
                            new_config.digital.bit_states = [BitState::DontCare; 9];
                            device.set_trigger_config(new_config);
                        }
                        ui.end_row();
                    }
                });
        });
    }

    fn render_retro_waveform_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        idx: usize,
        _notifications: &mut NotificationManager,
    ) {
        ui.group(|ui| {
            egui::Grid::new(format!("retro_waveform_{}", idx))
                .num_columns(5)
                .spacing([4.0, 4.0])
                .show(ui, |ui| {
                    if !device.get_waveform_config().enabled {
                        // Row 1: Enable/Power switch
                        ui.label(RichText::new("POWER").size(8.0).color(Color32::LIGHT_GRAY));

                        if ui
                            .add_sized(
                                [30.0, 22.0],
                                egui::Button::new(
                                    RichText::new("ON").size(8.0).color(Color32::RED),
                                ),
                            )
                            .clicked()
                        {
                            device.set_waveform(
                                device.get_waveform_config().waveform_type,
                                device.get_waveform_config().frequency_hz,
                            );
                        }

                        ui.label(""); // Empty labels instead of add_space
                        ui.label("");
                        ui.label("");
                        ui.end_row();
                    } else {
                        // Row 2: Waveform type selection with retro styling
                        ui.label(RichText::new("WAVE").size(8.0).color(Color32::LIGHT_GRAY));

                        let current_type = device.get_waveform_config().waveform_type;
                        let waveforms = [
                            (Waveform::Sine, "～", "SINE"),
                            (Waveform::Square, "⊓", "SQR"),
                            (Waveform::Triangle, "△", "TRI"),
                            (Waveform::Ekg, "💓", "EKG"),
                        ];

                        for (wave_type, _icon, label) in waveforms {
                            let is_selected = current_type == wave_type;
                            if ui
                                .add_sized(
                                    [22.0, 18.0],
                                    egui::Button::new(RichText::new(label).size(7.0).color(
                                        if is_selected {
                                            Color32::YELLOW
                                        } else {
                                            Color32::LIGHT_GRAY
                                        },
                                    )),
                                )
                                .clicked()
                            {
                                device.set_waveform(
                                    wave_type,
                                    device.get_waveform_config().frequency_hz,
                                );
                            }
                        }
                        ui.end_row();

                        // Row 3: Frequency control with dial
                        ui.label(RichText::new("FREQ").size(8.0).color(Color32::LIGHT_GRAY));

                        let mut freq = device.get_waveform_config().frequency_hz as f32;
                        if dial_widget(ui, &mut freq, 10.0..=4000.0, 45.0, Some("FREQ"), Some("Hz"))
                            .changed()
                        {
                            device.set_waveform(
                                device.get_waveform_config().waveform_type,
                                freq as i32,
                            );
                        }

                        ui.label(
                            RichText::new("PRESETS")
                                .size(8.0)
                                .color(Color32::LIGHT_GRAY),
                        );

                        // Frequency preset buttons
                        for &freq_val in &[100.0, 1000.0, 2000.0] {
                            let label = if freq_val >= 1000.0 {
                                format!("{}k", freq_val / 1000.0)
                            } else {
                                format!("{}", freq_val)
                            };

                            if ui
                                .add_sized(
                                    [20.0, 18.0],
                                    egui::Button::new(
                                        RichText::new(label).size(7.0).color(Color32::LIGHT_BLUE),
                                    ),
                                )
                                .clicked()
                            {
                                device.set_waveform(
                                    device.get_waveform_config().waveform_type,
                                    freq_val as i32,
                                );
                                // let freq_str = if freq_val >= 1000.0 {
                                //     format!("{:.1}kHz", freq_val / 1000.0)
                                // } else {
                                //     format!("{:.0}Hz", freq_val)
                                // };
                                // notifications.add_info(format!("Frequency: {} - {}", freq_str, device.name));
                            }
                        }
                        ui.end_row();
                    }
                });
        });
    }

    fn render_retro_capture_mode_config(
        &self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        _idx: usize,
        _notifications: &mut NotificationManager,
    ) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("MODE:")
                    .size(8.0)
                    .strong()
                    .color(Color32::LIGHT_BLUE),
            );

            // Get current mode for radio buttons
            let is_triggered = matches!(device.get_capture_mode(), CaptureModeFlat::Triggered);

            ui.add_space(10.0);

            // Triggered mode radio button
            let triggered_response = ui.selectable_label(
                is_triggered,
                RichText::new("⚡ TRIGGERED")
                    .size(7.0)
                    .color(if is_triggered {
                        Color32::YELLOW
                    } else {
                        Color32::GRAY
                    }),
            );

            ui.add_space(5.0);

            // Continuous mode radio button
            let continuous_response = ui.selectable_label(
                !is_triggered,
                RichText::new("🌊 CONTINUOUS")
                    .size(7.0)
                    .color(if !is_triggered {
                        Color32::YELLOW
                    } else {
                        Color32::GRAY
                    }),
            );

            // Handle mode changes
            if triggered_response.clicked() && !is_triggered {
                device.set_capture_mode(CaptureModeFlat::Triggered);
            }

            if continuous_response.clicked() && is_triggered {
                device.set_capture_mode(CaptureModeFlat::Continuous);
            }
        });

        ui.add_space(2.0);

        match &device.get_capture_mode() {
            CaptureModeFlat::Triggered => {
                // Time frame control for triggered mode
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("TIME:")
                            .size(8.0)
                            .strong()
                            .color(Color32::LIGHT_BLUE),
                    );

                    ui.add_space(10.0);

                    let time_response = ui.add(
                        egui::Slider::new(
                            &mut *device.get_mut_trigger_time_handle(),
                            MIN_TIME_FRAME..=MAX_TIME_FRAME,
                        )
                        .logarithmic(true)
                        .suffix(" s")
                        .custom_formatter(|n, _| {
                            if n >= 1.0 {
                                format!("{:.2}s", n)
                            } else if n >= 0.001 {
                                format!("{:.0}ms", n * 1000.0)
                            } else {
                                format!("{:.0}μs", n * 1_000_000.0)
                            }
                        }),
                    );

                    if time_response.changed() {
                        device.set_capture_mode(CaptureModeFlat::Triggered);
                    }
                });
            }
            CaptureModeFlat::Continuous => {
                // Buffer duration control for continuous mode
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("BUFFER:")
                            .size(8.0)
                            .strong()
                            .color(Color32::LIGHT_BLUE),
                    );

                    ui.add_space(5.0);

                    if ui
                        .add_sized(
                            [30.0, 22.0],
                            egui::Button::new(RichText::new("Loop").size(8.0).color(
                                if device.wrap {
                                    Color32::GREEN
                                } else {
                                    Color32::RED
                                },
                            )),
                        )
                        .clicked()
                    {
                        device.wrap = !device.wrap;
                    }

                    let buffer_response = ui.add(
                        egui::Slider::new(&mut *device.get_mut_buffer_time_handle(), 0.001..=10.0)
                            .logarithmic(true)
                            .custom_formatter(|n, _| {
                                if n >= 1.0 {
                                    format!("{:.2}s", n)
                                } else {
                                    format!("{:.0}ms", n * 1000.0)
                                }
                            }),
                    );
                });
            }
        }
    }
}
