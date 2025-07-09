use std::{sync::Arc, time::{Duration, Instant}};

use anyhow::{Error, Result};
use arc_swap::ArcSwap;
use fleascope_rs::{FleaProbe, IdleFleaScope, ProbeType};
use polars::frame::DataFrame;
use tokio::sync::watch;

use crate::device::{CaptureConfig, ControlCommand, DataPoint, DeviceData, Notification, WaveformConfig};


pub struct FleaWorker {
    pub fleascope: IdleFleaScope,
    pub data: Arc<ArcSwap<DeviceData>>, // Changed to Arc<ArcSwap> for sharing between threads
    pub config_change_rx: watch::Receiver<CaptureConfig>, // Channel for configuration changes
    pub control_rx: tokio::sync::mpsc::Receiver<ControlCommand>, // Channel for calibration commands
    pub notification_tx: tokio::sync::mpsc::Sender<Notification>, // Channel for calibration results
    pub waveform_rx: tokio::sync::watch::Receiver<WaveformConfig>, // Channel for waveform configuration
    pub x1: FleaProbe,
    pub x10: FleaProbe,
}

impl FleaWorker {
    /// Handle calibration commands received from the UI
    async fn handle_control_command(&mut self, command: ControlCommand) -> Result<()> {
        tracing::info!("Handling calibration command: {:?}", command);
        
        match command {
            ControlCommand::Calibrate0V(probe_multiplier) => {
                match probe_multiplier {
                    ProbeType::X1 => match self.x1.calibrate_0(&mut self.fleascope) {
                        Ok(_) => {},
                        Err(e) => self.notification_tx.send(Notification::Error(format!("Calibration failed: {}", e))).await.expect("Failed to send calibration result"),
                    },
                    ProbeType::X10 => match self.x10.calibrate_0(&mut self.fleascope) {
                        Ok(_) => {},
                        Err(e) => self.notification_tx.send(Notification::Error(format!("Calibration failed: {}", e))).await.expect("Failed to send calibration result"),

                    }
                };
                if let Err(e) = self.notification_tx.send(Notification::Success("Calibration completed successfully".to_string())).await {
                    tracing::error!("Failed to send calibration result: {}", e);
                }
            }
            ControlCommand::Calibrate3V(probe_multiplier) => {
                match probe_multiplier {
                    ProbeType::X1 => self.x1.calibrate_3v3(&mut self.fleascope),
                    ProbeType::X10 => self.x10.calibrate_3v3(&mut self.fleascope),
                };
                if let Err(e) = self.notification_tx.send(Notification::Success("Calibration completed successfully".to_string())).await {
                    tracing::error!("Failed to send calibration result: {}", e);
                }
            }
            ControlCommand::StoreCalibration() => {
                self.x1.write_calibration_to_flash(&mut self.fleascope);
                self.x10.write_calibration_to_flash(&mut self.fleascope);
                if let Err(e) = self.notification_tx.send(Notification::Success("Calibration completed successfully".to_string())).await {
                    tracing::error!("Failed to send calibration result: {}", e);
                }
            },
            ControlCommand::Exit => {
                tracing::info!("Exiting FleaWorker");
                return Err(Error::msg("Exiting FleaWorker")); // Handle exit logic if needed
            },
        };
        Ok(())
    }

    async fn set_lost_connection(&mut self) {
        tracing::info!("Lost connection");
        self.notification_tx
            .send(Notification::Error(
                "Lost connection to the device.".to_string(),
            ))
            .await
            .expect("Failed to send read error notification");
        let data = self.data.load();
        self.data.store(Arc::new(DeviceData {
            x_values: data.x_values.clone(),
            data_points: data.data_points.clone(),
            last_update: data.last_update,
            update_rate: 0.0,
            read_duration: Duration::from_secs_f32(0.0),
        }));
    }


