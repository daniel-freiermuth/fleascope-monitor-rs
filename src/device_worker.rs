use anyhow::{anyhow, Error, Result};
use arc_swap::ArcSwap;
use fleascope_rs::flea_scope::CaptureConfigError;
use fleascope_rs::{FleaProbe, IdleFleaScope, ProbeType, ReadStatus, ScopeReading};

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

use crate::device::{
    CaptureConfig, CaptureMode, ControlCommand, DataPoint, DeviceData, Notification, TriggerConfig,
    TriggerSource, WaveformConfig, WorkerStatus,
};

/// How often a paused worker checks that its device is still attached.
const PRESENCE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Whether the serial port the scope was opened on is still attached. Polled while
/// paused, so it must be cheap: listing ports scans every tty through libudev and even
/// opens the `serial8250` ones (tens of ms of CPU per call).
#[cfg(unix)]
pub fn port_attached(port: &str) -> bool {
    // Unplugging removes the device node even while it is open (USB serial and ptys
    // alike), and the kernel does not hand the name to a new device until it is closed.
    std::path::Path::new(port).exists()
}

/// Whether the serial port the scope was opened on is still attached.
#[cfg(not(unix))]
pub fn port_attached(port: &str) -> bool {
    // COM port names are not paths, so fall back to listing the ports.
    match fleascope_rs::FleaConnector::get_available_devices(None) {
        Ok(mut devices) => devices.any(|device| device.port == port),
        Err(e) => {
            tracing::warn!("Cannot list serial ports, assuming {port} is attached: {e}");
            true
        }
    }
}

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
    pub digital_batch_tx: tokio::sync::mpsc::Sender<Vec<u16>>,
    /// Whether the device is still attached. Nothing touches the port while paused, and
    /// every fleascope-rs call on an unplugged port panics, so the pause polls this.
    pub device_present: Box<dyn Fn() -> bool + Send + Sync>,
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
                    Ok(()) => Notification::Success("Calibration saved successfully".to_string()),
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
        }
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

    async fn apply_waveform(&mut self, fleascope: &mut IdleFleaScope) {
        tracing::info!("Waveform configuration changed, updating waveform");
        let waveform_config = self.waveform_rx.borrow_and_update().clone();
        if let Err(e) =
            fleascope.set_waveform(waveform_config.waveform_type, waveform_config.frequency_hz)
        {
            tracing::error!("Failed to set waveform: {}", e);
            if let Err(e) = self
                .notify(Notification::Error(format!("Failed to set waveform: {e}")))
                .await
            {
                tracing::warn!("Could not report waveform failure: {e:#}");
            }
        }
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
                self.apply_waveform(&mut fleascope).await;
            }

            if !self.is_running() {
                tracing::debug!("Device is paused, skipping data generation");

                // During pause, still check for config changes and calibration commands
                let command = tokio::select! {
                    signal = self.waveform_rx.changed() => {
                        if signal.is_ok() {
                            tracing::info!("Waveform configuration changed while paused");
                            // `changed()` marked it as seen; flag it again so the
                            // `has_changed()` check above applies it to the device.
                            self.waveform_rx.mark_changed();
                        }
                        None
                    }
                    Some(command) = self.control_rx.recv() => Some(command),
                    () = tokio::time::sleep(PRESENCE_POLL_INTERVAL) => None,
                };
                // Nothing touched the port while paused, so this is the only way to notice an
                // unplug before the next fleascope-rs call (RUN, calibration, teardown on
                // Remove) panics on the dead port. A consumed `Exit` is fine: `stop()` also
                // drops the device handle, which ends `wait_for_removal`.
                if !(self.device_present)() {
                    tracing::error!("Device unplugged while paused");
                    self.set_lost_connection().await;
                    return None;
                }
                if let Some(command) = command {
                    tracing::info!("Received calibration command while paused: {:?}", command);
                    if let Err(e) = self.handle_control_command(command, &mut fleascope).await {
                        tracing::info!("FleaWorker stopping: {e:#}");
                        break;
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
                CaptureMode::DigitalStream {
                    channel_mask,
                    us_per_sample,
                } => {
                    self.handle_digital_stream_capture(channel_mask, us_per_sample, fleascope)
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
                    // Exact compare: any change in the computed rate must be published.
                    #[allow(
                        clippy::float_cmp,
                        reason = "change detection, not equality of measurements"
                    )]
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
                        .map(fleascope_rs::AnalogTrigger::into_trigger_fields)
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
                        "Invalid trigger configuration: {e}"
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
            Err((scope, CaptureConfigError::SerialTerminal(e))) => {
                return self.start_failed("capture", scope, e.into(), true).await;
            }
            Err((scope, e)) => {
                return self.start_failed("capture", scope, e.into(), false).await;
            }
        };
        tracing::debug!("Successfully started read operation on FleaScope");

        loop {
            match fleascope_for_read.try_get_result() {
                Ok(ReadStatus::Done(scope, reading)) => {
                    self.mark_connected();
                    let data_copy = self.data.clone();
                    tokio::spawn(async move {
                        profiling::scope!("data_processing_pipeline");

                        let processed = reading.map_err(Error::from).and_then(|reading| {
                            profiling::scope!("convert_to_data_points");
                            FleaWorker::reading_to_data_points(&reading, &probe_clone)
                        });

                        match processed {
                            Ok((x_values, data_points)) => {
                                profiling::scope!("update_shared_data");

                                let new_data = DeviceData {
                                    x_values,
                                    data_points,
                                    last_update: Instant::now(),
                                };
                                data_copy.store(Arc::new(new_data));
                            }
                            Err(e) => {
                                tracing::error!("Failed to process triggered capture: {:#}", e);
                            }
                        }
                    });
                    return Ok(scope);
                }
                Ok(ReadStatus::Pending(scope)) => {
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
                return self.stopped(fleascope_for_read.cancel()).await;
            }
        }
    }

    /// Hands back the scope after cancelling a capture or stream; failing to
    /// cancel means the device is gone.
    async fn stopped<E>(&mut self, stop_result: Result<IdleFleaScope, E>) -> Result<IdleFleaScope>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        match stop_result {
            Ok(scope) => Ok(scope),
            Err(e) => {
                tracing::error!("Failed to stop device operation: {}", e);
                self.set_lost_connection().await;
                Err(Error::new(e).context("Connection lost while stopping device operation"))
            }
        }
    }

    /// Starting a capture or stream failed. A terminal error means the device is gone;
    /// anything else is a configuration problem that would fail again on every retry,
    /// so report it and pause.
    async fn start_failed(
        &mut self,
        what: &str,
        scope: IdleFleaScope,
        error: Error,
        connection_lost: bool,
    ) -> Result<IdleFleaScope> {
        tracing::error!("Failed to start {what}: {error:#}");
        if connection_lost {
            self.set_lost_connection().await;
            return Err(error.context(format!("Connection lost while starting {what}")));
        }
        if let Err(e) = self
            .notify(Notification::Error(format!(
                "Failed to start {what}: {error:#}"
            )))
            .await
        {
            tracing::warn!("Could not report {what} failure: {e:#}");
        }
        self.set_as_paused();
        Ok(scope)
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

        let mut streaming_scope = match fleascope.stream() {
            Ok(streaming_scope) => streaming_scope,
            Err((scope, e)) => {
                return self
                    .start_failed("continuous stream", scope, e.into(), true)
                    .await;
            }
        };
        let mut start_time = Instant::now();
        // let mut total_samples = 0u32;
        loop {
            let read_result = {
                profiling::scope!("read_batch_data");

                streaming_scope
                    .read(BUFF_SIZE)
                    .map(|buffer| probe.voltages(&buffer).map(Iterator::collect::<Vec<f64>>))
            };

            match read_result {
                Ok(batch_result) => {
                    self.mark_connected();
                    match batch_result {
                        Ok(batch) => {
                            profiling::scope!("send_batch_channel");

                            match self.batch_tx.try_send(batch) {
                                Ok(()) => {}
                                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                    tracing::debug!("Batch channel full - dropping batch (UI not draining fast enough)");
                                }
                                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                    tracing::warn!(
                                        "Failed to send batch - receiver has been dropped"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to calibrate streamed batch: {}", e);
                        }
                    }
                    if self.check_settings_changed() {
                        return self.stopped(streaming_scope.stop()).await;
                    }
                }
                Err(e) => {
                    tracing::error!("Error reading from continuous stream: {}", e);
                    self.set_lost_connection().await;
                    // A timeout leaves the port usable, so hand the stopped scope back.
                    if e.kind() == std::io::ErrorKind::TimedOut {
                        if let Ok(scope) = streaming_scope.stop() {
                            return Ok(scope);
                        }
                    }
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

    /// Returns `Err` only when the connection is lost; the scope is dropped then.
    async fn handle_digital_stream_capture(
        &mut self,
        channel_mask: u16,
        us_per_sample: u32,
        fleascope: IdleFleaScope,
    ) -> Result<IdleFleaScope> {
        let n_samples = IdleFleaScope::compute_n_samples_per_packet(channel_mask, us_per_sample);
        let mut streaming_scope =
            match fleascope.digital_stream(channel_mask, us_per_sample, n_samples) {
                Ok(streaming_scope) => streaming_scope,
                Err((scope, CaptureConfigError::SerialTerminal(e))) => {
                    return self
                        .start_failed("digital stream", scope, e.into(), true)
                        .await;
                }
                // E.g. firmware too old.
                Err((scope, e)) => {
                    return self
                        .start_failed("digital stream", scope, e.into(), false)
                        .await;
                }
            };
        loop {
            let batch_result = {
                profiling::scope!("read_digital_packet");
                streaming_scope.read_packet()
            };

            // Yield so the egui render task can run between reads.
            tokio::task::yield_now().await;

            match batch_result {
                Ok(bitmasks) => {
                    self.mark_connected();
                    {
                        profiling::scope!("send_digital_batch_channel");
                        match self.digital_batch_tx.try_send(bitmasks) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                tracing::debug!("Digital batch channel full - dropping batch (UI not draining fast enough)");
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                tracing::warn!("Failed to send digital batch - receiver dropped");
                            }
                        }
                    }
                    if self.check_settings_changed() {
                        return self.stopped(streaming_scope.stop()).await;
                    }
                }
                Err(e) => {
                    tracing::error!("Error reading from digital stream: {}", e);
                    self.set_lost_connection().await;
                    // A timeout leaves the port usable, so hand the stopped scope back.
                    if e.kind() == std::io::ErrorKind::TimedOut {
                        if let Ok(scope) = streaming_scope.stop() {
                            return Ok(scope);
                        }
                    }
                    return Err(Error::new(e).context("Connection lost during digital stream"));
                }
            }
        }
    }

    fn reading_to_data_points(
        reading: &ScopeReading,
        probe: &FleaProbe,
    ) -> Result<(Vec<f64>, Vec<DataPoint>)> {
        let data_points = probe
            .voltages(reading.raw_bnc())?
            .zip(reading.bitmap())
            .map(|(analog_channel, &bitmap)| DataPoint {
                analog_channel,
                digital_channels: std::array::from_fn(|i| bitmap & (1 << i) != 0),
            })
            .collect();

        Ok((reading.times().collect(), data_points))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    #[cfg(unix)]
    use fleascope_rs::{IdleFleaTerminal, StatelessFleaTerminal};
    #[cfg(unix)]
    use std::sync::atomic::{AtomicBool, Ordering};

    fn test_worker() -> (FleaWorker, watch::Receiver<WorkerStatus>) {
        let (_config_tx, config_change_rx) = watch::channel(CaptureConfig {
            probe_multiplier: ProbeType::X1,
            mode: CaptureMode::Continuous {},
        });
        let (_control_tx, control_rx) = tokio::sync::mpsc::channel(1);
        let (notification_tx, _notification_rx) = tokio::sync::mpsc::channel(1);
        let (_waveform_tx, waveform_rx) = watch::channel(WaveformConfig::default());
        let (batch_tx, _batch_rx) = tokio::sync::mpsc::channel(1);
        let (digital_batch_tx, _digital_batch_rx) = tokio::sync::mpsc::channel(1);
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
            digital_batch_tx,
            device_present: Box::new(|| true),
        };
        (worker, status_rx)
    }

    /// The UI derives the RUN/STOP button from the worker status channel
    /// (`control_panel.rs`). Resume after Pause must publish `running = true`,
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
    /// send to the worker (`worker_interface.rs`). The worker must keep its receivers
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

    /// The UI side of a worker, as `FleaScopeDevice` holds it. Dropping it after
    /// sending `Exit` is what `DeviceManager::remove_device` does.
    #[cfg(unix)]
    struct UiHandles {
        config_tx: watch::Sender<CaptureConfig>,
        waveform_tx: watch::Sender<WaveformConfig>,
        control_tx: tokio::sync::mpsc::Sender<ControlCommand>,
        notification_rx: tokio::sync::mpsc::Receiver<Notification>,
    }

    #[cfg(unix)]
    fn attach_ui(worker: &mut FleaWorker) -> UiHandles {
        let (config_tx, config_change_rx) = watch::channel(CaptureConfig {
            probe_multiplier: ProbeType::X1,
            mode: CaptureMode::Continuous {},
        });
        let (waveform_tx, waveform_rx) = watch::channel(WaveformConfig::default());
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(32);
        let (notification_tx, notification_rx) = tokio::sync::mpsc::channel(32);
        worker.config_change_rx = config_change_rx;
        worker.waveform_rx = waveform_rx;
        worker.control_rx = control_rx;
        worker.notification_tx = notification_tx;
        UiHandles {
            config_tx,
            waveform_tx,
            control_tx,
            notification_rx,
        }
    }

    /// How a [`FakeFleaScope`] answers.
    #[cfg(unix)]
    struct FakeFirmware {
        /// Reply to `ver`.
        version: &'static str,
        /// Packets sent in turn, one per poll, while a `digital_stream` runs; the
        /// stream ends on CTRL-C with the prompt, like the firmware. `None` answers
        /// `digital_stream` with the prompt like any other command.
        digital_packets: Option<Vec<Vec<u8>>>,
    }

    #[cfg(unix)]
    impl Default for FakeFirmware {
        fn default() -> Self {
            Self {
                version: "3.1.0",
                digital_packets: None,
            }
        }
    }

    /// A fake device on a pseudo terminal that answers every command with the prompt.
    #[cfg(unix)]
    struct FakeFleaScope {
        port: String,
        plugged: Arc<AtomicBool>,
        /// Every byte the worker wrote to the device.
        received: Arc<std::sync::Mutex<Vec<u8>>>,
        device: std::thread::JoinHandle<std::io::Result<()>>,
    }

    #[cfg(unix)]
    impl FakeFleaScope {
        fn plug_in() -> (Self, IdleFleaScope) {
            Self::plug_in_with(FakeFirmware::default())
        }

        fn plug_in_with(firmware: FakeFirmware) -> (Self, IdleFleaScope) {
            use serialport::SerialPort as _;
            use std::io::{ErrorKind, Read as _, Write as _};

            let (mut master, slave) = serialport::TTYPort::pair().unwrap();
            if firmware.digital_packets.is_some() {
                // Poll often enough to stream packets at a steady pace.
                master.set_timeout(Duration::from_millis(5)).unwrap();
            }
            let plugged = Arc::new(AtomicBool::new(true));
            let received = Arc::new(std::sync::Mutex::new(Vec::new()));
            let device = std::thread::spawn({
                let plugged = plugged.clone();
                let received = received.clone();
                move || {
                    let mut buf = [0u8; 256];
                    let mut line = Vec::new();
                    let mut streaming = false;
                    let mut next_packet = 0;
                    while plugged.load(Ordering::SeqCst) {
                        match master.read(&mut buf) {
                            Ok(n) => {
                                received.lock().unwrap().extend_from_slice(&buf[..n]);
                                for &byte in &buf[..n] {
                                    match byte {
                                        // CTRL-C ends a running command, which prints the prompt.
                                        0x03 => {
                                            if streaming {
                                                streaming = false;
                                                master.write_all(b"> ")?;
                                            }
                                            line.clear();
                                        }
                                        b'\n' => {
                                            let command = line.trim_ascii();
                                            // The driver refuses firmware outside 3.x on connect.
                                            if command == b"ver" {
                                                master.write_all(firmware.version.as_bytes())?;
                                                master.write_all(b"\n")?;
                                            }
                                            if firmware.digital_packets.is_some()
                                                && command.starts_with(b"digital_stream ")
                                            {
                                                streaming = true;
                                                next_packet = 0;
                                            } else {
                                                master.write_all(b"> ")?;
                                            }
                                            line.clear();
                                        }
                                        _ => line.push(byte),
                                    }
                                }
                            }
                            Err(e) if e.kind() == ErrorKind::TimedOut => {}
                            Err(e) => return Err(e),
                        }
                        let packets = firmware.digital_packets.as_deref().unwrap_or_default();
                        if streaming && !packets.is_empty() {
                            match master.write_all(&packets[next_packet % packets.len()]) {
                                // The worker is not reading right now; skip this packet.
                                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                                result => result?,
                            }
                            next_packet += 1;
                        }
                    }
                    Ok(())
                }
            });
            let port = slave.name().unwrap();
            let terminal = StatelessFleaTerminal::new(&port).unwrap();
            let scope = IdleFleaScope::new(IdleFleaTerminal::try_from(terminal).unwrap()).unwrap();
            drop(slave);
            (
                Self {
                    port,
                    plugged,
                    received,
                    device,
                },
                scope,
            )
        }

        /// Everything the worker wrote to the device so far.
        fn written(&self) -> String {
            String::from_utf8_lossy(&self.received.lock().unwrap()).into_owned()
        }

        /// Waits up to 2s for the worker's writes to satisfy `done`.
        async fn wait_until_written(&self, done: impl Fn(&str) -> bool) -> bool {
            tokio::time::timeout(Duration::from_secs(2), async {
                while !done(&self.written()) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .is_ok()
        }

        /// What `DeviceManager` gives the worker as `device_present`.
        fn presence(&self) -> Box<dyn Fn() -> bool + Send + Sync> {
            let port = self.port.clone();
            Box::new(move || port_attached(&port))
        }

        /// Closes the master side. Like the kernel does to an open tty whose USB
        /// device is unplugged, this hangs up the port: every further write fails.
        fn unplug(self) {
            self.plugged.store(false, Ordering::SeqCst);
            self.device.join().unwrap().unwrap();
        }

        /// Stops the fake after the worker closed its end of the port. The hangup
        /// that closing causes on the fake's side is expected then.
        fn remove_after_worker_closed_port(self) {
            self.plugged.store(false, Ordering::SeqCst);
            // Ignore only the hangup error; a panic in the fake still fails the test.
            let _hangup = self.device.join().unwrap();
        }
    }

    /// Nothing touches the port while paused, yet every fleascope-rs call on an
    /// unplugged port panics (and release builds abort). The worker must notice.
    #[cfg(unix)]
    #[tokio::test]
    async fn paused_worker_notices_unplug_on_its_own() {
        let (mut worker, mut status_rx) = test_worker();
        let ui = attach_ui(&mut worker);
        let (device, scope) = FakeFleaScope::plug_in();
        worker.device_present = device.presence();
        worker.set_as_paused();
        let task = tokio::spawn(async move { worker.run(scope).await });

        tokio::time::sleep(PRESENCE_POLL_INTERVAL * 3).await;
        assert!(
            status_rx.borrow().connected,
            "present device lost while paused"
        );
        assert!(!task.is_finished());

        device.unplug();
        tokio::time::timeout(
            Duration::from_secs(2),
            status_rx.wait_for(|status| !status.connected),
        )
        .await
        .expect("paused worker must notice the unplug")
        .unwrap();

        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
    }

    /// Removing the device right after the unplug must not reach
    /// `IdleFleaScope::teardown`, which writes to the dead port.
    #[cfg(unix)]
    #[tokio::test]
    async fn removing_device_unplugged_while_paused_does_not_panic() {
        let (mut worker, _status_rx) = test_worker();
        let ui = attach_ui(&mut worker);
        let (device, scope) = FakeFleaScope::plug_in();
        worker.device_present = device.presence();
        worker.set_as_paused();
        let task = tokio::spawn(async move { worker.run(scope).await });
        tokio::task::yield_now().await; // let the worker settle into its pause

        device.unplug();
        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
    }

    /// A command that wakes the pause after an unplug must be stopped by the presence
    /// check: RUN would start a stream and calibration would talk to the dead port, both
    /// of which panic in fleascope-rs on real hardware. The fake stays attached while the
    /// presence check reports it gone, so any byte the worker writes after the "unplug"
    /// proves the command reached the port. The command is dropped with the lost
    /// connection, and later ones are answered by `wait_for_removal`.
    #[cfg(unix)]
    async fn command_after_unplug_while_paused_never_reaches_port(
        command: impl Fn() -> ControlCommand,
    ) {
        fn describe(notification: Option<Notification>) -> String {
            match notification {
                Some(Notification::Success(msg)) => format!("Success({msg})"),
                Some(Notification::Error(msg)) => format!("Error({msg})"),
                None => "channel closed".to_string(),
            }
        }

        let name = format!("{:?}", command());
        let (mut worker, mut status_rx) = test_worker();
        let mut ui = attach_ui(&mut worker);
        let (device, scope) = FakeFleaScope::plug_in();
        let present = Arc::new(AtomicBool::new(true));
        worker.device_present = Box::new({
            let present = present.clone();
            move || present.load(Ordering::SeqCst)
        });
        // Without a calibration, Calibrate3V and StoreCalibration fail before any I/O.
        worker.x1.set_calibration(2048.0, 1.0);
        worker.x10.set_calibration(2048.0, 1.0);
        worker.set_as_paused();
        let task = tokio::spawn(async move { worker.run(scope).await });
        tokio::time::sleep(Duration::from_millis(50)).await; // settle into the pause
        let written_before = device.received.lock().unwrap().len();

        present.store(false, Ordering::SeqCst);
        ui.control_tx.try_send(command()).unwrap();
        let lost = tokio::time::timeout(
            Duration::from_secs(2),
            status_rx.wait_for(|status| !status.connected),
        )
        .await
        .is_ok_and(|status| status.is_ok());
        // Give the fake time to read anything the worker wrote.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let written = String::from_utf8_lossy(&device.received.lock().unwrap()[written_before..])
            .into_owned();
        assert!(
            written.is_empty(),
            "{name} after unplug reached the port: {written:?}"
        );
        assert!(lost, "{name} after unplug must report the lost connection");

        let first = tokio::time::timeout(Duration::from_secs(1), ui.notification_rx.recv())
            .await
            .unwrap();
        let first = describe(first);
        assert!(
            first.starts_with("Error(Lost connection"),
            "{name}: expected the lost-connection error first, got {first}"
        );

        ui.control_tx.try_send(command()).unwrap();
        let second = tokio::time::timeout(Duration::from_secs(1), ui.notification_rx.recv())
            .await
            .unwrap();
        let second = describe(second);
        assert!(
            second.starts_with("Error(Device disconnected"),
            "{name}: a later command must be rejected, got {second}"
        );

        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        drop(ui);
        let joined = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed");
        assert!(
            joined.is_ok(),
            "{name} after unplug panicked the worker: {joined:?}"
        );
        device.remove_after_worker_closed_port();
    }

    // Multi-threaded so the timeouts still fire if a command starts a busy capture.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_after_unplug_while_paused_does_not_start_capture() {
        command_after_unplug_while_paused_never_reaches_port(|| ControlCommand::Resume).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_calibration_after_unplug_while_paused_does_not_reach_port() {
        command_after_unplug_while_paused_never_reaches_port(|| {
            ControlCommand::Calibrate0V(ProbeType::X1)
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_volt_calibration_after_unplug_while_paused_does_not_reach_port() {
        command_after_unplug_while_paused_never_reaches_port(|| {
            ControlCommand::Calibrate3V(ProbeType::X10)
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn storing_calibration_after_unplug_while_paused_does_not_reach_port() {
        command_after_unplug_while_paused_never_reaches_port(ControlCommand::StoreCalibration)
            .await;
    }

    /// The presence check must not swallow commands while the device is still there.
    #[cfg(unix)]
    #[tokio::test]
    async fn calibration_while_paused_reaches_present_device() {
        let (mut worker, status_rx) = test_worker();
        let mut ui = attach_ui(&mut worker);
        let (device, scope) = FakeFleaScope::plug_in();
        worker.device_present = device.presence();
        worker.set_as_paused();
        let task = tokio::spawn(async move { worker.run(scope).await });
        tokio::task::yield_now().await; // let the worker settle into its pause

        ui.control_tx
            .try_send(ControlCommand::Calibrate0V(ProbeType::X1))
            .unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(2), ui.notification_rx.recv())
            .await
            .expect("calibration while paused must be answered")
            .unwrap();
        let msg = match answer {
            Notification::Success(msg) | Notification::Error(msg) => msg,
        };
        assert!(msg.starts_with("X1 "), "not a calibration answer: {msg}");
        assert!(status_rx.borrow().connected);

        device.unplug();
        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
    }

    /// Starting a stream on an unplugged port fails at once. The worker must report
    /// the lost connection instead of retrying in a hot loop. Multi-threaded so the
    /// timeout still fires if the worker spins.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn running_worker_reports_unplug_when_stream_cannot_start() {
        let (mut worker, mut status_rx) = test_worker();
        let ui = attach_ui(&mut worker);
        let (device, scope) = FakeFleaScope::plug_in();
        device.unplug();
        let task = tokio::spawn(async move { worker.run(scope).await });

        tokio::time::timeout(
            Duration::from_secs(2),
            status_rx.wait_for(|status| !status.connected),
        )
        .await
        .expect("worker must report the lost connection")
        .unwrap();

        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
    }

    /// The waveform generator controls work while paused (`control_panel.rs`). A change
    /// made then must still reach the device, not be swallowed by the pause wait.
    #[cfg(unix)]
    #[tokio::test]
    async fn waveform_change_while_paused_reaches_device() {
        let (mut worker, _status_rx) = test_worker();
        let ui = attach_ui(&mut worker);
        let (device, scope) = FakeFleaScope::plug_in();
        worker.device_present = device.presence();
        worker.set_as_paused();
        let task = tokio::spawn(async move { worker.run(scope).await });
        tokio::task::yield_now().await; // let the worker settle into its pause

        ui.waveform_tx
            .send(WaveformConfig {
                enabled: true,
                waveform_type: fleascope_rs::Waveform::Square,
                frequency_hz: 440,
            })
            .unwrap();

        let reached = tokio::time::timeout(Duration::from_secs(2), async {
            while !String::from_utf8_lossy(&device.received.lock().unwrap())
                .contains("wave square 440")
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;

        device.unplug();
        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
        assert!(
            reached.is_ok(),
            "waveform change made while paused never reached the device"
        );
    }

    /// Channels 0 and 2 at 500 Hz: 25 samples per packet, packed into 7 bytes.
    #[cfg(unix)]
    const DIGITAL_MASK: u16 = 0b101;
    #[cfg(unix)]
    const DIGITAL_US_PER_SAMPLE: u32 = 2000;
    #[cfg(unix)]
    const DIGITAL_START: &str = "digital_stream 0x5 2000 25\n";

    #[cfg(unix)]
    fn digital_stream_config(channel_mask: u16, us_per_sample: u32) -> CaptureConfig {
        CaptureConfig {
            probe_multiplier: ProbeType::X1,
            mode: CaptureMode::DigitalStream {
                channel_mask,
                us_per_sample,
            },
        }
    }

    /// A fake streaming, in turn: no channel high, channel 0 high, channel 2 high.
    #[cfg(unix)]
    fn streaming_firmware() -> FakeFirmware {
        FakeFirmware {
            digital_packets: Some(vec![vec![0x00; 7], vec![0x55; 7], vec![0xAA; 7]]),
            ..FakeFirmware::default()
        }
    }

    /// The bitmasks the worker must forward for the packets of `streaming_firmware`.
    #[cfg(unix)]
    fn streamed_batches() -> [Vec<u16>; 3] {
        [vec![0b000; 25], vec![0b001; 25], vec![0b100; 25]]
    }

    #[cfg(unix)]
    async fn next_notification(ui: &mut UiHandles) -> String {
        let notification = tokio::time::timeout(Duration::from_secs(2), ui.notification_rx.recv())
            .await
            .expect("expected a notification");
        match notification {
            Some(Notification::Success(msg)) => format!("Success({msg})"),
            Some(Notification::Error(msg)) => format!("Error({msg})"),
            None => "channel closed".to_string(),
        }
    }

    /// Proves the worker still holds a working scope: a paused worker that kept it
    /// answers a calibration from the device, `wait_for_removal` would reject it.
    #[cfg(unix)]
    async fn assert_scope_kept(ui: &mut UiHandles) {
        ui.control_tx
            .try_send(ControlCommand::Calibrate0V(ProbeType::X1))
            .unwrap();
        let answer = next_notification(ui).await;
        assert!(
            answer.starts_with("Success(X1 ") || answer.starts_with("Error(X1 "),
            "not a calibration answer: {answer}"
        );
    }

    /// Same, for a worker that dropped the scope and waits for removal.
    #[cfg(unix)]
    async fn assert_scope_dropped(ui: &mut UiHandles) {
        ui.control_tx.try_send(ControlCommand::Resume).unwrap();
        let answer = next_notification(ui).await;
        assert!(
            answer.starts_with("Error(Device disconnected"),
            "a disconnected worker must reject commands, got {answer}"
        );
    }

    #[cfg(unix)]
    async fn wait_for_status(
        status_rx: &mut watch::Receiver<WorkerStatus>,
        what: &str,
        done: impl FnMut(&WorkerStatus) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(2), status_rx.wait_for(done))
            .await
            .expect(what)
            .unwrap();
    }

    /// Firmware before 3.1.0 has no `digital_stream`. That is a configuration problem,
    /// not a lost device: report it, pause, keep the scope, and do not retry.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digital_stream_on_old_firmware_reports_error_and_pauses() {
        let (mut worker, mut status_rx) = test_worker();
        let mut ui = attach_ui(&mut worker);
        ui.config_tx
            .send(digital_stream_config(DIGITAL_MASK, DIGITAL_US_PER_SAMPLE))
            .unwrap();
        let (device, scope) = FakeFleaScope::plug_in_with(FakeFirmware {
            version: "3.0.0",
            ..FakeFirmware::default()
        });
        worker.device_present = device.presence();
        let task = tokio::spawn(async move { worker.run(scope).await });

        let first = next_notification(&mut ui).await;
        assert!(
            first.starts_with("Error(Failed to start digital stream"),
            "expected the unsupported-firmware error, got {first}"
        );
        wait_for_status(&mut status_rx, "worker must pause", |status| {
            !status.running
        })
        .await;
        assert!(
            status_rx.borrow().connected,
            "old firmware is not a lost connection"
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            ui.notification_rx.try_recv().is_err(),
            "the worker must not retry an unsupported stream"
        );
        assert!(!device.written().contains("digital_stream"));
        assert_scope_kept(&mut ui).await;

        device.unplug();
        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
    }

    /// Starting the stream on an unplugged port is a lost connection, not a
    /// configuration error to show and pause on.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digital_stream_start_on_unplugged_port_reports_lost_connection() {
        let (mut worker, mut status_rx) = test_worker();
        let mut ui = attach_ui(&mut worker);
        ui.config_tx
            .send(digital_stream_config(DIGITAL_MASK, DIGITAL_US_PER_SAMPLE))
            .unwrap();
        let (device, scope) = FakeFleaScope::plug_in_with(streaming_firmware());
        device.unplug();
        let task = tokio::spawn(async move { worker.run(scope).await });

        wait_for_status(
            &mut status_rx,
            "worker must report the lost connection",
            |status| !status.connected,
        )
        .await;
        let first = next_notification(&mut ui).await;
        assert!(
            first.starts_with("Error(Lost connection"),
            "expected the lost-connection error, got {first}"
        );
        assert_scope_dropped(&mut ui).await;

        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
    }

    /// Decoded packets reach the UI in order, and a config or waveform change stops
    /// the stream (CTRL-C) and restarts it with the new settings.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digital_stream_forwards_packets_and_restarts_on_change() {
        let (mut worker, status_rx) = test_worker();
        let ui = attach_ui(&mut worker);
        let (digital_tx, mut digital_rx) = tokio::sync::mpsc::channel(64);
        worker.digital_batch_tx = digital_tx;
        worker
            .status_tx
            .send_modify(|status| status.connected = false);
        ui.config_tx
            .send(digital_stream_config(DIGITAL_MASK, DIGITAL_US_PER_SAMPLE))
            .unwrap();
        let (device, scope) = FakeFleaScope::plug_in_with(streaming_firmware());
        worker.device_present = device.presence();
        let task = tokio::spawn(async move { worker.run(scope).await });

        let mut batches = Vec::new();
        for _ in 0..6 {
            let batch = tokio::time::timeout(Duration::from_secs(2), digital_rx.recv())
                .await
                .expect("expected a streamed batch")
                .unwrap();
            batches.push(batch);
        }
        let [none, ch0, ch2] = streamed_batches();
        assert_eq!(
            batches,
            [none.clone(), ch0.clone(), ch2.clone(), none, ch0, ch2]
        );
        assert!(
            status_rx.borrow().connected,
            "a streamed packet proves the device is connected"
        );
        assert!(device.written().contains(DIGITAL_START));

        ui.config_tx
            .send(digital_stream_config(0b1, DIGITAL_US_PER_SAMPLE))
            .unwrap();
        let restart = format!("{DIGITAL_START}\u{3}digital_stream 0x1 2000 25\n");
        assert!(
            device.wait_until_written(|w| w.ends_with(&restart)).await,
            "a config change must stop the stream and restart it: {:?}",
            device.written()
        );

        ui.waveform_tx
            .send(WaveformConfig {
                enabled: true,
                waveform_type: fleascope_rs::Waveform::Square,
                frequency_hz: 440,
            })
            .unwrap();
        let restarted_after_waveform = |w: &str| {
            w.split_once(&restart).is_some_and(|(_, after)| {
                after
                    .strip_prefix('\u{3}')
                    .and_then(|after| after.split_once("wave square 440"))
                    .is_some_and(|(_, after)| after.ends_with("digital_stream 0x1 2000 25\n"))
            })
        };
        assert!(
            device.wait_until_written(restarted_after_waveform).await,
            "a waveform change must stop the stream, apply it and restart: {:?}",
            device.written()
        );

        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("worker must stop on Exit")
            .expect("worker must not panic");
        drop(ui);
        device.remove_after_worker_closed_port();
    }

    /// A UI that does not drain its batches must not stall or end the read loop: the
    /// worker keeps reading packets, a queued Pause still stops the stream, and the
    /// paused worker does not restart it.
    #[cfg(unix)]
    async fn digital_stream_survives_undrained_ui(keep_receiver: bool) {
        let (mut worker, mut status_rx) = test_worker();
        let ui = attach_ui(&mut worker);
        let (digital_tx, digital_rx) = tokio::sync::mpsc::channel(1);
        worker.digital_batch_tx = digital_tx;
        let mut digital_rx = keep_receiver.then_some(digital_rx);
        worker
            .status_tx
            .send_modify(|status| status.connected = false);
        ui.config_tx
            .send(digital_stream_config(DIGITAL_MASK, DIGITAL_US_PER_SAMPLE))
            .unwrap();
        let (device, scope) = FakeFleaScope::plug_in_with(streaming_firmware());
        worker.device_present = device.presence();
        // Lets the test clear `connected`, which only a newly read packet sets again.
        let status_tx = worker.status_tx.clone();
        let task = tokio::spawn(async move { worker.run(scope).await });

        wait_for_status(&mut status_rx, "the stream must start", |status| {
            status.connected
        })
        .await;
        // Many more packets than the channel holds.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let status = *status_rx.borrow();
        assert!(
            status.running && status.connected,
            "a full or closed batch channel must not stop the worker: {status:?}"
        );
        assert!(
            device.written().ends_with(DIGITAL_START),
            "a full or closed batch channel must not stop the stream: {:?}",
            device.written()
        );
        for _ in 0..3 {
            status_tx.send_modify(|status| status.connected = false);
            wait_for_status(
                &mut status_rx,
                "the worker must keep reading packets",
                |status| status.connected,
            )
            .await;
        }
        if let Some(digital_rx) = &mut digital_rx {
            let [none, ..] = streamed_batches();
            assert_eq!(digital_rx.try_recv().unwrap(), none, "first batch is kept");
            tokio::time::timeout(Duration::from_secs(2), digital_rx.recv())
                .await
                .expect("a drained channel must receive batches again")
                .unwrap();
        }
        assert!(status_rx.borrow().running);
        assert!(device.written().ends_with(DIGITAL_START));

        ui.control_tx.try_send(ControlCommand::Pause).unwrap();
        wait_for_status(
            &mut status_rx,
            "Pause must reach the streaming worker",
            |status| !status.running,
        )
        .await;
        assert!(
            device.written().ends_with(&format!("{DIGITAL_START}\u{3}")),
            "Pause must stop the stream: {:?}",
            device.written()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            device.written().matches("digital_stream").count(),
            1,
            "a paused worker must not restart the stream"
        );

        device.unplug();
        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digital_stream_drops_batches_when_ui_lags() {
        digital_stream_survives_undrained_ui(true).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digital_stream_keeps_running_without_batch_receiver() {
        digital_stream_survives_undrained_ui(false).await;
    }

    /// A read timeout leaves the port usable: report the lost connection, but stop
    /// the stream and keep the scope.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digital_stream_read_timeout_keeps_scope() {
        let (mut worker, mut status_rx) = test_worker();
        let mut ui = attach_ui(&mut worker);
        ui.config_tx
            .send(digital_stream_config(DIGITAL_MASK, DIGITAL_US_PER_SAMPLE))
            .unwrap();
        let (device, scope) = FakeFleaScope::plug_in_with(FakeFirmware {
            digital_packets: Some(Vec::new()),
            ..FakeFirmware::default()
        });
        worker.device_present = device.presence();
        let task = tokio::spawn(async move { worker.run(scope).await });

        wait_for_status(&mut status_rx, "a silent stream must time out", |status| {
            !status.connected
        })
        .await;
        let first = next_notification(&mut ui).await;
        assert!(
            first.starts_with("Error(Lost connection"),
            "expected the lost-connection error, got {first}"
        );
        assert!(
            device
                .wait_until_written(|w| w.ends_with(&format!("{DIGITAL_START}\u{3}")))
                .await,
            "the timed-out stream must be stopped: {:?}",
            device.written()
        );
        assert!(!status_rx.borrow().running);
        assert_scope_kept(&mut ui).await;

        device.unplug();
        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
    }

    /// Unplugging mid-stream is a read error other than a timeout: the scope is
    /// dropped and the worker waits for the device to be removed.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digital_stream_unplug_drops_scope() {
        let (mut worker, mut status_rx) = test_worker();
        let mut ui = attach_ui(&mut worker);
        let (digital_tx, mut digital_rx) = tokio::sync::mpsc::channel(64);
        worker.digital_batch_tx = digital_tx;
        ui.config_tx
            .send(digital_stream_config(DIGITAL_MASK, DIGITAL_US_PER_SAMPLE))
            .unwrap();
        let (device, scope) = FakeFleaScope::plug_in_with(streaming_firmware());
        let task = tokio::spawn(async move { worker.run(scope).await });

        tokio::time::timeout(Duration::from_secs(2), digital_rx.recv())
            .await
            .expect("the stream must start")
            .unwrap();
        device.unplug();
        wait_for_status(&mut status_rx, "worker must report the unplug", |status| {
            !status.connected
        })
        .await;
        let first = next_notification(&mut ui).await;
        assert!(
            first.starts_with("Error(Lost connection"),
            "expected the lost-connection error, got {first}"
        );
        assert_scope_dropped(&mut ui).await;

        drop(ui);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker must stop once the device is removed")
            .expect("worker must not panic");
    }
}
