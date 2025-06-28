use crate::device::{DeviceManager, FleaScopeDevice};
use egui::{Color32, RichText};
use egui_plot::{Line, Plot, PlotPoints};

pub struct PlotArea {
    plot_height: f32,
    colors: Vec<Color32>,
    show_grid: bool,
    auto_scale: bool,
    time_window: f64, // seconds
}

impl Default for PlotArea {
    fn default() -> Self {
        Self {
            plot_height: 200.0,
            colors: vec![
                Color32::YELLOW,
                Color32::LIGHT_BLUE,
                Color32::LIGHT_GREEN,
                Color32::LIGHT_RED,
                Color32::from_rgb(255, 165, 0),   // Orange
                Color32::from_rgb(128, 0, 128),   // Purple
                Color32::from_rgb(255, 192, 203), // Pink
                Color32::from_rgb(0, 255, 255),   // Cyan
                Color32::from_rgb(255, 20, 147),  // Deep Pink
                Color32::from_rgb(50, 205, 50),   // Lime Green
            ],
            show_grid: true,
            auto_scale: true,
            time_window: 2.0, // Show 2 seconds of data
        }
    }
}

impl PlotArea {
    pub fn ui(&mut self, ui: &mut egui::Ui, device_manager: &DeviceManager) {
        ui.heading("📈 Oscilloscope Display");

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.show_grid, "Show Grid");
            ui.checkbox(&mut self.auto_scale, "Auto Scale");
            ui.separator();
            ui.label("Time Window:");
            ui.add(egui::Slider::new(&mut self.time_window, 0.1..=10.0).suffix("s"));
            ui.separator();
            ui.label("Plot Height:");
            ui.add(egui::Slider::new(&mut self.plot_height, 100.0..=400.0).suffix("px"));
        });

        ui.separator();

        // Use a more efficient scroll area that takes available space
        let available_height = ui.available_height();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false]) // Don't shrink, use full available space
            .max_height(available_height)
            .show(ui, |ui| {
                // Set minimum width to prevent horizontal clipping
                ui.set_min_width(ui.available_width());

                for (device_idx, device) in device_manager.get_devices().iter().enumerate() {
                    if !device.is_connected() {
                        continue;
                    }

                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&device.name).heading().strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(format!("📡 {}", device.hostname));
                                    let status_color = if device.is_connected() {
                                        Color32::GREEN
                                    } else {
                                        Color32::RED
                                    };
                                    ui.colored_label(status_color, "●");
                                },
                            );
                        });

                        // Analog Channel Plot
                        if device.enabled_channels[0] {
                            ui.label(RichText::new("Analog Channel (12-bit)").strong());
                            self.render_analog_plot(ui, device, device_idx);
                        }

                        // Digital Channels Plot
                        let enabled_digital =
                            device.enabled_channels[1..].iter().any(|&enabled| enabled);
                        if enabled_digital {
                            ui.label(RichText::new("Digital Channels").strong());
                            self.render_digital_plot(ui, device, device_idx);
                        }
                    });

                    ui.add_space(10.0);
                }

                if device_manager.get_devices().is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(50.0);
                        ui.label(RichText::new("No devices connected").size(16.0).weak());
                        ui.label("Add a device from the control panel →");
                    });
                }
            });
    }

    fn render_analog_plot(
        &mut self,
        ui: &mut egui::Ui,
        device: &FleaScopeDevice,
        device_idx: usize,
    ) {
        let data_guard = device.data.lock().unwrap();
        let (x_data, y_data) = data_guard.get_analog_data();
        drop(data_guard);

        if x_data.is_empty() {
            ui.label("No data available");
            return;
        }

        let plot = Plot::new(format!("analog_plot_{}", device_idx))
            .height(self.plot_height)
            .show_grid(self.show_grid)
            .auto_bounds([true, true].into())
            .allow_zoom(true)
            .allow_drag(true)
            .allow_scroll(false);

        plot.show(ui, |plot_ui| {
            // Filter data to time window
            let latest_time = x_data.last().copied().unwrap_or(0.0);
            let min_time = latest_time - self.time_window;

            let filtered_data: Vec<[f64; 2]> = x_data
                .iter()
                .zip(y_data.iter())
                .filter(|(x, _)| **x >= min_time)
                .map(|(x, y)| [*x, *y])
                .collect();

            if !filtered_data.is_empty() {
                let filtered_points = PlotPoints::from(filtered_data);
                let line = Line::new(filtered_points)
                    .color(self.colors[0])
                    .width(2.0)
                    .name("Analog");
                plot_ui.line(line);
            }
        });
    }

    fn render_digital_plot(
        &mut self,
        ui: &mut egui::Ui,
        device: &FleaScopeDevice,
        device_idx: usize,
    ) {
        let data_guard = device.data.lock().unwrap();
        let x_data = &data_guard.x_values;

        if x_data.is_empty() {
            ui.label("No data available");
            return;
        }

        let latest_time = x_data.last().copied().unwrap_or(0.0);
        let min_time = latest_time - self.time_window;

        let plot = Plot::new(format!("digital_plot_{}", device_idx))
            .height(self.plot_height * 1.5) // Taller for multiple digital channels
            .show_grid(self.show_grid)
            .auto_bounds([true, true].into())
            .allow_zoom(true)
            .allow_drag(true)
            .allow_scroll(false)
            .y_axis_min_width(40.0);

        plot.show(ui, |plot_ui| {
            for ch in 0..9 {
                if !device.enabled_channels[ch + 1] {
                    continue;
                }

                let (x_data, y_data) = data_guard.get_digital_channel_data(ch);

                let filtered_data: Vec<[f64; 2]> = x_data
                    .iter()
                    .zip(y_data.iter())
                    .filter(|(x, _)| **x >= min_time)
                    .map(|(x, y)| [*x, *y + ch as f64 * 1.2]) // Offset each channel vertically
                    .collect();

                if !filtered_data.is_empty() {
                    let filtered_points = PlotPoints::from(filtered_data);
                    let color_idx = (ch + 1) % self.colors.len();
                    let line = Line::new(filtered_points)
                        .color(self.colors[color_idx])
                        .width(1.5)
                        .name(format!("D{}", ch));
                    plot_ui.line(line);
                }
            }
        });
        drop(data_guard);
    }
}
