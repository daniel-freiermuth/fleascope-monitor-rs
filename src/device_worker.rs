use anyhow::{anyhow, bail, Context as _, Error, Result};
use arc_swap::ArcSwap;
use fleascope_rs::flea_scope::{
    BITMAP_COLUMN_NAME, CALIBRATED_COLUMN_NAME, RAW_COLUMN_NAME, TIME_COLUMN_NAME,
};
use fleascope_rs::trigger_config::TriggerConfig as _;
use fleascope_rs::{FleaProbe, IdleFleaScope, ProbeType};
use polars::frame::DataFrame;
use polars::prelude::{IntoLazy, PolarsResult, UInt16Chunked};
use polars::series::IntoSeries;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

use crate::device::{
    CaptureConfig, CaptureMode, ControlCommand, DataPoint, DeviceData, Notification, TriggerConfig,
    TriggerSource, WaveformConfig, WorkerStatus,
};

/// How often a paused worker checks that its device is still attached.
const PRESENCE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long fleascope-rs `cancel()`/`stop()` may wait for the prompt after CTRL-C.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(1);

/// How long one `ReadingFleaScope::try_get_result` may take: a single serial read,
/// which fleascope-rs bounds with a 70 ms port timeout.
const POLL_TIMEOUT: Duration = Duration::from_secs(1);

const IO_THREAD_NAME: &str = "fleascope-io";

/// Runs a fleascope-rs call that may panic or never return on its own thread and gives up
/// after `deadline`, abandoning that thread; `None` means the call did not deliver.
/// fleascope-rs panics on serial errors (`BusyFleaTerminal::cancel`, `read_chunk`), and
/// `cancel` waits for the prompt with no deadline. An abandoned thread either finishes late
/// and drops its result, or panics: with `panic = "unwind"` that ends only the thread, and
/// under `panic = "abort"` (release builds) `contain_io_panics` keeps it from aborting.
async fn run_contained<T: Send + 'static>(
    deadline: Duration,
    call: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    contain_io_panics();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name(IO_THREAD_NAME.to_string())
        .spawn(move || {
            // The receiver is gone once we gave up; the late result is dropped then.
            let _ = result_tx.send(call());
        });
    if let Err(e) = spawned {
        tracing::error!("Cannot spawn a thread for device I/O: {e}");
        return None;
    }
    if let Ok(Ok(result)) = tokio::time::timeout(deadline, result_rx).await {
        return Some(result);
    }
    tracing::error!("Device I/O did not complete within {deadline:?}");
    None
}

/// `cancel()`/`stop()` under `run_contained`; `None` means the device did not answer CTRL-C.
async fn cancel_with_deadline(
    cancel: impl FnOnce() -> IdleFleaScope + Send + 'static,
) -> Option<IdleFleaScope> {
    run_contained(CANCEL_TIMEOUT, cancel).await
}

/// Unwinding ends only the panicking thread, so there is nothing to contain.
#[cfg(not(panic = "abort"))]
fn contain_io_panics() {}

