use anyhow::{Error, Result};
use arc_swap::ArcSwap;
use fleascope_rs::{FleaProbe, IdleFleaScope, ProbeType};
use polars::prelude::*;
use std::panic;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::time::sleep;

use crate::device::{
    CaptureConfig, ControlCommand, DataPoint, DeviceData, Notification, TriggerConfig, WaveformConfig
};

pub struct FleaWorker {
    pub data: Arc<ArcSwap<DeviceData>>, // Changed to Arc<ArcSwap> for sharing between threads
    pub config_change_rx: watch::Receiver<CaptureConfig>, // Channel for configuration changes
    pub control_rx: tokio::sync::mpsc::Receiver<ControlCommand>, // Channel for calibration commands
    pub notification_tx: tokio::sync::mpsc::Sender<Notification>, // Channel for calibration results
    pub waveform_rx: tokio::sync::watch::Receiver<WaveformConfig>, // Channel for waveform configuration
    pub x1: FleaProbe,
    pub x10: FleaProbe,
    pub running: bool,
}

impl FleaWorker {
    async fn handle_control_command(
        &mut self,
        command: ControlCommand,
        fleascope: &mut IdleFleaScope,
    ) -> Result<()> {
        tracing::info!("Handling control command: {:?}", command);

        match command {
            ControlCommand::Calibrate0V(probe_multiplier) => match probe_multiplier {
                ProbeType::X1 => match self.x1.calibrate_0(fleascope) {
                    Ok(_) => {
                        self.notification_tx
                            .send(Notification::Success(
                                "X1 probe calibrated at 0V".to_string(),
                            ))
                            .await
                            .expect("Failed to send calibration result");
                    }
                    Err(e) => self
                        .notification_tx
                        .send(Notification::Error(format!("X1 calibration failed: {}", e)))
                        .await
                        .expect("Failed to send calibration result"),
                },
                ProbeType::X10 => match self.x10.calibrate_0(fleascope) {
                    Ok(_) => {
                        self.notification_tx
                            .send(Notification::Success(
                                "X10 probe calibrated at 0V".to_string(),
                            ))
                            .await
                            .expect("Failed to send calibration result");
                    }
                    Err(e) => self
                        .notification_tx
                        .send(Notification::Error(format!(
                            "X10 calibration failed: {}",
                            e
                        )))
                        .await
                        .expect("Failed to send calibration result"),
                },
            },
            ControlCommand::Calibrate3V(probe_multiplier) => match probe_multiplier {
                ProbeType::X1 => match self.x1.calibrate_3v3(fleascope) {
                    Ok(_) => {
                        self.notification_tx
                            .send(Notification::Success(
                                "X1 probe calibrated at 3.3V".to_string(),
                            ))
                            .await
                            .expect("Failed to send calibration result");
                    }
                    Err(e) => self
                        .notification_tx
                        .send(Notification::Error(format!("X1 calibration failed: {}", e)))
                        .await
                        .expect("Failed to send calibration result"),
                },
                ProbeType::X10 => match self.x10.calibrate_3v3(fleascope) {
                    Ok(_) => {
                        self.notification_tx
                            .send(Notification::Success(
                                "X10 probe calibrated at 3.3V".to_string(),
                            ))
                            .await
                            .expect("Failed to send calibration result");
                    }
                    Err(e) => self
                        .notification_tx
                        .send(Notification::Error(format!(
                            "X10 calibration failed: {}",
                            e
                        )))
                        .await
                        .expect("Failed to send calibration result"),
                },
            },
            ControlCommand::StoreCalibration() => {
                match Ok(())
                    .and(self.x1.write_calibration_to_flash(fleascope))
                    .and(self.x10.write_calibration_to_flash(fleascope))
                {
                    Ok(_) => self
                        .notification_tx
                        .blocking_send(Notification::Success(
                            "Calibration saved successfully".to_string(),
                        ))
                        .expect("Failed to send calibration save success"),
                    Err(e) => self
                        .notification_tx
                        .blocking_send(Notification::Error(format!(
                            "Failed to save calibration: {}",
                            e
                        )))
                        .expect("Failed to send calibration save error"),
                }
            }
            ControlCommand::Exit => {
                tracing::info!("Exiting FleaWorker");
                return Err(Error::msg("Exiting FleaWorker")); // Handle exit logic if needed
            }
            ControlCommand::Pause => {
                self.set_as_paused().await;
            }
            ControlCommand::Resume => {
                self.set_as_running();
            }
            ControlCommand::Step => {
                tracing::info!("Stepping FleaWorker");
                // Implement step logic if needed, e.g., trigger a single read
                // This could be a no-op if stepping is not supported
            }
        };
        Ok(())
    }

