use crate::{
    device::DeviceManager,
    worker_interface::{CaptureModeFlat, FleaScopeDevice},
};
use egui::{Color32, RichText};
use egui_plot::{Line, Plot, PlotPoints};
use polars::{
    frame::DataFrame,
    prelude::{col, lit, Column, DataType, IntoLazy},
};
use std::collections::VecDeque;
use std::time::Instant;

#[derive(Clone)]
pub struct ContinuousBuffer {
    data: DataFrame,
    sample_rate_hz: u32,
    last_t: f64,
}

impl ContinuousBuffer {
    pub fn new(sample_rate_hz: u32) -> Self {
        profiling::scope!("ContinuousBuffer::new");

        let bnc_column = Column::new("bnc".into(), [0.0].as_ref());
        let time_index = Column::new("time".into(), [0.0].as_ref());

        let df = DataFrame::new(vec![time_index, bnc_column]).expect("Failed to create DataFrame");

        Self {
            data: df,
            sample_rate_hz,
            last_t: 0.0,
        }
    }

    pub fn add_batch(&mut self, batch: Vec<f64>) {
        profiling::scope!("ContinuousBuffer::add_batch");

        let time_step = 1.0 / self.sample_rate_hz as f64;

        // Create time and BNC value vectors
        let mut time_values = Vec::with_capacity(batch.len());
        let mut bnc_values = Vec::with_capacity(batch.len());

        for &bnc_value in batch.iter() {
            time_values.push(self.last_t);
            bnc_values.push(bnc_value);
            self.last_t += time_step;
        }

        // Create new DataFrame from the batch with time as index
        let time_column = Column::new("time".into(), time_values);
        let bnc_column = Column::new("bnc".into(), bnc_values);

        let batch_df = DataFrame::new(vec![time_column, bnc_column])
            .expect("Failed to create batch DataFrame");

        // Concatenate with existing data
        self.data = self
            .data
            .vstack(&batch_df)
            .expect("Failed to concatenate DataFrames");
    }

    fn cleanup_old_batches(&mut self, keep_time: f64) {
        profiling::scope!("ContinuousBuffer::cleanup_old_batches");

        let keep_samples = (keep_time * self.sample_rate_hz as f64) as usize;
        let current_height = self.data.height();

        if current_height > keep_samples {
            let rows_to_remove = current_height - keep_samples;
            // Keep only the last keep_samples rows
            self.data = self.data.slice(rows_to_remove as i64, keep_samples);
        }
    }

