use anyhow::{anyhow, Error, Result};
use arc_swap::ArcSwap;
use fleascope_rs::flea_scope::{
    BITMAP_COLUMN_NAME, CALIBRATED_COLUMN_NAME, RAW_COLUMN_NAME, TIME_COLUMN_NAME,
};
use fleascope_rs::trigger_config::TriggerConfig as _;
use fleascope_rs::{FleaProbe, IdleFleaScope, ProbeType};
use polars::frame::DataFrame;
use polars::prelude::{IntoLazy, UInt16Chunked};
use polars::series::IntoSeries;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

use crate::device::{
    CaptureConfig, CaptureMode, ControlCommand, DataPoint, DeviceData, Notification, TriggerConfig,
    TriggerSource, WaveformConfig, WorkerStatus,
};

pub struct FleaWorker {
    pub data: Arc<ArcSwap<DeviceData>>,
    /// Single source of truth for the worker's state; only the worker loop sends.
    pub status_tx: watch::Sender<WorkerStatus>,
    pub config_change_rx: watch::Receiver<CaptureConfig>,
    pub control_rx: tokio::sync::mpsc::Receiver<ControlCommand>,
    pub notification_tx: tokio::sync::mpsc::Sender<Notification>,
    pub waveform_rx: tokio::sync::watch::Receiver<WaveformConfig>,
    pub x1: FleaProbe,
    pub x10: FleaProbe,
    pub batch_tx: tokio::sync::mpsc::Sender<Vec<f64>>,
}

impl FleaWorker {
    /// Fails only when the UI side dropped its receiver, i.e. the device was removed.
    async fn notify(&self, notification: Notification) -> Result<()> {
        self.notification_tx
            .send(notification)
            .await
            .map_err(|_| anyhow!("notification receiver dropped"))
    }

    /// `FleaScopeDevice` owns the watch senders; once it is dropped (device removed)
    /// there is nobody left to serve and the worker must shut down.
    fn ui_dropped(&self) -> bool {
        self.config_change_rx.has_changed().is_err() || self.waveform_rx.has_changed().is_err()
    }

    fn probe_mut(&mut self, probe_type: ProbeType) -> (&mut FleaProbe, &'static str) {
        match probe_type {
            ProbeType::X1 => (&mut self.x1, "X1"),
            ProbeType::X10 => (&mut self.x10, "X10"),
        }
    }

    async fn handle_control_command(
        &mut self,
        command: ControlCommand,
        fleascope: &mut IdleFleaScope,
    ) -> Result<()> {
        tracing::info!("Handling control command: {:?}", command);

        match command {
            ControlCommand::Calibrate0V(probe_type) => {
                let (probe, name) = self.probe_mut(probe_type);
                let notification = match probe.calibrate_0(fleascope) {
                    Ok(_) => Notification::Success(format!("{name} probe calibrated at 0V")),
                    Err(e) => Notification::Error(format!("{name} calibration failed: {e}")),
                };
                self.notify(notification).await?;
            }
            ControlCommand::Calibrate3V(probe_type) => {
                let (probe, name) = self.probe_mut(probe_type);
                let notification = match probe.calibrate_3v3(fleascope) {
                    Ok(_) => Notification::Success(format!("{name} probe calibrated at 3.3V")),
                    Err(e) => Notification::Error(format!("{name} calibration failed: {e}")),
                };
                self.notify(notification).await?;
            }
            ControlCommand::StoreCalibration() => {
                let notification = match Ok(())
                    .and(self.x1.write_calibration_to_flash(fleascope))
                    .and(self.x10.write_calibration_to_flash(fleascope))
                {
                    Ok(_) => Notification::Success("Calibration saved successfully".to_string()),
                    Err(e) => Notification::Error(format!("Failed to save calibration: {e}")),
                };
                self.notify(notification).await?;
            }
            ControlCommand::Exit => {
                tracing::info!("Exiting FleaWorker");
                return Err(Error::msg("Exiting FleaWorker")); // Handle exit logic if needed
            }
            ControlCommand::Pause => {
                self.set_as_paused();
            }
            ControlCommand::Resume => {
                self.set_as_running();
            }
        };
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.status_tx.borrow().running
    }

    fn set_as_paused(&mut self) {
        tracing::info!("Setting FleaWorker as paused");
        self.status_tx.send_modify(|status| {
            status.running = false;
            status.update_rate = 0.0;
        });
    }