    async fn set_as_paused(&mut self) {
        tracing::info!("Setting FleaWorker as paused");
        self.running = false;
        sleep(Duration::from_millis(20)).await;
        let data = self.data.load();
        self.data.store(Arc::new(DeviceData {
            x_values: data.x_values.clone(),
            data_points: data.data_points.clone(),
            last_update: data.last_update,
            update_rate: 0.0,
            connected: true,
            running: self.running,
        }));
    }

    async fn set_lost_connection(&mut self) {
        tracing::info!("Lost connection");
        self.notification_tx
            .send(Notification::Error(
                "Lost connection to the device.".to_string(),
            ))
            .await
            .expect("Failed to send read error notification");
        self.running = false;
        sleep(Duration::from_millis(20)).await;
        let data = self.data.load();
        self.data.store(Arc::new(DeviceData {
            x_values: data.x_values.clone(),
            data_points: data.data_points.clone(),
            last_update: data.last_update,
            update_rate: 0.0,
            connected: false,
            running: self.running,
        }));
    }

    fn set_as_running(&mut self) {
        tracing::info!("Setting FleaWorker as running");
        self.running = true;
    }

    pub async fn run(&mut self, mut fleascope: IdleFleaScope) -> Result<()> {
        tracing::info!("FleaWorker started");
        let mut update_rate = 0.0;
        let mut last_rate_update = Instant::now();
        let mut read_count = 0;

        loop {
            // Global profiler frame marker
            #[cfg(feature = "puffin")]
            puffin::GlobalProfiler::lock().new_frame();

            if let Ok(command) = self.control_rx.try_recv() {
                tracing::info!("Received control command: {:?}", command);
                if (self.handle_control_command(command, &mut fleascope).await).is_err() {
                    break;
                }
            }
            if self
                .waveform_rx
                .has_changed()
                .expect("Failed to check for waveform config change")
            {
                tracing::info!("Waveform configuration changed, updating waveform");
                let waveform_config = self.waveform_rx.borrow_and_update().clone();
                fleascope.set_waveform(waveform_config.waveform_type, waveform_config.frequency_hz);
            }

            tracing::debug!("Starting new data generation iteration");
            // Check if device is paused first
            let capture_config = self.config_change_rx.borrow_and_update().clone();
            if !self.running {
                tracing::debug!("Device is paused, skipping data generation");

                // During pause, still check for config changes and calibration commands
                tokio::select! {
                    signal = self.waveform_rx.changed() => {
                        if signal.is_ok() {
                            tracing::info!("Configuration changed while paused, will restart");
                        }
                    }
                    Some(command) = self.control_rx.recv() => {
                        tracing::info!("Received calibration command while paused: {:?}", command);
                        if (self.handle_control_command(command, &mut fleascope).await).is_err() {
                            break;
                        }
                    }
                }
                continue;
            }

            tracing::debug!("Device is running, starting data generation");
            fleascope = self.handle_triggered_capture(update_rate, capture_config.probe_multiplier, capture_config.time_frame, capture_config.trigger_config, fleascope).await;
            {
                #[cfg(feature = "puffin")]
                puffin::profile_scope!("update_rate_calculation");

                if last_rate_update.elapsed() >= Duration::from_secs(1) {
                    update_rate = read_count as f64 / last_rate_update.elapsed().as_secs_f64();
                    read_count = 0;
                    last_rate_update = Instant::now();
                }
                read_count += 1;
            }
        }
        fleascope.teardown();
        let data = self.data.load();
        self.data.store(Arc::new(DeviceData {
            x_values: data.x_values.clone(),
            data_points: data.data_points.clone(),
            last_update: data.last_update,
            update_rate: 0.0,
            connected: false,
            running: false,
        }));
        Err(Error::msg("FleaWorker exited"))
    }