    pub fn get_data_in_window(
        &self,
        window_duration: f64,
        wrap: bool,
        plot_width: u32,
    ) -> (Vec<f64>, Vec<f64>) {
        profiling::scope!("ContinuousBuffer::get_data_in_window");

        if self.data.height() == 0 {
            return (Vec::new(), Vec::new());
        }

        let latest_time = self.last_t;

        let window_start = latest_time - window_duration;
        let time_bin_size = window_duration / plot_width as f64;

        // Use Polars lazy evaluation for efficient filtering and resampling
        let filtered_df = {
            profiling::scope!("polars_filter_and_resample");
            let rmse = self
                .data
                .clone()
                .lazy()
                .filter(col("time").gt_eq(lit(window_start)))
                .select([
                    polars::prelude::col("bnc").std(1),
                ])
                .collect()
                .expect("Failed to filter and resample DataFrame")
                .column("bnc")
                .expect("time column not found")
                .f64()
                .expect("time should be f64")
                .into_no_null_iter()
                .collect::<Vec<_>>();
            tracing::debug!("RMS Error in window: {:?}", rmse);
            let mut df = self
                .data
                .clone()
                .lazy()
                .filter(col("time").gt_eq(lit(window_start)))
                .with_column(
                    (col("time") / lit(time_bin_size))
                        .cast(DataType::Int32)
                        .alias("time_bin"),
                )
                .group_by([col("time_bin")])
                .agg([
                    col("time").min().alias("time_min"),
                    col("time").max().alias("time_max"),
                    col("bnc").min().alias("bnc_min"),
                    col("bnc").median().alias("bnc_median"),
                    col("bnc").mean().alias("bnc_mean"),
                    col("bnc").max().alias("bnc_max"),
                ])
                .sort(
                    ["time_min"],
                    polars::prelude::SortMultipleOptions::default(),
                )
                .with_row_index("idx", None)
                .filter(col("idx").gt(lit(0)).and(col("idx").lt(col("idx").max())));
            if wrap {
                df = df.with_column(col("time_min") % lit(window_duration))
            }
            df.sort(
                ["time_min"],
                polars::prelude::SortMultipleOptions::default(),
            )
            .select([
                polars::prelude::col("time_min").alias("time"),
                polars::prelude::col("bnc_mean").alias("bnc"),
            ])
            .collect()
            .expect("Failed to filter and resample DataFrame")
        };

        // Extract vectors efficiently - handle both resampled and non-resampled data
        // Resampled data - interleave min/max points
        profiling::scope!("extract_resampled_data");

        let time = filtered_df
            .column("time")
            .expect("time column not found")
            .f64()
            .expect("time should be f64")
            .into_no_null_iter()
            .collect::<Vec<_>>();
        let bnc = filtered_df
            .column("bnc")
            .expect("bnc column not found")
            .f64()
            .expect("bnc should be f64")
            .into_no_null_iter()
            .collect::<Vec<_>>();

        (time, bnc)
    }
}
/// RLE-compressed rolling buffer for digital-stream samples.
///
/// Consecutive samples with the same bitmask are merged into a single run
/// entry (start_time, mask).  For typical digital protocol signals this gives
/// orders-of-magnitude compression over per-sample storage.
///
/// Range queries use binary search on the sorted run timestamps for
/// O(log n) range-start discovery, then O(k) scan over k visible runs.
///
/// `get_step_waveform` performs full OR/AND min-max reduction per pixel column
/// so transitions are never missed regardless of zoom level.
pub struct DigitalStreamBuffer {
    /// Start time of each run — SoA layout for cache-efficient binary search.
    run_t: VecDeque<f64>,
    /// Bitmask constant for each run (ends just before the next run starts).
    run_mask: VecDeque<u16>,
    /// Exclusive end of the last run (time after the last sample processed).
    pub last_t: f64,
    /// Windowed sample-rate estimate (samples/s, updated every 0.5 s).
    pub sample_rate_hz: f64,
    configured_rate_hz: Option<f64>,
    /// Maximum number of runs to retain (controls memory usage).
    max_runs: usize,
    rate_samples: u64,
    rate_window_start: Option<Instant>,
}

/// 10 bytes per run in SoA layout (8 for f64 + 2 for u16, no padding waste).
pub const BYTES_PER_RUN: usize = 10;

impl DigitalStreamBuffer {
    const RATE_WINDOW_SECS: f64 = 0.5;

    pub fn new(max_bytes: usize) -> Self {
        Self {
            run_t: VecDeque::new(),
            run_mask: VecDeque::new(),
            last_t: 0.0,
            sample_rate_hz: 10_000.0,
            configured_rate_hz: None,
            max_runs: (max_bytes / BYTES_PER_RUN).max(16),
            rate_samples: 0,
            rate_window_start: None,
        }
    }

    pub fn set_max_bytes(&mut self, max_bytes: usize) {
        self.max_runs = (max_bytes / BYTES_PER_RUN).max(16);
    }

    pub fn set_configured_us_per_sample(&mut self, us: u32) {
        self.configured_rate_hz = if us > 0 {
            Some(1_000_000.0 / us as f64)
        } else {
            None
        };
    }

    /// Number of runs currently stored.
    pub fn run_count(&self) -> usize {
        self.run_t.len()
    }

    /// Approximate bytes used by the run storage.
    pub fn bytes_used(&self) -> usize {
        self.run_t.len() * BYTES_PER_RUN
    }

    pub fn add_batch(&mut self, batch: Vec<u16>) {
        if !batch.is_empty() {
            self.rate_samples += batch.len() as u64;
            let ws = self.rate_window_start.get_or_insert_with(Instant::now);
            let elapsed = ws.elapsed().as_secs_f64();
            if elapsed >= Self::RATE_WINDOW_SECS {
                self.sample_rate_hz = self.rate_samples as f64 / elapsed;
                self.rate_samples = 0;
                self.rate_window_start = Some(Instant::now());
            }
        }

        let dt = 1.0 / self.configured_rate_hz
            .unwrap_or(self.sample_rate_hz)
            .max(1.0);

        for &sample in &batch {
            // RLE: only start a new run when the mask changes.
            match self.run_mask.back() {
                Some(&last) if last == sample => {}
                _ => {
                    self.run_t.push_back(self.last_t);
                    self.run_mask.push_back(sample);
                }
            }
            self.last_t += dt;
        }
    }

