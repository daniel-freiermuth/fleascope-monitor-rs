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

#[derive(Clone)]
pub struct ContinuousBuffer {
    data: DataFrame,
    sample_rate_hz: u32,
    last_t: f64,
}

impl ContinuousBuffer {
    pub fn new(sample_rate_hz: u32) -> Self {
        #[cfg(feature = "puffin")]
        puffin::profile_function!();

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
        #[cfg(feature = "puffin")]
        puffin::profile_function!();

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
        #[cfg(feature = "puffin")]
        puffin::profile_function!();

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
        #[cfg(feature = "puffin")]
        puffin::profile_function!();

        if self.data.height() == 0 {
            return (Vec::new(), Vec::new());
        }

        let latest_time = self.last_t;

        let window_start = latest_time - window_duration;
        let time_bin_size = window_duration / plot_width as f64;

        // Use Polars lazy evaluation for efficient filtering and resampling
        let filtered_df = {
            #[cfg(feature = "puffin")]
            puffin::profile_scope!("polars_filter_and_resample");
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
                polars::prelude::col("bnc_median").alias("bnc"),
            ])
            .collect()
            .expect("Failed to filter and resample DataFrame")
        };

        // Extract vectors efficiently - handle both resampled and non-resampled data
        // Resampled data - interleave min/max points
        #[cfg(feature = "puffin")]
        puffin::profile_scope!("extract_resampled_data");

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
        #[cfg(feature = "puffin")]
        puffin::profile_function!();

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
        #[cfg(feature = "puffin")]
        puffin::profile_scope!("get_plot_data");

        match &device.get_capture_mode() {
            CaptureModeFlat::Continuous => {
                // Process any new batches from the channel
                {
                    #[cfg(feature = "puffin")]
                    puffin::profile_scope!("process_channel_batches");
                    let device_name = &device.name;
                    let buffer = self
                        .continuous_buffers
                        .entry(device_name.clone())
                        .or_insert_with(|| {
                            #[cfg(feature = "puffin")]
                            puffin::profile_scope!("create_new_buffer");
                            ContinuousBuffer::new(51_436)
                        }); // 1 second max buffer

                    while let Ok(batch) = device.batch_rx.try_recv() {
                        #[cfg(feature = "puffin")]
                        puffin::profile_scope!("add_single_batch");
                        tracing::debug!("Received batch with {} points", batch.len());
                        // Get or create buffer for this device
                        buffer.add_batch(batch);
                    }
                    tracing::debug!("Cleaning up old batches");
                    buffer.cleanup_old_batches(device.get_continuous_config().buffer_time);
                }
                #[cfg(feature = "puffin")]
                puffin::profile_scope!("continuous_mode_data");

                let device_name = &device.name;

                // Get windowed data from our channel-fed buffer
                if let Some(buffer) = self.continuous_buffers.get(device_name) {
                    #[cfg(feature = "puffin")]
                    puffin::profile_scope!("buffer_windowed_data");
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
                #[cfg(feature = "puffin")]
                puffin::profile_scope!("triggered_mode_data");
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
        #[cfg(feature = "puffin")]
        puffin::profile_function!();

        let (x_data, y_data) = self.get_analog_data(device);

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

        let plot_response = plot.show(ui, |plot_ui| {
            let filtered_data: Vec<[f64; 2]> = x_data
                .iter()
                .zip(y_data.iter())
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
                return;
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

                        let (x_data, y_data) = data.get_digital_channel_data(ch);

                        let filtered_data: Vec<[f64; 2]> = x_data
                            .iter()
                            .zip(y_data.iter())
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
            }
        }
    }
}
