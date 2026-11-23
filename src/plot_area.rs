use crate::{
    device::DeviceManager,
    worker_interface::{CaptureModeFlat, FleaScopeDevice},
};
use egui::{Color32, RichText};
use egui_plot::{Line, Plot, PlotPoints};
use std::collections::VecDeque;

#[derive(Clone)]
pub struct ContinuousBuffer {
    samples: VecDeque<f64>,
    sample_rate_hz: u32,
    /// Samples received since creation; the global index of the next sample.
    total_samples: u64,
}

/// Running mean of the samples falling into one plot bin.
struct Bin {
    id: i64,
    t_min: f64,
    sum: f64,
    count: u32,
}

impl Bin {
    fn mean(&self) -> f64 {
        self.sum / f64::from(self.count)
    }
}

impl ContinuousBuffer {
    pub fn new(sample_rate_hz: u32) -> Self {
        Self {
            samples: VecDeque::new(),
            sample_rate_hz,
            total_samples: 0,
        }
    }

    pub fn add_batch(&mut self, batch: Vec<f64>) {
        profiling::scope!("ContinuousBuffer::add_batch");

        self.total_samples += batch.len() as u64;
        self.samples.extend(batch);
    }

    fn cleanup_old_batches(&mut self, keep_time: f64) {
        profiling::scope!("ContinuousBuffer::cleanup_old_batches");

        let keep_samples = (keep_time * self.sample_rate_hz as f64) as usize;
        let excess = self.samples.len().saturating_sub(keep_samples);
        self.samples.drain(..excess);
    }

    /// Mean-decimates the last `window_duration` seconds to about one point per pixel.
    /// Bins are aligned to absolute time so they stay put while the signal scrolls.
    pub fn get_data_in_window(
        &self,
        window_duration: f64,
        wrap: bool,
        plot_width: u32,
    ) -> (Vec<f64>, Vec<f64>) {
        profiling::scope!("ContinuousBuffer::get_data_in_window");

        let rate = f64::from(self.sample_rate_hz);
        let window_len = ((window_duration * rate).round() as usize).min(self.samples.len());
        let skip = self.samples.len() - window_len;
        let first_index = self.total_samples - self.samples.len() as u64 + skip as u64;
        // Bin ids come from integer sample indices so float error never moves a
        // sample across a bin edge.
        let samples_per_bin = window_duration * rate / f64::from(plot_width);

        let mut bins: Vec<Bin> = Vec::with_capacity(plot_width as usize + 2);
        for (i, &v) in self.samples.range(skip..).enumerate() {
            let index = first_index + i as u64;
            let id = (index as f64 / samples_per_bin).floor() as i64;
            match bins.last_mut() {
                Some(bin) if bin.id == id => {
                    bin.sum += v;
                    bin.count += 1;
                }
                _ => bins.push(Bin {
                    id,
                    t_min: index as f64 / rate,
                    sum: v,
                    count: 1,
                }),
            }
        }

        // The first and last bins are only partially filled.
        let full_bins = bins
            .get(1..bins.len().saturating_sub(1))
            .unwrap_or_default();
        let mut points: Vec<(f64, f64)> = full_bins.iter().map(|b| (b.t_min, b.mean())).collect();
        if wrap {
            for (t, _) in &mut points {
                *t %= window_duration;
            }
            points.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        points.into_iter().unzip()
    }
}

pub struct PlotArea {
    plot_height: f32,
    colors: Vec<Color32>,
    show_grid: bool,
    continuous_buffers: std::collections::HashMap<String, ContinuousBuffer>, // Per-device buffers
    width: u32,
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
            continuous_buffers: std::collections::HashMap::new(),
            width: 1500,
        }
    }
}

impl PlotArea {
    pub fn ui(&mut self, ui: &mut egui::Ui, device_manager: &mut DeviceManager) {
        profiling::scope!("PlotArea::ui");

        ui.heading("📈 Oscilloscope Display");

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.show_grid, "Show Grid");
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

                for (device_idx, device) in device_manager.get_devices_mut().iter_mut().enumerate()
                {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&device.name).heading().strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(format!("📡 {}", device.name));
                                    let status_color = Color32::GREEN; // Default to green
                                                                       /*
                                                                       let status_color = if device.is_connected() {
                                                                           Color32::GREEN
                                                                       } else {
                                                                           Color32::RED
                                                                       };
                                                                       */
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