    /// Remove runs that have fully expired and enforce the memory cap.
    pub fn cleanup(&mut self, keep_seconds: f64) {
        let cutoff = self.last_t - keep_seconds;
        // A run ends at the start of the next run, so remove run[i] when
        // run_t[i+1] <= cutoff (run[i] is entirely before the cutoff).
        while self.run_t.len() >= 2 && self.run_t[1] <= cutoff {
            self.run_t.pop_front();
            self.run_mask.pop_front();
        }
        // Memory cap.
        while self.run_t.len() > self.max_runs {
            self.run_t.pop_front();
            self.run_mask.pop_front();
        }
    }

    /// Binary search: index of the last run whose start ≤ t.
    fn find_run_at_or_before(&self, t: f64) -> Option<usize> {
        let n = self.run_t.len();
        if n == 0 { return None; }
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.run_t[mid] <= t { lo = mid + 1; } else { hi = mid; }
        }
        if lo > 0 { Some(lo - 1) } else { None }
    }

    /// Build a step waveform with full min-max OR/AND reduction per pixel column.
    ///
    /// Returns egui_plot points ready for `PlotPoints::from`.  Transitions that
    /// occur within a single pixel column produce a vertical segment (same x,
    /// two y values) so no edge is ever silently dropped.
    ///
    /// `y_offset` stacks channels vertically (pass `ch as f64 * 1.2`).
    pub fn get_step_waveform(
        &self,
        channel: usize,
        x_min: f64,
        x_max: f64,
        n_pixels: usize,
        y_offset: f64,
    ) -> Vec<[f64; 2]> {
        if n_pixels == 0 || x_max <= x_min || self.run_t.is_empty() {
            return vec![];
        }

        let px_w = (x_max - x_min) / n_pixels as f64;

        // Find the run active at x_min (its mask is the initial display state).
        let start_idx = self.find_run_at_or_before(x_min).unwrap_or(0);
        let initial_mask = self.run_mask[start_idx];

        // Per-pixel OR/AND accumulators.
        let mut ever_high = vec![false; n_pixels];
        let mut ever_low  = vec![false; n_pixels];
        let mut has_data  = vec![false; n_pixels];

        let n_runs = self.run_t.len();
        let mut i = start_idx;
        while i < n_runs {
            let t_start = self.run_t[i];
            if t_start > x_max { break; }
            let t_end = if i + 1 < n_runs { self.run_t[i + 1] } else { self.last_t };
            let mask  = self.run_mask[i];
            let bit   = (mask >> channel) & 1 != 0;

            // Clip the run to [x_min, x_max] and map to pixel columns.
            let t_lo = t_start.max(x_min);
            let t_hi = t_end.min(x_max);
            if t_lo < t_hi {
                let px_lo = ((t_lo - x_min) / px_w).floor() as usize;
                let px_hi = ((t_hi - x_min) / px_w).ceil()  as usize;
                let px_lo = px_lo.min(n_pixels - 1);
                let px_hi = px_hi.min(n_pixels);
                for px in px_lo..px_hi {
                    has_data[px] = true;
                    if bit { ever_high[px] = true; } else { ever_low[px] = true; }
                }
            }
            i += 1;
        }

        // Build the step-function polyline.
        // Transition columns emit two points at the same x (vertical segment).
        let mut points: Vec<[f64; 2]> = Vec::new();
        let mut cur_y = if (initial_mask >> channel) & 1 != 0 { 1.0f64 } else { 0.0 };
        let mut active = false;

        for px in 0..n_pixels {
            if !has_data[px] { continue; }
            let x = x_min + (px as f64 + 0.5) * px_w;

            match (ever_high[px], ever_low[px]) {
                (true, false) => {
                    if !active || cur_y != 1.0 {
                        if active { points.push([x, cur_y + y_offset]); }
                        points.push([x, 1.0 + y_offset]);
                        cur_y = 1.0; active = true;
                    } else {
                        points.push([x, 1.0 + y_offset]);
                    }
                }
                (false, true) => {
                    if !active || cur_y != 0.0 {
                        if active { points.push([x, cur_y + y_offset]); }
                        points.push([x, 0.0 + y_offset]);
                        cur_y = 0.0; active = true;
                    } else {
                        points.push([x, 0.0 + y_offset]);
                    }
                }
                (true, true) => {
                    // Sub-pixel transition: draw vertical segment, toggle state.
                    let new_y = 1.0 - cur_y;
                    if active {
                        points.push([x, cur_y + y_offset]);
                        points.push([x, new_y + y_offset]);
                    } else {
                        // Direction unknown — show full vertical span.
                        points.push([x, 0.0 + y_offset]);
                        points.push([x, 1.0 + y_offset]);
                    }
                    cur_y = new_y;
                    active = true;
                }
                (false, false) => {}
            }
        }
        points
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
                            let label = if device.get_capture_mode()
                                == CaptureModeFlat::DigitalStream
                            {
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
                let max_bytes = device.get_digital_stream_memory_bytes();
                let buffer = self
                    .digital_stream_buffers
                    .entry(device.name.clone())
                    .or_insert_with(|| DigitalStreamBuffer::new(max_bytes));

                // Sync memory cap (user may have changed the slider).
                buffer.set_max_bytes(max_bytes);
                buffer.set_configured_us_per_sample(
                    device.get_digital_stream_sample_interval_us(),
                );

                while let Ok(batch) = device.digital_batch_rx.try_recv() {
                    buffer.add_batch(batch);
                }
                let window_s = device.get_digital_stream_buffer_window_s();
                buffer.cleanup(window_s * 2.0);

                device.measured_digital_sample_rate_hz = buffer.sample_rate_hz;
                device.digital_stream_bytes_used = buffer.bytes_used();

                let mask     = device.get_digital_stream_channel_mask();
                let enabled  = device.enabled_channels;
                let last_t   = buffer.last_t;
                let n_active = (0..9usize).filter(|&ch| (mask >> ch) & 1 != 0).count();

                if n_active == 0 {
                    ui.label("No active channels");
                    return;
                }

                // Number of rendered pixel columns (use last known plot width).
                let plot_px = self.width as usize;
                let colors  = self.colors.clone();
                let follow  = device.digital_stream_follow_mode;

                let plot = Plot::new(format!("digital_stream_plot_{}", device_idx))
                    .height(self.plot_height * 1.5)
                    .show_grid(self.show_grid)
                    // No auto_bounds — egui_plot persists zoom/pan state.
                    // include_x hints keep the initial view sensible.
                    .include_x(last_t - window_s)
                    .include_x(last_t)
                    .include_y(-0.2)
                    .include_y(n_active as f64 * 1.2)
                    .allow_zoom(true)
                    .allow_drag(true)
                    .allow_scroll(false)
                    .y_axis_min_width(40.0);

                let plot_response = plot.show(ui, |plot_ui| {
                    // Force-follow: override bounds to scroll with new data.
                    if follow {
                        let n_ch = n_active as f64;
                        plot_ui.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                            [last_t - window_s, -0.2],
                            [last_t, n_ch * 1.2],
                        ));
                    }

                    let bounds = plot_ui.plot_bounds();
                    let x_min  = bounds.min()[0];
                    let x_max  = bounds.max()[0];

                    for ch in 0..9usize {
                        if (mask >> ch) & 1 == 0 || !enabled[ch + 1] { continue; }
                        let pts = buffer.get_step_waveform(
                            ch, x_min, x_max, plot_px, ch as f64 * 1.2,
                        );
                        if !pts.is_empty() {
                            let color_idx = (ch + 1) % colors.len();
                            plot_ui.line(
                                Line::new(format!("D{}", ch), PlotPoints::from(pts))
                                    .color(colors[color_idx])
                                    .width(1.5),
                            );
                        }
                    }
                });

                self.width = plot_response.response.rect.width() as u32;

                // Unfollow when the user drags or scrolls the plot.
                if follow && plot_response.response.dragged() {
                    device.digital_stream_follow_mode = false;
                }
                if follow
                    && plot_response.response.hovered()
                    && ui.ctx().input(|i| i.zoom_delta() != 1.0 || i.raw_scroll_delta != egui::Vec2::ZERO)
                {
                    device.digital_stream_follow_mode = false;
                }
            }
        }
    }
}