    async fn set_lost_connection(&mut self) {
        tracing::info!("Lost connection");
        self.status_tx.send_replace(WorkerStatus {
            running: false,
            connected: false,
            update_rate: 0.0,
        });
        if let Err(e) = self
            .notify(Notification::Error(
                "Lost connection to the device.".to_string(),
            ))
            .await
        {
            tracing::warn!("Could not report lost connection: {e:#}");
        }
    }

    fn set_as_running(&mut self) {
        tracing::info!("Setting FleaWorker as running");
        self.status_tx.send_modify(|status| status.running = true);
    }

    /// A successful read proves the device is reachable.
    fn mark_connected(&self) {
        self.status_tx.send_if_modified(|status| {
            let changed = !status.connected;
            status.connected = true;
            changed
        });
    }

    pub async fn run(&mut self, fleascope: IdleFleaScope) {
        tracing::info!("FleaWorker started");
        match self.capture_loop(fleascope).await {
            Some(fleascope) => fleascope.teardown(),
            None => self.wait_for_removal().await,
        }
        self.status_tx.send_replace(WorkerStatus {
            running: false,
            connected: false,
            update_rate: 0.0,
        });
        tracing::info!("FleaWorker stopped");
    }

    /// Returns the scope on a clean stop, `None` once the connection is lost.
    async fn capture_loop(&mut self, mut fleascope: IdleFleaScope) -> Option<IdleFleaScope> {
        let mut last_rate_update = Instant::now();
        let mut read_count = 0;

        loop {
            if let Ok(command) = self.control_rx.try_recv() {
                tracing::info!("Received control command: {:?}", command);
                if let Err(e) = self.handle_control_command(command, &mut fleascope).await {
                    tracing::info!("FleaWorker stopping: {e:#}");
                    break;
                }
                continue;
            }
            if self.ui_dropped() {
                tracing::info!("Device handle dropped, stopping FleaWorker");
                break;
            }
            if self.waveform_rx.has_changed().unwrap_or(false) {
                tracing::info!("Waveform configuration changed, updating waveform");
                let waveform_config = self.waveform_rx.borrow_and_update().clone();
                fleascope.set_waveform(waveform_config.waveform_type, waveform_config.frequency_hz);
            }

            if !self.is_running() {
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
                        if let Err(e) = self.handle_control_command(command, &mut fleascope).await {
                            tracing::info!("FleaWorker stopping: {e:#}");
                            break;
                        }
                    }
                }
                continue;
            }

            tracing::debug!("Device is running, starting data generation");
            let capture_config = self.config_change_rx.borrow_and_update().clone();
            let capture = match capture_config.mode {
                CaptureMode::Triggered {
                    trigger_config,
                    time_frame,
                } => {
                    self.handle_triggered_capture(
                        capture_config.probe_multiplier,
                        time_frame,
                        trigger_config,
                        fleascope,
                    )
                    .await
                }
                CaptureMode::Continuous {} => {
                    self.handle_continuous_capture(capture_config.probe_multiplier, fleascope)
                        .await
                }
            };
            fleascope = match capture {
                Ok(fleascope) => fleascope,
                Err(e) => {
                    tracing::error!("Capture failed: {e:#}");
                    return None;
                }
            };