    fn get_analog_data(&mut self, device: &mut FleaScopeDevice) -> (Vec<f64>, Vec<f64>) {
        profiling::scope!("get_plot_data");

        match &device.get_capture_mode() {
            CaptureModeFlat::Continuous => {
                // Process any new batches from the channel
                {
                    profiling::scope!("process_channel_batches");
                    let device_name = &device.name;
                    let buffer = self
                        .continuous_buffers
                        .entry(device_name.clone())
                        .or_insert_with(|| {
                            profiling::scope!("create_new_buffer");
                            ContinuousBuffer::new(51_436)
                        }); // 1 second max buffer

                    while let Ok(batch) = device.batch_rx.try_recv() {
                        profiling::scope!("add_single_batch");
                        tracing::debug!("Received batch with {} points", batch.len());
                        // Get or create buffer for this device
                        buffer.add_batch(batch);
                    }
                    tracing::debug!("Cleaning up old batches");
                    buffer.cleanup_old_batches(device.get_continuous_config().buffer_time);
                }
                profiling::scope!("continuous_mode_data");

                let device_name = &device.name;

                // Get windowed data from our channel-fed buffer
                if let Some(buffer) = self.continuous_buffers.get(device_name) {
                    profiling::scope!("buffer_windowed_data");
                    buffer.get_data_in_window(
                        device.get_continuous_config().buffer_time,
                        device.wrap,
                        self.width,
                    )
                } else {
                    (vec![], vec![])
                }
            }
            CaptureModeFlat::Triggered => {
                let data = device.data.load();
                profiling::scope!("triggered_mode_data");
                data.get_analog_data()
            }
        }
    }

    fn render_analog_plot(
        &mut self,
        ui: &mut egui::Ui,
        device: &mut FleaScopeDevice,
        device_idx: usize,
    ) {
        profiling::scope!("PlotArea::render_analog_plot");

        let (x_data, y_data) = self.get_analog_data(device);

        if x_data.is_empty() {
            ui.label("No data available");
            return;
        }

        let plot = Plot::new(format!("analog_plot_{}", device_idx))
            .height(self.plot_height)
            .show_grid(self.show_grid)
            .auto_bounds([true, true])
            .allow_zoom(true)
            .allow_drag(true)
            .allow_scroll(false);

        let plot_response = plot.show(ui, |plot_ui| {
            let filtered_data: Vec<[f64; 2]> = x_data
                .iter()
                .zip(y_data.iter())
                .map(|(x, y)| [*x, *y])
                .collect();

            if !filtered_data.is_empty() {
                let filtered_points = PlotPoints::from(filtered_data);
                let line = Line::new("Analog", filtered_points)
                    .color(self.colors[0])
                    .width(2.0);
                plot_ui.line(line);
            }
        });
        self.width = plot_response.response.rect.width() as u32;
    }

    fn render_digital_plot(
        &mut self,
        ui: &mut egui::Ui,
        device: &FleaScopeDevice,
        device_idx: usize,
    ) {
        match device.get_capture_mode() {
            CaptureModeFlat::Continuous => {
                ui.label("Digital plotting not supported in Continuous mode");
            }
            CaptureModeFlat::Triggered => {
                let data = device.data.load();
                let x_data = &data.x_values;

                if x_data.is_empty() {
                    ui.label("No data available");
                    return;
                }

                let plot = Plot::new(format!("digital_plot_{}", device_idx))
                    .height(self.plot_height * 1.5) // Taller for multiple digital channels
                    .show_grid(self.show_grid)
                    .auto_bounds([true, true])
                    .allow_zoom(true)
                    .allow_drag(true)
                    .allow_scroll(false)
                    .y_axis_min_width(40.0);

                plot.show(ui, |plot_ui| {
                    for ch in 0..9 {
                        if !device.enabled_channels[ch + 1] {
                            continue;
                        }

                        let (x_data, y_data) = data.get_digital_channel_data(ch);

                        let filtered_data: Vec<[f64; 2]> = x_data
                            .iter()
                            .zip(y_data.iter())
                            .map(|(x, y)| [*x, *y + ch as f64 * 1.2]) // Offset each channel vertically
                            .collect();

                        if !filtered_data.is_empty() {
                            let filtered_points = PlotPoints::from(filtered_data);
                            let color_idx = (ch + 1) % self.colors.len();
                            let line = Line::new(format!("D{}", ch), filtered_points)
                                .color(self.colors[color_idx])
                                .width(1.5);
                            plot_ui.line(line);
                        }
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After ~33 min of streaming at 1 MHz, absolute bin ids exceed `i32::MAX`
    /// for the smallest window; binning must still produce the window.
    #[test]
    fn bins_window_after_long_capture() {
        let mut buf = ContinuousBuffer::new(1_000_000);
        buf.total_samples = 2_000_000_000;
        buf.add_batch(vec![1.0; 1000]);

        let (times, values) = buf.get_data_in_window(0.001, false, 1500);

        // Bins are narrower than a sample: one point per sample minus both edge bins.
        assert_eq!(times.len(), 998);
        assert_eq!(values.len(), 998);
        assert!(times.iter().all(|&t| t >= 2000.0));
        assert!(values.iter().all(|&v| (v - 1.0).abs() < f64::EPSILON));
    }

    #[test]
    fn decimates_to_bin_means_without_partial_edge_bins() {
        // 1 kHz, 1 s window, 100 px: 10 samples per bin alternating 0/2.
        let mut buf = ContinuousBuffer::new(1000);
        buf.add_batch((0..1000).map(|i| f64::from(i % 2) * 2.0).collect());

        let (times, values) = buf.get_data_in_window(1.0, false, 100);

        assert_eq!(times.len(), 98);
        assert!(times.windows(2).all(|w| w[0] < w[1]));
        assert!(values.iter().all(|&v| (v - 1.0).abs() < 1e-9));
    }
}