/// A panic hook runs before the abort, so parking the panicking thread in it keeps a panic
/// in a `run_contained` thread from aborting the process. std calls the hook while holding
/// its hook lock for reading, so a parked thread keeps that lock: after the first contained
/// panic, `std::panic::set_hook`/`take_hook` block forever. Nothing in the app calls them
/// after start-up; the hook must stay the last one installed.
#[cfg(panic = "abort")]
fn contain_io_panics() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().name() == Some(IO_THREAD_NAME) {
                tracing::error!("fleascope-rs panicked during device I/O, parking: {info}");
                loop {
                    std::thread::park();
                }
            }
            previous(info);
        }));
    });
}

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
                let command = tokio::select! {
                    signal = self.waveform_rx.changed() => {
                        if signal.is_ok() {
                            tracing::info!("Configuration changed while paused, will restart");
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
            // `read_chunk` panics on serial errors other than TimedOut, BrokenPipe and
            // UnexpectedEof (e.g. EIO), so poll on a contained thread.
            let poll = move || fleascope_for_read.try_get_result();
            match run_contained(POLL_TIMEOUT, poll).await {
                Some(Ok(Ok((scope, reading)))) => {
                    self.mark_connected();
                    let data_copy = self.data.clone();
                    tokio::spawn(async move {
                        profiling::scope!("data_processing_pipeline");

                        let processed = {
                            profiling::scope!("parse_csv");
                            reading
                                .parse_csv()
                                .and_then(|df| {
                                    profiling::scope!("apply_calibration");
                                    probe_clone.apply_calibration(df).collect()
                                })
                                .map_err(Error::from)
                                .and_then(|df| {
                                    profiling::scope!("convert_to_data_points");
                                    FleaWorker::convert_polars_to_data_points(&df)
                                })
                        };

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
                Some(Ok(Err(scope))) => {
                    fleascope_for_read = scope;
                }
                Some(Err(_)) | None => {
                    tracing::error!("Error during hardware read: Connection lost");
                    self.set_lost_connection().await;
                    return Err(Error::msg("Connection lost during hardware read"));
                }
            }
            let settings_changed = {
                // A tracy span is !Send; keep it off the awaits below so `run` stays Send.
                profiling::scope!("hardware_wait_polling_loop");
                self.check_settings_changed()
            };

            if settings_changed {
                tracing::info!("Settings changed during hardware wait, calling unblock()");
                // A silent device is indistinguishable from one waiting for its trigger.
                if let Some(scope) = cancel_with_deadline(move || fleascope_for_read.cancel()).await
                {
                    return Ok(scope);
                }
                self.set_lost_connection().await;
                return Err(Error::msg(
                    "Device did not answer CTRL-C during hardware read",
                ));
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
            let read_result = {
                profiling::scope!("read_batch_data");

                streaming_scope
                    .read(BUFF_SIZE)
                    .map(|buffer| Self::calibrate_batch(probe, buffer))
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
                        if let Some(scope) =
                            cancel_with_deadline(move || streaming_scope.stop()).await
                        {
                            return Ok(scope);
                        }
                        self.set_lost_connection().await;
                        return Err(Error::msg("Device did not answer CTRL-C during streaming"));
                    }
                }
                // The port reads with a 70 ms timeout, so this may be a stall of a live device:
                // stop and pause, keeping the scope so RUN resumes, unless CTRL-C goes unanswered.
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    tracing::error!("Timed out reading from continuous stream: {}", e);
                    self.set_lost_connection().await;
                    return cancel_with_deadline(move || streaming_scope.stop())
                        .await
                        .ok_or_else(|| {
                            Error::new(e).context("Device did not answer CTRL-C after a stall")
                        });
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

    fn calibrate_batch(probe: &FleaProbe, buffer: Vec<u16>) -> PolarsResult<Vec<f64>> {
        let ca = UInt16Chunked::from_vec(RAW_COLUMN_NAME.into(), buffer);
        let df = DataFrame::new(vec![ca.into_series().into()])?;
        let calibrated = probe.apply_calibration(df.lazy()).collect()?;
        Ok(calibrated
            .column(CALIBRATED_COLUMN_NAME)?
            .f64()?
            .into_no_null_iter()
            .collect())
    }

    fn convert_polars_to_data_points(df: &DataFrame) -> Result<(Vec<f64>, Vec<DataPoint>)> {
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

        let time_col = df
            .column(TIME_COLUMN_NAME)
            .context("Time column not found in DataFrame")?;
        let bnc_col = df
            .column(CALIBRATED_COLUMN_NAME)
            .context("BNC column not found in DataFrame")?;
        let bitmap_col = df
            .column(BITMAP_COLUMN_NAME)
            .context("Bitmap column not found in DataFrame")?;

        let time_values: Vec<f64> = time_col
            .f64()
            .context("Time column conversion failed")?
            .into_no_null_iter()
            .collect();
        let bnc_values: Vec<f64> = bnc_col
            .f64()
            .context("BNC column conversion failed")?
            .into_no_null_iter()
            .collect();

        // Handle string bitmap data (e.g., "0x1ff")
        // TODO maybe use the fleascope-rs function
        if bitmap_col.dtype() != &polars::datatypes::DataType::String {
            bail!(
                "Bitmap column has type {}, expected string",
                bitmap_col.dtype()
            );
        }
        let bitmap_values: Vec<u16> = {
            profiling::scope!("parse_bitmap_strings");

            bitmap_col
                .str()
                .context("Bitmap column conversion failed")?
                .into_iter()
                .map(|opt_str| {
                    let s = opt_str.ok_or_else(|| anyhow!("Null bitmap value encountered"))?;
                    // Hexadecimal string like "0x1ff"
                    s.get(2..)
                        .and_then(|hex| u16::from_str_radix(hex, 16).ok())
                        .ok_or_else(|| anyhow!("Invalid bitmap hex string '{}'", s))
                })
                .collect::<Result<_>>()?
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
        Ok((x_values, data_points))
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
            device_present: Box::new(|| true),
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

    /// The UI side of a worker, as `FleaScopeDevice` holds it. Dropping it after
    /// sending `Exit` is what `DeviceManager::remove_device` does.
    #[cfg(unix)]
    struct UiHandles {
        config_tx: watch::Sender<CaptureConfig>,
        _waveform_tx: watch::Sender<WaveformConfig>,
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
            _waveform_tx: waveform_tx,
            control_tx,
            notification_rx,
        }
    }

    /// A fake device on a pseudo terminal that answers every command and CTRL-C with the prompt.
    #[cfg(unix)]
    struct FakeFleaScope {
        port: String,
        plugged: Arc<AtomicBool>,
        answering: Arc<AtomicBool>,
        device: std::thread::JoinHandle<()>,
    }

    #[cfg(unix)]
    impl FakeFleaScope {
        fn plug_in() -> (Self, IdleFleaScope) {
            use serialport::SerialPort as _;
            use std::io::{ErrorKind, Read as _, Write as _};

            let (mut master, slave) = serialport::TTYPort::pair().unwrap();
            let plugged = Arc::new(AtomicBool::new(true));
            let answering = Arc::new(AtomicBool::new(true));
            let device = std::thread::spawn({
                let plugged = plugged.clone();
                let answering = answering.clone();
                move || {
                    let mut buf = [0u8; 256];
                    while plugged.load(Ordering::SeqCst) {
                        match master.read(&mut buf) {
                            Ok(_) if !answering.load(Ordering::SeqCst) => {}
                            Ok(n) => {
                                for _ in buf[..n]
                                    .iter()
                                    .filter(|&&byte| byte == b'\n' || byte == 0x03)
                                {
                                    master.write_all(b"> ").unwrap();
                                }
                            }
                            // The host closed its end of the port (the worker dropped the scope).
                            Err(e) if e.kind() == ErrorKind::BrokenPipe => break,
                            Err(e) => assert_eq!(e.kind(), ErrorKind::TimedOut, "{e}"),
                        }
                    }
                }
            });
            let port = slave.name().unwrap();
            let terminal = StatelessFleaTerminal::new(&port).unwrap();
            let scope = IdleFleaScope::new(IdleFleaTerminal::try_from(terminal).unwrap());
            drop(slave);
            (
                Self {
                    port,
                    plugged,
                    answering,
                    device,
                },
                scope,
            )
        }

        /// What `DeviceManager` gives the worker as `device_present`.
        fn presence(&self) -> Box<dyn Fn() -> bool + Send + Sync> {
            let port = self.port.clone();
            Box::new(move || port_attached(&port))
        }

        /// Keeps the port attached but stops answering, like a wedged device.
        fn go_silent(&self) {
            self.answering.store(false, Ordering::SeqCst);
        }

        /// Closes the master side. Like the kernel does to an open tty whose USB
        /// device is unplugged, this hangs up the port: every further write fails.
        fn unplug(self) {
            self.plugged.store(false, Ordering::SeqCst);
            self.device.join().unwrap();
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

    /// A fleascope-rs panic in a contained I/O thread must not wedge std's panic-hook
    /// lock: libtest swaps the hook after the last test, so `cargo test` would never exit.
    #[tokio::test]
    async fn panic_in_contained_io_leaves_panic_hook_usable() {
        let result = run_contained(CANCEL_TIMEOUT, || {
            std::hint::black_box(None::<()>).expect("Serial read error");
        })
        .await;
        assert!(result.is_none());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            std::panic::set_hook(std::panic::take_hook());
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the panic hook lock must stay free");
    }

    /// Runs the worker on its own thread and runtime, so a worker hung for good (a
    /// fleascope-rs `cancel()` waiting for a prompt) fails the test instead of hanging it.
    /// The test drives it from its own thread; the worker loops may never yield.
    #[cfg(unix)]
    fn run_on_own_thread(
        mut worker: FleaWorker,
        scope: IdleFleaScope,
    ) -> std::sync::mpsc::Receiver<()> {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(worker.run(scope));
            done_tx.send(()).unwrap();
        });
        done_rx
    }

    #[cfg(unix)]
    fn wait_until(
        status_rx: &watch::Receiver<WorkerStatus>,
        what: &str,
        done: impl Fn(&WorkerStatus) -> bool,
    ) {
        let deadline = Instant::now() + CANCEL_TIMEOUT + Duration::from_secs(2);
        while !done(&status_rx.borrow()) {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn next_notification(ui: &mut UiHandles) -> String {
        let deadline = Instant::now() + CANCEL_TIMEOUT + Duration::from_secs(2);
        loop {
            if let Ok(Notification::Success(message) | Notification::Error(message)) =
                ui.notification_rx.try_recv()
            {
                return message;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a notification"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// What `DeviceManager::remove_device` does; the worker must then finish.
    #[cfg(unix)]
    fn remove(ui: UiHandles, done: &std::sync::mpsc::Receiver<()>) {
        ui.control_tx.try_send(ControlCommand::Exit).unwrap();
        drop(ui);
        done.recv_timeout(CANCEL_TIMEOUT + Duration::from_secs(2))
            .expect("worker must stop once the device is removed");
    }

    /// A stream that times out may mean the device stopped answering. `stop()` would
    /// then wait forever for a prompt (`BusyFleaTerminal::cancel` has no deadline).
    #[cfg(unix)]
    #[test]
    fn continuous_read_timeout_on_silent_device_gives_up_instead_of_hanging() {
        let (mut worker, status_rx) = test_worker();
        let ui = attach_ui(&mut worker);
        let (device, scope) = FakeFleaScope::plug_in();
        device.go_silent();
        let done = run_on_own_thread(worker, scope);
        wait_until(&status_rx, "the connection is lost", |status| {
            !status.connected
        });
        remove(ui, &done);
        device.unplug();
    }

    /// In triggered mode a silent device looks like one waiting for its trigger, so
    /// STOP (or any settings change, or removal) reaches `cancel()` on it.
    #[cfg(unix)]
    #[test]
    fn stopping_triggered_capture_on_silent_device_gives_up_instead_of_hanging() {
        let (mut worker, status_rx) = test_worker();
        let ui = attach_ui(&mut worker);
        ui.config_tx.send_replace(CaptureConfig {
            probe_multiplier: ProbeType::X1,
            mode: CaptureMode::Triggered {
                trigger_config: TriggerConfig::default(),
                time_frame: 0.1,
            },
        });
        let (device, scope) = FakeFleaScope::plug_in();
        device.go_silent();
        let done = run_on_own_thread(worker, scope);
        std::thread::sleep(Duration::from_millis(200)); // let it wait for the trigger
        ui.control_tx.try_send(ControlCommand::Pause).unwrap();
        wait_until(&status_rx, "the connection is lost", |status| {
            !status.connected
        });
        remove(ui, &done);
        device.unplug();
    }

    /// A device that merely paused its stream for one read timeout still answers
    /// CTRL-C; the worker must keep its scope so RUN restarts the stream.
    #[cfg(unix)]
    #[test]
    fn continuous_read_timeout_on_live_device_stays_resumable() {
        let (mut worker, _status_rx) = test_worker();
        let mut ui = attach_ui(&mut worker);
        let (device, scope) = FakeFleaScope::plug_in();
        let done = run_on_own_thread(worker, scope);
        // The fake never streams samples, so every stream read times out.
        assert!(next_notification(&mut ui).contains("Lost connection"));
        ui.control_tx.try_send(ControlCommand::Resume).unwrap();
        let after_run = next_notification(&mut ui);
        assert!(
            after_run.contains("Lost connection"),
            "RUN must restart the stream, got {after_run:?}"
        );
        remove(ui, &done);
        device.unplug();
    }
}