    async fn handle_triggered_capture(&mut self, update_rate: f64, probe: ProbeType, time_frame: f64, trigger_config: TriggerConfig, idle_scope: IdleFleaScope) -> IdleFleaScope {
        let probe = match probe {
            ProbeType::X1 => &self.x1,
            ProbeType::X10 => &self.x10,
        };
        let probe_clone = probe.clone(); // Clone early to avoid borrowing issues
        let trigger_str = {
            #[cfg(feature = "puffin")]
            puffin::profile_scope!("trigger_string_conversion");

            probe.trigger_to_string(trigger_config.into())
        };

        let trigger_str = match trigger_str {
            Ok(str) => str,
            Err(e) => {
                tracing::error!("Failed to convert trigger to string: {}", e);
                self.notification_tx
                    .blocking_send(Notification::Error(format!(
                        "Invalid trigger configuration: {}",
                        e
                    )))
                    .expect("Failed to send error notification");
                self.set_as_paused().await;
                return idle_scope;
            }
        };

        let star_res = {
            #[cfg(feature = "puffin")]
            puffin::profile_scope!("hardware_read_async");

            idle_scope.read_async(
                Duration::from_secs_f64(time_frame),
                trigger_str,
                None,
            )
        };
        let mut fleascope_for_read = match star_res {
            Ok(fleascope_for_read) => fleascope_for_read,
            Err((s, e)) => {
                tracing::error!("Failed to start read operation: {}", e);
                return s;
            }
        };
        tracing::debug!("Successfully started read operation on FleaScope");

        while !fleascope_for_read.is_done() {
            #[cfg(feature = "puffin")]
            puffin::profile_scope!("hardware_wait_polling_loop");

            if self
                .config_change_rx
                .has_changed()
                .expect("Failed to check for config change")
            {
                #[cfg(feature = "puffin")]
                puffin::profile_scope!("config_change_detected");

                tracing::info!("Configuration changed during hardware read, calling unblock()");
                fleascope_for_read.cancel();
                break;
            }
            if self
                .waveform_rx
                .has_changed()
                .expect("Failed to check for waveform change")
            {
                #[cfg(feature = "puffin")]
                puffin::profile_scope!("waveform_change_detected");

                tracing::info!("Waveform changed during hardware read, calling unblock()");
                fleascope_for_read.cancel();
                break;
            }
            if !self.control_rx.is_empty() {
                #[cfg(feature = "puffin")]
                puffin::profile_scope!("control_command_detected");

                tracing::info!("Received control command during hardware read");
                fleascope_for_read.cancel();
                break;
            };
        }

        let (idle_scope, res) = {
            #[cfg(feature = "puffin")]
            puffin::profile_scope!("hardware_wait_completion");

            fleascope_for_read.wait()
        };
        let (f, data_s) = match res {
            Ok((data_s, f)) => (data_s, f),
            Err(_e) => {
                self.set_lost_connection().await;
                return idle_scope;
            }
        };

        let data_copy = self.data.clone();
        let running = self.running;
        tokio::spawn(async move {
            #[cfg(feature = "puffin")]
            puffin::profile_scope!("data_processing_pipeline");

            let _parse_csv_scope = {
                #[cfg(feature = "puffin")]
                puffin::profile_scope!("parse_csv");
                IdleFleaScope::parse_csv(&data_s, f)
                    .map(|df| {
                        #[cfg(feature = "puffin")]
                        puffin::profile_scope!("apply_calibration");
                        probe_clone.apply_calibration(df).collect().unwrap()
                    })
                    .map(|df| {
                        #[cfg(feature = "puffin")]
                        puffin::profile_scope!("convert_to_data_points");
                        FleaWorker::convert_polars_to_data_points(df)
                    })
            };

            _parse_csv_scope
                .map(|data_points| {
                    #[cfg(feature = "puffin")]
                    puffin::profile_scope!("update_shared_data");

                    let new_data = DeviceData {
                        x_values: data_points.0,
                        data_points: data_points.1,
                        last_update: Instant::now(),
                        update_rate,
                        connected: true,
                        running,
                    };
                    data_copy.store(Arc::new(new_data));
                })
                .ok();
        });
        idle_scope
    }