            if last_rate_update.elapsed() >= Duration::from_secs(1) {
                let update_rate = read_count as f64 / last_rate_update.elapsed().as_secs_f64();
                // A pause during the capture above already reset the rate; don't overwrite it.
                self.status_tx.send_if_modified(|status| {
                    let changed = status.running && status.update_rate != update_rate;
                    if changed {
                        status.update_rate = update_rate;
                    }
                    changed
                });
                read_count = 0;
                last_rate_update = Instant::now();
            }
            read_count += 1;
        }
        Some(fleascope)
    }

    /// After a lost connection the scope is gone, but `FleaScopeDevice` still holds the
    /// senders and `.expect`s on every send. Keep the receivers alive and drain commands
    /// until the device is removed (`Exit`) or its handle is dropped.
    async fn wait_for_removal(&mut self) {
        loop {
            tokio::select! {
                command = self.control_rx.recv() => match command {
                    None | Some(ControlCommand::Exit) => break,
                    Some(ControlCommand::Pause) => {}
                    Some(command) => {
                        tracing::warn!("Ignoring {command:?}: device disconnected");
                        let notification = Notification::Error(
                            "Device disconnected. Remove it and add it again.".to_string(),
                        );
                        if self.notify(notification).await.is_err() {
                            break;
                        }
                    }
                },
                changed = self.config_change_rx.changed() => if changed.is_err() { break },
                changed = self.waveform_rx.changed() => if changed.is_err() { break },
            }
        }
    }

    /// Returns `Err` only when the connection is lost; the scope is consumed then.
    async fn handle_triggered_capture(
        &mut self,
        probe: ProbeType,
        time_frame: f64,
        trigger_config: TriggerConfig,
        idle_scope: IdleFleaScope,
    ) -> Result<IdleFleaScope> {
        let probe = match probe {
            ProbeType::X1 => &self.x1,
            ProbeType::X10 => &self.x10,
        };
        let probe_clone = probe.clone(); // Clone early to avoid borrowing issues
        let trigger_str = {
            profiling::scope!("trigger_string_conversion");

            match trigger_config.source {
                TriggerSource::Analog => {
                    tracing::debug!("Converting analog trigger to string");
                    trigger_config
                        .analog
                        .into_trigger(probe)
                        .map(|trigger| trigger.into_trigger_fields())
                }
                TriggerSource::Digital => {
                    tracing::debug!("Converting digital trigger to string");
                    Ok(trigger_config.digital.into_trigger_fields())
                }
            }
        };

        let trigger_str = match trigger_str {
            Ok(str) => str,
            Err(e) => {
                tracing::error!("Failed to convert trigger to string: {}", e);
                if let Err(e) = self
                    .notify(Notification::Error(format!(
                        "Invalid trigger configuration: {}",
                        e
                    )))
                    .await
                {
                    tracing::warn!("Could not report invalid trigger: {e:#}");
                }
                self.set_as_paused();
                return Ok(idle_scope);
            }
        };

        let star_res = {
            profiling::scope!("hardware_read_async");

            idle_scope.read_async(Duration::from_secs_f64(time_frame), trigger_str, None)
        };
        let mut fleascope_for_read = match star_res {
            Ok(fleascope_for_read) => fleascope_for_read,
            Err((s, e)) => {
                tracing::error!("Failed to start read operation: {}", e);
                return Ok(s);
            }
        };
        tracing::debug!("Successfully started read operation on FleaScope");

        loop {
            match fleascope_for_read.try_get_result() {
                Ok(Ok((scope, reading))) => {
                    self.mark_connected();
                    let data_copy = self.data.clone();
                    tokio::spawn(async move {
                        profiling::scope!("data_processing_pipeline");

                        let _parse_csv_scope = {
                            profiling::scope!("parse_csv");
                            reading
                                .parse_csv()
                                .map(|df| {
                                    profiling::scope!("apply_calibration");
                                    probe_clone.apply_calibration(df).collect().unwrap()
                                })
                                .map(|df| {
                                    profiling::scope!("convert_to_data_points");
                                    FleaWorker::convert_polars_to_data_points(df)
                                })
                        };

                        _parse_csv_scope
                            .map(|data_points| {
                                profiling::scope!("update_shared_data");

                                let new_data = DeviceData {
                                    x_values: data_points.0,
                                    data_points: data_points.1,
                                    last_update: Instant::now(),
                                };
                                data_copy.store(Arc::new(new_data));
                            })
                            .ok();
                    });
                    return Ok(scope);
                }
                Ok(Err(scope)) => {
                    fleascope_for_read = scope;
                }
                Err(_) => {
                    tracing::error!("Error during hardware read: Connection lost");
                    self.set_lost_connection().await;
                    return Err(Error::msg("Connection lost during hardware read"));
                }
            }
            profiling::scope!("hardware_wait_polling_loop");

            if self.check_settings_changed() {
                tracing::info!("Settings changed during hardware wait, calling unblock()");
                return Ok(fleascope_for_read.cancel());
            }
        }
    }

    /// A closed channel counts as a change so the capture unblocks and `run` can shut down.
    fn check_settings_changed(&self) -> bool {
        if self.config_change_rx.has_changed().unwrap_or(true) {
            profiling::scope!("config_change_detected");

            tracing::info!("Configuration changed during hardware read, calling unblock()");
            return true;
        }
        if self.waveform_rx.has_changed().unwrap_or(true) {
            profiling::scope!("waveform_change_detected");

            tracing::info!("Waveform changed during hardware read, calling unblock()");
            return true;
        }
        if !self.control_rx.is_empty() {
            profiling::scope!("control_command_detected");

            tracing::info!("Received control command during hardware read");
            return true;
        }
        false
    }

    /// Returns `Err` only when the connection is lost; the scope is dropped then.
    async fn handle_continuous_capture(
        &mut self,
        probe: ProbeType,
        fleascope: IdleFleaScope,
    ) -> Result<IdleFleaScope> {
        const BUFF_SIZE: usize = 512;
        let probe = match probe {
            ProbeType::X1 => &self.x1,
            ProbeType::X10 => &self.x10,
        };

        let mut streaming_scope = fleascope.stream();
        let mut start_time = Instant::now();
        // let mut total_samples = 0u32;
        loop {
            let batch_result = {
                profiling::scope!("read_batch_data");

                streaming_scope.read(BUFF_SIZE).map(|buffer| {
                    // Process the buffer data here
                    let ca = UInt16Chunked::from_vec(RAW_COLUMN_NAME.into(), buffer);
                    let column = ca.into_series().into();
                    let df = DataFrame::new(vec![column]).unwrap();
                    probe
                        .apply_calibration(df.lazy())
                        .collect()
                        .unwrap()
                        .column(CALIBRATED_COLUMN_NAME)
                        .unwrap()
                        .f64()
                        .unwrap()
                        .into_no_null_iter()
                        .collect()
                })
            };

            match batch_result {
                Ok(batch) => {
                    self.mark_connected();
                    {
                        profiling::scope!("send_batch_channel");

                        match self.batch_tx.try_send(batch) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                tracing::debug!("Batch channel full - dropping batch (UI not draining fast enough)");
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                tracing::warn!("Failed to send batch - receiver has been dropped");
                            }
                        }
                    }
                    if self.check_settings_changed() {
                        return Ok(streaming_scope.stop());
                    }
                }
                // fleascope-rs treats only BrokenPipe/UnexpectedEof as connection loss and
                // `cancel()` tolerates TimedOut, so keep the stop-and-pause path for timeouts.
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    tracing::error!("Timed out reading from continuous stream: {}", e);
                    self.set_lost_connection().await;
                    return Ok(streaming_scope.stop());
                }
                Err(e) => {
                    tracing::error!("Error reading from continuous stream: {}", e);
                    self.set_lost_connection().await;
                    // `stop()` writes CTRL-C to the dead port and panics inside
                    // fleascope-rs (`BusyFleaTerminal::cancel`), so drop the stream instead.
                    drop(streaming_scope);
                    return Err(Error::new(e).context("Connection lost during continuous read"));
                }
            }
            let now = Instant::now();
            //total_samples += BUFF_SIZE as u32;
            tracing::info!(
                "Read {} samples in {}ms = {}Hz from continuous stream",
                BUFF_SIZE,
                (now - start_time).as_millis(),
                BUFF_SIZE as f64 / (now - start_time).as_secs_f64()
            );
            start_time = now;
        }
    }

    fn convert_polars_to_data_points(df: DataFrame) -> (Vec<f64>, Vec<DataPoint>) {
        profiling::scope!("FleaWorker::convert_polars_to_data_points");

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
        profiling::scope!("extract_dataframe_columns");

        let time_col = match df.column(TIME_COLUMN_NAME) {
            Ok(col) => col,
            Err(e) => {
                tracing::error!("Failed to get time column: {}", e);
                panic!("Time column not found in DataFrame");
            }
        };

        let bnc_col = match df.column(CALIBRATED_COLUMN_NAME) {
            Ok(col) => col,
            Err(e) => {
                tracing::error!("Failed to get bnc column: {}", e);
                panic!("BNC column not found in DataFrame");
            }
        };

        let bitmap_col = match df.column(BITMAP_COLUMN_NAME) {
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
            profiling::scope!("parse_bitmap_strings");

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

        profiling::scope!("create_data_points_from_vectors");

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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_worker() -> (FleaWorker, watch::Receiver<WorkerStatus>) {
        let (_config_tx, config_change_rx) = watch::channel(CaptureConfig {
            probe_multiplier: ProbeType::X1,
            mode: CaptureMode::Continuous {},
        });
        let (_control_tx, control_rx) = tokio::sync::mpsc::channel(1);
        let (notification_tx, _notification_rx) = tokio::sync::mpsc::channel(1);
        let (_waveform_tx, waveform_rx) = watch::channel(WaveformConfig::default());
        let (batch_tx, _batch_rx) = tokio::sync::mpsc::channel(1);
        let (status_tx, status_rx) = watch::channel(WorkerStatus {
            running: true,
            connected: true,
            update_rate: 0.0,
        });
        let worker = FleaWorker {
            data: Arc::new(ArcSwap::new(Arc::new(DeviceData {
                x_values: Vec::new(),
                data_points: Vec::new(),
                last_update: Instant::now(),
            }))),
            status_tx,
            config_change_rx,
            control_rx,
            notification_tx,
            waveform_rx,
            x1: FleaProbe::new(ProbeType::X1),
            x10: FleaProbe::new(ProbeType::X10),
            batch_tx,
        };
        (worker, status_rx)
    }

    /// The UI derives the RUN/STOP button from the worker status channel
    /// (control_panel.rs). Resume after Pause must publish `running = true`,
    /// otherwise the device stays "paused" in the UI forever.
    #[test]
    fn resume_after_pause_publishes_running_to_status() {
        let (mut worker, status_rx) = test_worker();

        worker.set_as_paused();
        assert!(!status_rx.borrow().running);

        worker.set_as_running();
        assert!(
            status_rx.borrow().running,
            "Resume must publish running=true to the worker status"
        );
    }

    /// `test_worker` drops every UI-side handle, exactly like
    /// `DeviceManager::remove_device` does. The worker must notice and unblock
    /// instead of panicking (release builds use `panic = "abort"`).
    #[test]
    fn dropped_device_handle_unblocks_capture_instead_of_panicking() {
        let (worker, _status_rx) = test_worker();

        assert!(worker.ui_dropped());
        assert!(worker.check_settings_changed());
    }

    #[test]
    fn live_device_handle_with_unchanged_settings_keeps_capturing() {
        let (mut worker, _status_rx) = test_worker();
        let (_config_tx, config_change_rx) = watch::channel(CaptureConfig {
            probe_multiplier: ProbeType::X1,
            mode: CaptureMode::Continuous {},
        });
        let (_waveform_tx, waveform_rx) = watch::channel(WaveformConfig::default());
        worker.config_change_rx = config_change_rx;
        worker.waveform_rx = waveform_rx;

        assert!(!worker.ui_dropped());
        assert!(!worker.check_settings_changed());
    }

    #[tokio::test]
    async fn lost_connection_is_published_even_without_notification_receiver() {
        let (mut worker, status_rx) = test_worker();

        worker.set_lost_connection().await;

        let status = *status_rx.borrow();
        assert!(!status.connected);
        assert!(!status.running);
    }

    /// After a lost connection the device stays in the UI, which `.expect`s on every
    /// send to the worker (worker_interface.rs). The worker must keep its receivers
    /// alive until the device is removed, rejecting commands it cannot serve.
    #[tokio::test]
    async fn disconnected_worker_keeps_ui_channels_open_until_exit() {
        let (mut worker, status_rx) = test_worker();
        let (config_tx, config_change_rx) = watch::channel(CaptureConfig {
            probe_multiplier: ProbeType::X1,
            mode: CaptureMode::Continuous {},
        });
        let (waveform_tx, waveform_rx) = watch::channel(WaveformConfig::default());
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(32);
        let (notification_tx, mut notification_rx) = tokio::sync::mpsc::channel(32);
        worker.config_change_rx = config_change_rx;
        worker.waveform_rx = waveform_rx;
        worker.control_rx = control_rx;
        worker.notification_tx = notification_tx;

        worker.set_lost_connection().await;
        let task = tokio::spawn(async move { worker.wait_for_removal().await });

        control_tx.try_send(ControlCommand::Resume).unwrap();
        config_tx
            .send(CaptureConfig {
                probe_multiplier: ProbeType::X10,
                mode: CaptureMode::Continuous {},
            })
            .unwrap();
        waveform_tx.send(WaveformConfig::default()).unwrap();

        // Lost-connection report, then the rejected Resume.
        for _ in 0..2 {
            assert!(matches!(
                notification_rx.recv().await,
                Some(Notification::Error(_))
            ));
        }
        assert!(!status_rx.borrow().running);
        assert!(!task.is_finished());

        control_tx.try_send(ControlCommand::Exit).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop on Exit")
            .unwrap();
    }
}
