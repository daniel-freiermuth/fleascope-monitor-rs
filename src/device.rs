use anyhow::{Error, Result};
use std::{panic};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch::{self, Sender};
use fleascope_rs::{AnalogTrigger, AnalogTriggerBuilder, BitState, DigitalTrigger, FleaProbe, IdleFleaScope, ProbeType, Trigger, Waveform};
use polars::prelude::*;
use arc_swap::ArcSwap;

// Time frame constants for consistent validation
pub const MIN_TIME_FRAME: f64 = 0.000122; // 122μs
pub const MAX_TIME_FRAME: f64 = 3.49;     // 3.49s

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub probe_multiplier: ProbeType,
    pub trigger_config: TriggerConfig,
    pub time_frame: f64,
    pub is_paused: bool,
}

pub enum Notification {
    Message(String),
    Success(String),
    Error(String),  
}

#[derive(Debug)]
pub enum ControlCommand {
    Calibrate0V(ProbeType),
    Calibrate3V(ProbeType),
    StoreCalibration(),
    Exit,
}

#[derive(Debug)]
pub enum CalibrationResult {
    Success(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct DataPoint {
    pub timestamp: f64,
    pub analog_channel: f64,
    pub digital_channels: [bool; 9],
}

#[derive(Debug, Clone)]
pub struct DeviceData {
    pub x_values: Vec<f64>,
    pub data_points: Vec<DataPoint>,
    pub last_update: Instant,
    pub read_duration: Duration,      // Last read operation duration
    pub update_rate: f64,
}

impl DeviceData {
    pub fn get_analog_data(&self) -> (Vec<f64>, Vec<f64>) {
        let x = self.x_values.clone();
        let y = self.data_points.iter().map(|p| p.analog_channel).collect();
        (x, y)
    }

    pub fn get_digital_channel_data(&self, channel: usize) -> (Vec<f64>, Vec<f64>) {
        if channel >= 9 {
            return (Vec::new(), Vec::new());
        }

        let x = self.x_values.clone();
        let y = self
            .data_points
            .iter()
            .map(|p| {
                if p.digital_channels[channel] {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();
        (x, y)
    }
}

pub struct FleaScopeDevice {
    pub name: String,
    pub data: Arc<ArcSwap<DeviceData>>, // Changed to Arc<ArcSwap> for sharing between threads
    pub enabled_channels: [bool; 10], // 1 analog + 9 digital
    pub time_frame: f64,             // Time window in seconds
    pub is_paused: bool,   // Pause/continue state (thread-safe)
    pub probe_multiplier: ProbeType, // Probe selection
    pub trigger_config: TriggerConfig, // Trigger configuration
    pub waveform_config: WaveformConfig, // Waveform generator configuration
    config_change_tx: watch::Sender<CaptureConfig>, // Channel for configuration changes
    control_signal_tx: tokio::sync::mpsc::Sender<ControlCommand>, // Channel for calibration commands
    pub notification_rx: tokio::sync::mpsc::Receiver<Notification>, // Channel for calibration results
    waveform_tx: Sender<WaveformConfig>, // Channel for waveform configuration
}

struct FleaWorker {
    fleascope: IdleFleaScope,
    data: Arc<ArcSwap<DeviceData>>, // Changed to Arc<ArcSwap> for sharing between threads
    config_change_rx: watch::Receiver<CaptureConfig>, // Channel for configuration changes
    control_rx: tokio::sync::mpsc::Receiver<ControlCommand>, // Channel for calibration commands
    notification_tx: tokio::sync::mpsc::Sender<Notification>, // Channel for calibration results
    waveform_rx: tokio::sync::watch::Receiver<WaveformConfig>, // Channel for waveform configuration
    x1: FleaProbe,
    x10: FleaProbe,
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
                    &trigger_str,
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

                        let (idle_scope, f, data_s) = fleascope_for_read.wait();
                        self.fleascope = idle_scope;

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

impl FleaScopeDevice {
    pub fn new(
        name: String, 
        config_change_tx: Sender<CaptureConfig>, 
        data: Arc<ArcSwap<DeviceData>>,
        calibration_tx: tokio::sync::mpsc::Sender<ControlCommand>,
        notification_rx: tokio::sync::mpsc::Receiver<Notification>,
        initial_config: CaptureConfig,
        waveform_tx: Sender<WaveformConfig>,
        initial_waveform: WaveformConfig,
    ) -> Self {
        Self {
            name,
            data,
            enabled_channels: [true; 10], // All channels enabled by default
            time_frame: initial_config.time_frame,             // Default 2 seconds
            is_paused: initial_config.is_paused,
            probe_multiplier: initial_config.probe_multiplier, // Default x1 probe
            trigger_config: initial_config.trigger_config, // Default trigger config
            waveform_config: initial_waveform, // Default waveform config
            config_change_tx,
            control_signal_tx: calibration_tx,
            notification_rx,
            waveform_tx,
        }
    }

    /// Signal that configuration has changed and data generation should restart
    fn signal_config_change(&self) {
        self.config_change_tx.send(CaptureConfig {
            probe_multiplier: self.probe_multiplier,
            trigger_config: self.trigger_config.clone(),
            time_frame: self.time_frame,
            is_paused: self.is_paused,
        }).expect("Failed to send config change signal");
    }

    pub fn pause(&mut self) {
        self.is_paused = true;
        self.signal_config_change();
    }

    pub fn stop(mut self) {
        self.control_signal_tx
            .try_send(ControlCommand::Exit)
            .expect("Failed to send exit command");
    }

    pub fn resume(&mut self) {
        self.is_paused = false;
        self.signal_config_change();
    }

    pub fn is_paused(&self) -> bool {
        self.is_paused
    }

    pub fn set_waveform(&mut self, waveform_type: Waveform, frequency_hz: i32) {
        self.waveform_config.waveform_type = waveform_type;
        self.waveform_config.frequency_hz = frequency_hz.clamp(10, 4000);
        self.waveform_config.enabled = true;
        self.waveform_tx.send(self.waveform_config.clone())
            .expect("Failed to send waveform configuration");
    }

    pub fn get_waveform_status(&self) -> String {
        if self.waveform_config.enabled {
            let freq_str = if self.waveform_config.frequency_hz >= 1000 {
                format!("{:.1}kHz", self.waveform_config.frequency_hz / 1000)
            } else {
                format!("{:.0}Hz", self.waveform_config.frequency_hz)
            };
            format!("{} {}", self.waveform_config.waveform_type.as_str(), freq_str)
        } else {
            "Off".to_string()
        }
    }

    pub fn set_probe_multiplier(&mut self, multiplier: ProbeType) {
        self.probe_multiplier = multiplier;
        self.signal_config_change();
    }

    pub fn set_trigger_config(&mut self, trigger_config: TriggerConfig) {
        self.trigger_config = trigger_config;
        self.signal_config_change();
    }

    pub fn set_enabled_channels(&mut self, enabled: [bool; 10]) {
        self.enabled_channels = enabled;
    }

    pub fn set_time_frame(&mut self, time_frame: f64) {
        // Clamp time frame to valid range: 122μs to 3.49s
        self.time_frame = time_frame.clamp(MIN_TIME_FRAME, MAX_TIME_FRAME);
        self.signal_config_change();
    }

    /// Send 0V calibration command (non-blocking)
    pub fn start_calibrate_0v(&self) -> Result<(), anyhow::Error> {
        self.control_signal_tx
            .try_send(ControlCommand::Calibrate0V(self.probe_multiplier))
            .map_err(|e| anyhow::anyhow!("Failed to send calibration command: {}", e))
    }

    /// Send 3V calibration command (non-blocking)  
    pub fn start_calibrate_3v(&self) -> Result<(), anyhow::Error> {
        self.control_signal_tx
            .try_send(ControlCommand::Calibrate3V(self.probe_multiplier))
            .map_err(|e| anyhow::anyhow!("Failed to send calibration command: {}", e))
    }

    /// Send store calibration command (non-blocking)
    pub fn start_store_calibration(&self) -> Result<(), anyhow::Error> {
        self.control_signal_tx
            .try_send(ControlCommand::StoreCalibration())
            .map_err(|e| anyhow::anyhow!("Failed to send storage command: {}", e))
    }
}

#[derive(Default)]
pub struct DeviceManager {
    devices: Vec<FleaScopeDevice>,
}

impl DeviceManager {
    pub fn add_device(&mut self, hostname: String) -> Result<()> {
        let (scope, x1, x10) = IdleFleaScope::connect(Some(&hostname), None, true)?;
        let initial_config = CaptureConfig {
            probe_multiplier: ProbeType::X1,
            trigger_config: TriggerConfig::default(),
            time_frame: 0.1, // Default 2 seconds
            is_paused: false,
        };
        let initial_waveform = WaveformConfig::default();

        let (capture_config_tx, capture_config_rx) = watch::channel(initial_config.clone());
        let (waveform_tx, waveform_rx) = watch::channel(initial_waveform.clone());

        // Create calibration channels
        let (calibration_tx, calibration_rx) = tokio::sync::mpsc::channel::<ControlCommand>(32);
        let (notification_tx, notification_rx) = tokio::sync::mpsc::channel::<Notification>(32);

        let data = Arc::new(ArcSwap::new(Arc::new(DeviceData {
                x_values: Vec::new(),
                data_points: Vec::new(),
                last_update: Instant::now(),
                read_duration: Duration::ZERO,
                update_rate: 0.0,
            })));

        let worker = FleaWorker {
            fleascope: scope, // Wrap in Some for handling during calibration
            data: data.clone(),
            config_change_rx: capture_config_rx,
            control_rx: calibration_rx,
            notification_tx,
            x1, x10,
            waveform_rx, // Channel for waveform configuration
        };

        let device = FleaScopeDevice::new(
            hostname,
            capture_config_tx,
            data,
            calibration_tx,
            notification_rx,
            initial_config,
            waveform_tx,
            initial_waveform,
        );
        let _handle = worker.start_data_generation(); // Store handle for proper lifecycle management

        self.devices.push(device);
        Ok(())
    }

    pub fn get_devices(&self) -> &[FleaScopeDevice] {
        &self.devices
    }

    pub fn get_devices_mut(&mut self) -> &mut [FleaScopeDevice] {
        &mut self.devices
    }

    pub fn remove_device(&mut self, index: usize) -> Result<FleaScopeDevice> {
        if index < self.devices.len() {
            Ok(self.devices.remove(index))
        } else {
            Err(anyhow::anyhow!("Device index out of bounds"))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TriggerSource {
    Analog,
    Digital,
}

#[derive(Debug, Clone)]
pub struct TriggerConfig {
    pub source: TriggerSource,
    pub analog: AnalogTrigger,
    pub digital: DigitalTrigger,
}

impl Into<Trigger> for TriggerConfig {
    fn into(self) -> Trigger {
        match self.source {
            TriggerSource::Analog => self.analog.into(),
            TriggerSource::Digital => self.digital.into(),
        }
    }
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            source: TriggerSource::Digital,
            analog: AnalogTrigger::start_capturing_when().auto(0.0),
            digital: DigitalTrigger::start_capturing_when().is_matching(),
        }
    }
}

pub fn bitstate_to_str(state: BitState) -> &'static str {
    match state {
        BitState::DontCare => "?",
        BitState::High => "1",
        BitState::Low => "0",
    }
}

pub fn cycle_bitstate(state: BitState) -> BitState {
    match state {
        BitState::DontCare => BitState::High,
        BitState::High => BitState::Low,
        BitState::Low => BitState::DontCare,
    }
}

pub fn waveform_to_str(waveform: Waveform) -> &'static str {
    match waveform {
        Waveform::Sine => "Sine",
        Waveform::Square => "Square",
        Waveform::Triangle => "Triangle",
        Waveform::Ekg => "EKG",
    }
}

pub fn waveform_to_icon(waveform: Waveform) -> &'static str {
    match waveform {
        Waveform::Sine => "～",
        Waveform::Square => "⊓",
        Waveform::Triangle => "△",
        Waveform::Ekg => "💓",
    }
}

#[derive(Debug, Clone)]
pub struct WaveformConfig {
    pub enabled: bool,
    pub waveform_type: Waveform,
    pub frequency_hz: i32, // 10 Hz to 4000 Hz
}

impl Default for WaveformConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            waveform_type: Waveform::Sine,
            frequency_hz: 100, // Default 100 Hz
        }
    }
}

impl WaveformConfig {
    pub fn is_frequency_valid(&self) -> bool {
        self.frequency_hz >= 10 && self.frequency_hz <= 4000
    }

    pub fn clamp_frequency(&mut self) {
        self.frequency_hz = self.frequency_hz.clamp(10, 4000);
    }
}