    fn convert_polars_to_data_points(df: DataFrame) -> (Vec<f64>, Vec<DataPoint>) {
        #[cfg(feature = "puffin")]
        puffin::profile_function!();

        tracing::debug!(
            "Converting DataFrame with columns: {:?}",
            df.get_column_names()
        );
        tracing::debug!(
            "DataFrame shape: {} rows, {} columns",
            df.height(),
            df.width()
        );

        // Extract columns from the DataFrame
        #[cfg(feature = "puffin")]
        puffin::profile_scope!("extract_dataframe_columns");

        let time_col = match df.column("time") {
            Ok(col) => col,
            Err(e) => {
                tracing::error!("Failed to get time column: {}", e);
                panic!("Time column not found in DataFrame");
            }
        };

        let bnc_col = match df.column("bnc") {
            Ok(col) => col,
            Err(e) => {
                tracing::error!("Failed to get bnc column: {}", e);
                panic!("BNC column not found in DataFrame");
            }
        };

        let bitmap_col = match df.column("bitmap") {
            Ok(col) => col,
            Err(e) => {
                tracing::error!("Failed to get bitmap column: {}", e);
                panic!("Bitmap column not found in DataFrame");
            }
        };

        let time_values: Vec<f64> = match time_col.f64() {
            Ok(chunked) => chunked.into_no_null_iter().collect(),
            Err(e) => {
                tracing::error!("Failed to convert time column to f64: {}", e);
                panic!("Time column conversion failed");
            }
        };

        let bnc_values: Vec<f64> = match bnc_col.f64() {
            Ok(chunked) => chunked.into_no_null_iter().collect(),
            Err(e) => {
                tracing::error!("Failed to convert bnc column to f64: {}", e);
                panic!("BNC column conversion failed");
            }
        };

        // Convert bitmap column - handle both string and numeric formats
        let bitmap_values: Vec<u16> = if bitmap_col.dtype() == &polars::datatypes::DataType::String
        {
            #[cfg(feature = "puffin")]
            puffin::profile_scope!("parse_bitmap_strings");

            // Handle string bitmap data (e.g., "0x1ff", "0101010101", or "255")
            // TODO maybe use the fleascope-rs function
            match bitmap_col.str() {
                Ok(chunked) => {
                    let mut values = Vec::new();
                    for opt_str in chunked.into_iter() {
                        match opt_str {
                            Some(s) => {
                                // Hexadecimal string like "0x1ff"
                                match u16::from_str_radix(&s[2..], 16) {
                                    Ok(val) => values.push(val),
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to parse hex string '{}': {}",
                                            s,
                                            e
                                        );
                                        panic!("Invalid bitmap hex string");
                                    }
                                }
                            }
                            None => {
                                tracing::error!("Found null bitmap value");
                                panic!("Null bitmap value encountered");
                            }
                        }
                    }
                    values
                }
                Err(e) => {
                    tracing::error!("Failed to convert bitmap column to string: {}", e);
                    panic!("Bitmap column conversion failed");
                }
            }
        } else {
            panic!("Bitmap column is not a string type, expected string or numeric format");
        };

        tracing::debug!(
            "Extracted {} time values, {} BNC values, {} bitmap values",
            time_values.len(),
            bnc_values.len(),
            bitmap_values.len()
        );

        tracing::debug!(
            "Successfully converted DataFrame to vectors, processing {} data points",
            time_values.len()
        );

        #[cfg(feature = "puffin")]
        puffin::profile_scope!("create_data_points_from_vectors");

        let mut x_values = Vec::new();
        let mut data_points = Vec::new();

        for ((time, bnc), bitmap) in time_values
            .iter()
            .zip(bnc_values.iter())
            .zip(bitmap_values.iter())
        {
            x_values.push(*time);

            // Extract digital channels from bitmap
            let mut digital_channels = [false; 9];
            for (i, ch) in digital_channels.iter_mut().enumerate() {
                *ch = (bitmap & (1 << i)) != 0;
            }

            data_points.push(DataPoint {
                analog_channel: *bnc,
                digital_channels,
            });
        }

        tracing::debug!("Converted to {} data points", data_points.len());
        (x_values, data_points)
    }
}