    pub fn start_data_generation(mut self) -> tokio::task::JoinHandle<()> {
        // Create a new receiver for configuration changes
        let mut update_rate = 0.0;
        let mut last_rate_update = Instant::now();
        let mut read_count = 0;

        // Start the cancellation-aware data generation loop
        tokio::spawn(async move {
            tracing::debug!("Starting cancellation-aware data generation loop");
            loop {
                match self.control_rx.try_recv() {
                    Ok(command) => {
                        tracing::info!("Received calibration command while paused: {:?}", command);
                        match self.handle_control_command(command).await {
                            Err(_) => break,
                            _ => {}
                        }
                    },
                    _ => {}
                }
                if self.waveform_rx.has_changed().expect("Failed to check for waveform config change") {
                    tracing::info!("Waveform configuration changed, updating waveform");
                    let waveform_config = self.waveform_rx.borrow_and_update().clone();
                    self.fleascope.set_waveform(waveform_config.waveform_type, waveform_config.frequency_hz); 
                }
            
                tracing::debug!("Starting new data generation iteration");
                // Check if device is paused first
                let capture_config = self.config_change_rx.borrow_and_update().clone();
                if capture_config.is_paused {
                    tracing::debug!("Device is paused, skipping data generation");
                    
                    // During pause, still check for config changes and calibration commands
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                        signal = self.config_change_rx.changed() => {
                            if signal.is_ok() {
                                tracing::info!("Configuration changed while paused, will restart");
                            }
                        }
                        signal = self.waveform_rx.changed() => {
                            if signal.is_ok() {
                                tracing::info!("Configuration changed while paused, will restart");
                            }
                        }
                        Some(command) = self.control_rx.recv() => {
                            tracing::info!("Received calibration command while paused: {:?}", command);
                            match self.handle_control_command(command).await {
                                Err(_) => break,
                                _ => {},
                            }
                        }
                    }
                    continue;
                }

                tracing::debug!("Device is running, starting data generation");
                
                let start_time = Instant::now();
                let probe = match capture_config.probe_multiplier {
                    ProbeType::X1 => &self.x1,
                    ProbeType::X10 => &self.x10,
                };
                let probe_clone = probe.clone(); // Clone early to avoid borrowing issues

                let trigger_str = match probe.trigger_to_string(capture_config.trigger_config.into()) {
                    Ok(str) => str,
                    Err(e) => {
                        tracing::error!("Failed to convert trigger to string: {}", e);
                        continue
                    }
                };

                let star_res = self.fleascope.read_async(
                    Duration::from_secs_f64(capture_config.time_frame),
                    trigger_str,
                    None
                );
                match star_res {
                    Ok(mut fleascope_for_read) => {
                        tracing::debug!("Successfully started read operation on FleaScope");

                        while !fleascope_for_read.is_done() {
                            if self.config_change_rx.has_changed().expect("Failed to check for config change") {
                                tracing::info!("Configuration changed during hardware read, calling unblock()");
                                fleascope_for_read.cancel();
                                break;
                            }
                            if self.waveform_rx.has_changed().expect("Failed to check for waveform change") {
                                tracing::info!("Waveform changed during hardware read, calling unblock()");
                                fleascope_for_read.cancel();
                                break;
                            }
                            if !self.control_rx.is_empty() {
                                fleascope_for_read.cancel();
                                break;
                            };
                        }
                        let read_duration = start_time.elapsed();
                        
                        if last_rate_update.elapsed() >= Duration::from_secs(1) {
                            update_rate = read_count as f64 / last_rate_update.elapsed().as_secs_f64();
                            read_count = 0;
                            last_rate_update = Instant::now();
                        }
                        read_count += 1;

                        let (idle_scope, res) = fleascope_for_read.wait();
                        self.fleascope = idle_scope;
                        let (f, data_s) = match res {
                            Ok((data_s, f)) => (data_s, f),
                            Err(_e) => {
                                self.set_lost_connection().await;
                                break;
                            }
                        };

                        let data_copy = self.data.clone();
                        tokio::spawn(async move {
                            IdleFleaScope::parse_csv(&data_s, f)
                                .map(|df| probe_clone.apply_calibration(df).collect().unwrap())
                                .map(|df| FleaWorker::convert_polars_to_data_points(df))
                                .map(|data_points| {
                                    // Update data with lock-free operation using ArcSwap
                                    let new_data = DeviceData {
                                        x_values : data_points.0,
                                        data_points : data_points.1,
                                        last_update : Instant::now(),
                                        read_duration : read_duration,
                                        update_rate,
                                    };
                                    data_copy.store(Arc::new(new_data));
                                }).ok();
                        });
                    }
                    Err((s, e)) => {
                        tracing::error!("Failed to start read operation: {}", e);
                        self.fleascope = s; // Restore idle scope on error
                    }
                }
            }
            self.fleascope.teardown();
        })
        
    }

    fn convert_polars_to_data_points(df: DataFrame) -> (Vec<f64>, Vec<DataPoint>) {
        tracing::debug!("Converting DataFrame with columns: {:?}", df.get_column_names());
        tracing::debug!("DataFrame shape: {} rows, {} columns", df.height(), df.width());
        
        // Extract columns from the DataFrame
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
        let bitmap_values: Vec<u16> = if bitmap_col.dtype() == &polars::datatypes::DataType::String {
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
                                        tracing::error!("Failed to parse hex string '{}': {}", s, e);
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

        tracing::debug!("Extracted {} time values, {} BNC values, {} bitmap values", 
                       time_values.len(), bnc_values.len(), bitmap_values.len());

        tracing::debug!("Successfully converted DataFrame to vectors, processing {} data points", time_values.len());

        let mut x_values = Vec::new();
        let mut data_points = Vec::new();

        for ((time, bnc), bitmap) in time_values.iter().zip(bnc_values.iter()).zip(bitmap_values.iter()) {
            x_values.push(*time);
            
            // Extract digital channels from bitmap
            let mut digital_channels = [false; 9];
            for i in 0..9 {
                digital_channels[i] = (bitmap & (1 << i)) != 0;
            }

            data_points.push(DataPoint {
                timestamp: *time,
                analog_channel: *bnc,
                digital_channels,
            });
        }

        tracing::debug!("Converted to {} data points", data_points.len());
        (x_values, data_points)
    }

}