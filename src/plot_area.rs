use crate::{
    device::DeviceManager,
    worker_interface::{CaptureModeFlat, FleaScopeDevice},
};
use egui::{Color32, RichText};
use egui_plot::{Line, Plot, PlotPoints};
use std::collections::VecDeque;
use std::time::Instant;

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

/// Bitmask summary of consecutive digital samples.
#[derive(Clone, Copy)]
struct DigitalBin {
    t: f64,
    /// Channels high in every sample of the bin.
    all_high: u16,
    /// Channels high in at least one sample; differs from `all_high` where a channel toggled.
    any_high: u16,
    count: usize,
}

/// Rolling buffer for digital-stream samples, decimated into bins that keep every
/// edge: a channel that toggled inside a bin is drawn as a vertical line.
/// Sample rate is estimated using a 0.5 s windowed counter so that draining
/// multiple batches in the same render frame does not produce a near-zero
/// elapsed time and a false Msps reading.
pub struct DigitalStreamBuffer {
    bins: VecDeque<DigitalBin>,
    /// Bin still collecting samples; carries over between packets.
    pending: Option<DigitalBin>,
    last_t: f64,
    /// Estimated samples / second (from windowed counter); 0 until measured.
    pub sample_rate_hz: f64,
    /// Exact rate from the FW config; `None` for free-run (us_per_sample == 0).
    configured_rate_hz: Option<f64>,
    /// Display window in seconds; kept in sync from the plot arm each frame.
    window_s: f64,
    /// Accumulated sample count since the rate window opened.
    rate_samples: u64,
    /// When the current rate-measurement window opened.
    rate_window_start: Option<Instant>,
    last_batch: Option<Instant>,
}

impl DigitalStreamBuffer {
    const RATE_WINDOW_SECS: f64 = 0.5;
    /// Target display density: MAX_POINTS bins per `window_s`.
    const MAX_POINTS: usize = 2048;
    /// Memory safety cap: 65 536 bins × 32 bytes ≈ 2 MB.
    const MEMORY_CAP: usize = 65_536;

    pub fn new() -> Self {
        Self {
            bins: VecDeque::new(),
            pending: None,
            last_t: 0.0,
            sample_rate_hz: 0.0,
            configured_rate_hz: None,
            window_s: 1.0,
            rate_samples: 0,
            rate_window_start: None,
            last_batch: None,
        }
    }

    pub fn set_configured_us_per_sample(&mut self, us: u32) {
        let configured = (us > 0).then(|| 1_000_000.0 / f64::from(us));
        if configured != self.configured_rate_hz {
            // The stream restarts at the new rate; the old estimate no longer applies.
            self.reset_rate();
        }
        self.configured_rate_hz = configured;
    }

    pub fn set_window_s(&mut self, window_s: f64) {
        self.window_s = window_s.max(0.001);
    }

    /// Forget the rate estimate once batches stop arriving (paused, stopped or
    /// failed stream), so it neither shows a stale value nor spans the gap.
    pub fn expire_idle_rate(&mut self) {
        if self
            .last_batch
            .is_some_and(|t| t.elapsed().as_secs_f64() >= Self::RATE_WINDOW_SECS)
        {
            self.reset_rate();
        }
    }

    fn reset_rate(&mut self) {
        self.sample_rate_hz = 0.0;
        self.rate_samples = 0;
        self.rate_window_start = None;
        self.last_batch = None;
    }

    pub fn add_batch(&mut self, batch: &[u16]) {
        if batch.is_empty() {
            return;
        }
        self.expire_idle_rate();
        self.last_batch = Some(Instant::now());
        self.rate_samples += batch.len() as u64;
        let window_start = self.rate_window_start.get_or_insert_with(Instant::now);
        let elapsed = window_start.elapsed().as_secs_f64();
        if elapsed >= Self::RATE_WINDOW_SECS {
            self.sample_rate_hz = self.rate_samples as f64 / elapsed;
            self.rate_samples = 0;
            self.rate_window_start = Some(Instant::now());
        }

        // Free-run samples can't be placed on the time axis until the rate is measured.
        let rate = match self.configured_rate_hz {
            Some(rate) => rate,
            None if self.sample_rate_hz > 0.0 => self.sample_rate_hz,
            None => return,
        };
        let dt = 1.0 / rate;
        let step = ((rate * self.window_s / Self::MAX_POINTS as f64) as usize).max(1);

        for &sample in batch {
            let bin = self.pending.get_or_insert(DigitalBin {
                t: self.last_t,
                all_high: u16::MAX,
                any_high: 0,
                count: 0,
            });
            bin.all_high &= sample;
            bin.any_high |= sample;
            bin.count += 1;
            if bin.count >= step {
                self.bins.push_back(*bin);
                self.pending = None;
            }
            self.last_t += dt;
        }
    }

    pub fn cleanup(&mut self, keep_seconds: f64) {
        let cutoff = self.last_t - keep_seconds;
        while self.bins.front().is_some_and(|bin| bin.t < cutoff) {
            self.bins.pop_front();
        }
        let excess = self.bins.len().saturating_sub(Self::MEMORY_CAP);
        self.bins.drain(..excess);
    }

    /// Return (x_times, y_values) for `channel` (0-8) over the last `window_s` seconds.
    /// At most 2 × MAX_POINTS entries: a bin where the channel toggled yields both levels.
    pub fn get_channel_data(&self, channel: usize, window_s: f64) -> (Vec<f64>, Vec<f64>) {
        let cutoff = self.last_t - window_s;
        let level = |mask: u16| f64::from((mask >> channel) & 1);
        let mut x = Vec::new();
        let mut y = Vec::new();
        for bin in self.bins.iter().filter(|bin| bin.t >= cutoff) {
            let t = bin.t - cutoff;
            let (low, high) = (level(bin.all_high), level(bin.any_high));
            if low == high {
                x.push(t);
                y.push(low);
            } else {
                // Draw the edge starting from the level the line is already at.
                let from = y.last().copied().unwrap_or(low);
                x.extend([t, t]);
                y.extend([from, 1.0 - from]);
            }
        }
        (x, y)
    }
}

pub struct PlotArea {
    plot_height: f32,
    colors: Vec<Color32>,
    show_grid: bool,
    continuous_buffers: std::collections::HashMap<String, ContinuousBuffer>,
    digital_stream_buffers: std::collections::HashMap<String, DigitalStreamBuffer>,
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
            digital_stream_buffers: std::collections::HashMap::new(),
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

                        // Analog Channel Plot (not available in Digital Stream mode)
                        if device.enabled_channels[0]
                            && device.get_capture_mode() != CaptureModeFlat::DigitalStream
                        {
                            ui.label(RichText::new("Analog Channel (12-bit)").strong());
                            self.render_analog_plot(ui, device, device_idx);
                        }

                        // Digital Channels Plot
                        let enabled_digital =
                            device.enabled_channels[1..].iter().any(|&enabled| enabled);
                        if enabled_digital
                            || device.get_capture_mode() == CaptureModeFlat::DigitalStream
                        {
                            let label =
                                if device.get_capture_mode() == CaptureModeFlat::DigitalStream {
                                    "Digital Stream"
                                } else {
                                    "Digital Channels"
                                };
                            ui.label(RichText::new(label).strong());
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
            CaptureModeFlat::DigitalStream => {
                // No analog channel in digital stream mode
                (vec![], vec![])
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
        device: &mut FleaScopeDevice,
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
                    .height(self.plot_height * 1.5)
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
                            .map(|(x, y)| [*x, *y + ch as f64 * 1.2])
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
            CaptureModeFlat::DigitalStream => {
                let buffer = self
                    .digital_stream_buffers
                    .entry(device.name.clone())
                    .or_insert_with(DigitalStreamBuffer::new);

                let window_s = device.get_digital_stream_buffer_window_s();
                // Sync rate and window before draining so step is correct for
                // this frame's batches.
                buffer.set_configured_us_per_sample(device.get_digital_stream_sample_interval_us());
                buffer.set_window_s(window_s);

                while let Ok(batch) = device.digital_batch_rx.try_recv() {
                    buffer.add_batch(&batch);
                }
                buffer.cleanup(window_s * 2.0);
                buffer.expire_idle_rate();

                // Propagate the live rate estimate so the control panel can display it.
                device.measured_digital_sample_rate_hz = buffer.sample_rate_hz;

                let mask = device.get_digital_stream_channel_mask();
                let has_data =
                    (0..9).any(|ch| (mask >> ch) & 1 != 0 && device.enabled_channels[ch + 1]);

                if !has_data {
                    ui.label("No active channels");
                    return;
                }

                let plot = Plot::new(format!("digital_stream_plot_{}", device_idx))
                    .height(self.plot_height * 1.5)
                    .show_grid(self.show_grid)
                    .auto_bounds([true, true])
                    .allow_zoom(true)
                    .allow_drag(true)
                    .allow_scroll(false)
                    .y_axis_min_width(40.0);

                // Snapshot data outside the closure to avoid borrow conflict
                let channel_data: Vec<Option<(Vec<f64>, Vec<f64>)>> = (0..9)
                    .map(|ch| {
                        if (mask >> ch) & 1 == 0 || !device.enabled_channels[ch + 1] {
                            return None;
                        }
                        Some(buffer.get_channel_data(ch, window_s))
                    })
                    .collect();

                let colors = self.colors.clone();
                plot.show(ui, |plot_ui| {
                    for (ch, data_opt) in channel_data.into_iter().enumerate() {
                        if let Some((x_data, y_data)) = data_opt {
                            let filtered_data: Vec<[f64; 2]> = x_data
                                .iter()
                                .zip(y_data.iter())
                                .map(|(x, y)| [*x, *y + ch as f64 * 1.2])
                                .collect();
                            if !filtered_data.is_empty() {
                                let filtered_points = PlotPoints::from(filtered_data);
                                let color_idx = (ch + 1) % colors.len();
                                let line = Line::new(format!("D{}", ch), filtered_points)
                                    .color(colors[color_idx])
                                    .width(1.5);
                                plot_ui.line(line);
                            }
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

    #[test]
    fn digital_decimation_keeps_single_sample_pulse() {
        // 100 kHz over 1 s: 48 samples per bin, fed in 56-sample packets.
        let mut buf = DigitalStreamBuffer::new();
        buf.set_configured_us_per_sample(10);
        let mut samples = vec![0u16; 560];
        samples[200] = 0b1; // one-sample pulse on D0, inside the 5th bin
        for packet in samples.chunks(56) {
            buf.add_batch(packet);
        }

        let (times, d0) = buf.get_channel_data(0, 1.0);
        let (_, d1) = buf.get_channel_data(1, 1.0);

        // Bins span packets evenly: 560 / 48 = 11 full bins, the pulse bin drawn as an edge.
        assert_eq!(times.len(), 12);
        assert_eq!(d0.iter().filter(|&&v| v > 0.5).count(), 1);
        assert_eq!(d1.len(), 11);
        assert!(d1.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn free_run_digital_samples_wait_for_a_measured_rate() {
        let mut buf = DigitalStreamBuffer::new();
        buf.set_configured_us_per_sample(0);
        buf.add_batch(&[0b1; 56]);

        assert!(buf.sample_rate_hz == 0.0, "no rate is known yet");
        assert!(buf.get_channel_data(0, 1.0).0.is_empty());
    }
}
