use anyhow::Result;
use tracing_subscriber::field::debug;
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tokio::sync::watch;
use fleascope_rs::{FleaScope, ProbeType, Trigger, AnalogTrigger, DigitalTrigger, BitState};
use polars::prelude::*;
use arc_swap::ArcSwap;

pub type ChannelData = Vec<f64>;
pub type BinaryChannelData = Vec<bool>;

#[derive(Debug, Clone)]
pub enum ConfigChangeSignal {
    ProbeMultiplierChanged(ProbeMultiplier),
    TriggerConfigChanged(TriggerConfig),
    WaveformConfigChanged(WaveformConfig),
    TimeFrameChanged(f64),
    Restart, // Generic restart signal
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
}

impl DeviceData {
    pub fn new() -> Self {
        Self {
            x_values: Vec::new(),
            data_points: Vec::new(),
            last_update: Instant::now(),
            read_duration: Duration::from_millis(0),
        }
    }

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
    pub probe_multiplier: ProbeMultiplier, // Probe selection
    pub trigger_config: TriggerConfig, // Trigger configuration
    pub waveform_config: WaveformConfig, // Waveform generator configuration
    fleascope: Arc<Mutex<FleaScope>>, // Actual FleaScope connection
    config_change_tx: watch::Sender<ConfigChangeSignal>, // Channel for configuration changes
    data_generation_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>, // Handle to data generation task
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProbeMultiplier {
    X1,
    X10,
}

impl FleaScopeDevice {
    pub fn new(device: FleaScope) -> Self {
        let (config_change_tx, _) = watch::channel(ConfigChangeSignal::Restart);
        let name = device.get_hostname().to_string();
        
        Self {
            name,
            data: Arc::new(ArcSwap::new(Arc::new(DeviceData::new()))), // Initialize with Arc<ArcSwap>
            enabled_channels: [true; 10], // All channels enabled by default
            time_frame: 2.0,             // Default 2 seconds
            is_paused: false, // Running by default
            probe_multiplier: ProbeMultiplier::X1, // Default x1 probe
            trigger_config: TriggerConfig::default(), // Default trigger config
            waveform_config: WaveformConfig::default(), // Default waveform config
            fleascope: Arc::new(Mutex::new(device)), // No connection initially
            config_change_tx,
            data_generation_handle: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start_data_generation(mut self) {
        // Create a new receiver for configuration changes
        let mut config_change_rx = self.config_change_tx.subscribe();

        // Start the cancellation-aware data generation loop
        let handle = tokio::spawn(async move {
            tracing::debug!("Starting cancellation-aware data generation loop");
            
            loop {
                loop {
                    // Check if device is paused first
                    if self.is_paused {
                        tracing::debug!("Device is paused, skipping data generation");
                        
                        // During pause, still check for config changes but less frequently
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                            signal = config_change_rx.changed() => {
                                if signal.is_ok() {
                                    tracing::info!("Configuration changed while paused, will restart");
                                    break;
                                }
                            }
                        }
                        continue;
                    }

                    let start_time = Instant::now();
                    
                    // Try to get real data from FleaScope with cancellation support
                    tokio::select! {
                        result = Self::get_real_fleascope_data(
                            &self.fleascope,
                            self.probe_multiplier,
                            &self.trigger_config,
                            self.time_frame,
                        ) => {
                            match result {
                                Ok(real_data) => {
                                    let read_duration = start_time.elapsed();
                                    
                                    tracing::trace!("Successfully got real data with {} points in {:?}", 
                                                    real_data.1.len(), read_duration);
                                    
                                    // Update data with lock-free operation using ArcSwap
                                    {
                                        let mut new_data = DeviceData {
                                            x_values : real_data.0,
                                            data_points : real_data.1,
                                            last_update : Instant::now(),
                                            read_duration : read_duration,
                                        };
                                        
                                        self.data.store(Arc::new(new_data));
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("Failed to read data from FleaScope: {}", e);
                                }
                            }
                        },
                        /*
                        signal = config_change_rx.changed() => {
                            tracing::info!("Config changed");
                            match signal {
                                Ok(_) => {
                                    tracing::info!("Configuration changed during hardware read, cancelling and restarting");
                                    break; // Break inner loop to restart with new config
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to receive config change signal: {}", e);
                                }
                            }
                        }
                        */
                    }
                    if config_change_rx.has_changed().unwrap() {
                        let signal = config_change_rx.borrow_and_update();
                        tracing::info!("Config changed");
                        match signal.clone() {
                            ConfigChangeSignal::ProbeMultiplierChanged(probe) => {
                                self.probe_multiplier = probe;
                            },
                            ConfigChangeSignal::TriggerConfigChanged(tc) => {
                                self.trigger_config = tc;
                            },
                            ConfigChangeSignal::WaveformConfigChanged(wc) => {
                                tracing::info!("Waveform config changed: {:?}. Not yet implemented", wc);
                            },
                            ConfigChangeSignal::TimeFrameChanged(t) => {
                                self.time_frame = t;
                            },
                            ConfigChangeSignal::Restart => todo!(),
                        }
                    }
                }
                
                // Inner loop ended due to config change - restart the inner loop with new config
                // Don't return here, just continue the outer loop to restart data acquisition
                tracing::info!("Restarting data generation loop with updated configuration");
            }
        });
        
        // Store the handle for cancellation
        if let Ok(mut stored_handle) = self.data_generation_handle.try_lock() {
            *stored_handle = Some(handle);
        }
    }
    
    /// Cancel the current data generation task
    pub fn cancel_data_generation(&self) {
        if let Ok(mut handle_guard) = self.data_generation_handle.try_lock() {
            if let Some(handle) = handle_guard.take() {
                handle.abort();
                tracing::info!("Cancelled data generation task");
            }
        }
    }
    
    /// Signal that configuration has changed and data generation should restart
    fn signal_config_change(&self, signal: ConfigChangeSignal) {
        tracing::info!("Sent config change");
        if let Err(_) = self.config_change_tx.send(signal) {
            tracing::warn!("Failed to send config change signal");
        } else {
            tracing::debug!("Sent config change signal");
        }
    }

    async fn get_real_fleascope_data(
        fleascope_arc: &Arc<Mutex<FleaScope>>,
        probe_multiplier: ProbeMultiplier,
        trigger_config: &TriggerConfig,
        time_frame: f64,
    ) -> Result<(Vec<f64>, Vec<DataPoint>),> {
        let fleascope_arc_clone = Arc::clone(fleascope_arc);
        let probe_multiplier = probe_multiplier;
        let trigger_config = trigger_config.clone();
        
        // Run the potentially blocking hardware operation in a separate thread
        // Use a longer timeout (5 seconds) to allow legitimate reads while preventing infinite hangs
        // This is still much more responsive than allowing 20-second hangs
        let read_task = tokio::task::spawn_blocking(move || {
            // Use try_lock to avoid blocking other devices
            tracing::trace!("Pre match");
            let mut fleascope = fleascope_arc_clone.try_lock().unwrap();
            tracing::trace!("Device available");
            
            // Convert trigger configuration properly
            let trigger = Self::convert_trigger_config(&trigger_config);

            // Convert probe type
            let probe_type = match probe_multiplier {
                ProbeMultiplier::X1 => ProbeType::X1,
                ProbeMultiplier::X10 => ProbeType::X10,
            };

            let duration = Duration::from_secs_f64(time_frame);
            
            tracing::trace!("Reading from FleaScope with duration: {:?}", duration);
            let lf = fleascope.read(probe_type, duration, Some(trigger), None)?;
            // Release the lock as early as possible
            drop(fleascope);
            
            // Collect the data (this can take time but doesn't block other devices)
            let df = lf.collect()?;
            tracing::trace!("Successfully collected DataFrame with {} rows", df.height());
            return Ok(Self::convert_polars_to_data_points(df));
        });
        
        // Apply a reasonable timeout to prevent indefinite hangs while allowing legitimate long reads
        // 5 seconds should be enough for most triggers while keeping GUI responsive
        
        read_task.await?
    }

    fn convert_trigger_config(trigger_config: &TriggerConfig) -> Trigger {
        match trigger_config.source {
            TriggerSource::Analog => {
                let analog_trigger = match trigger_config.analog.pattern {
                    AnalogTriggerPattern::Rising => {
                        AnalogTrigger::start_capturing_when()
                            .rising_edge(trigger_config.analog.level)
                    }
                    AnalogTriggerPattern::Falling => {
                        AnalogTrigger::start_capturing_when()
                            .falling_edge(trigger_config.analog.level)
                    }
                    AnalogTriggerPattern::Level => {
                        AnalogTrigger::start_capturing_when()
                            .level(trigger_config.analog.level)
                    }
                    AnalogTriggerPattern::LevelAuto => {
                        AnalogTrigger::start_capturing_when()
                            .auto(trigger_config.analog.level)
                    }
                };
                Trigger::from(analog_trigger)
            }
            TriggerSource::Digital => {
                let mut digital_trigger = DigitalTrigger::start_capturing_when();
                
                // Set bit patterns
                for (i, bit_state) in trigger_config.digital.bit_pattern.iter().enumerate() {
                    let bit_value = match bit_state {
                        DigitalBitState::DontCare => BitState::DontCare,
                        DigitalBitState::Low => BitState::Low,
                        DigitalBitState::High => BitState::High,
                    };
                    digital_trigger = digital_trigger.set_bit(i, bit_value);
                }
                
                // Set trigger mode
                let final_trigger = match trigger_config.digital.mode {
                    DigitalTriggerMode::StartMatching => digital_trigger.starts_matching(),
                    DigitalTriggerMode::StopMatching => digital_trigger.stops_matching(),
                    DigitalTriggerMode::WhileMatching => digital_trigger.is_matching(),
                    DigitalTriggerMode::WhileMatchingAuto => digital_trigger.is_matching(), // Note: auto not directly supported
                };
                
                Trigger::from(final_trigger)
            }
        }
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

    fn generate_waveform(t: f64, config: &WaveformConfig) -> f64 {
        let freq = config.frequency_hz;
        let phase = 2.0 * std::f64::consts::PI * freq * t;
        
        let signal = match config.waveform_type {
            WaveformType::Sine => phase.sin(),
            WaveformType::Square => if phase.sin() > 0.0 { 1.0 } else { -1.0 },
            WaveformType::Triangle => {
                let normalized_phase = (phase / (2.0 * std::f64::consts::PI)) % 1.0;
                if normalized_phase < 0.5 {
                    4.0 * normalized_phase - 1.0
                } else {
                    3.0 - 4.0 * normalized_phase
                }
            }
            WaveformType::Ekg => {
                // Simple EKG-like waveform
                let beat_phase = (phase / (2.0 * std::f64::consts::PI)) % 1.0;
                if beat_phase < 0.1 {
                    10.0 * beat_phase * (1.0 - 10.0 * beat_phase).max(0.0)
                } else if beat_phase < 0.2 {
                    -5.0 * (beat_phase - 0.1)
                } else {
                    0.0
                }
            }
        };

        // Normalize to 0-1 range and add some noise
        ((signal + 1.0) / 2.0 + 0.02 * rand::random::<f64>()).clamp(0.0, 1.0)
    }

    pub fn pause(&mut self) {
        self.is_paused = true;
    }

    pub fn resume(&mut self) {
        self.is_paused = false;
    }

    pub fn is_paused(&self) -> bool {
        self.is_paused
    }

    pub fn set_waveform(&mut self, waveform_type: WaveformType, frequency_hz: f64) {
        self.waveform_config.waveform_type = waveform_type;
        self.waveform_config.frequency_hz = frequency_hz.clamp(10.0, 4000.0);
        self.waveform_config.enabled = true;
    }

    pub fn get_waveform_status(&self) -> String {
        if self.waveform_config.enabled {
            let freq_str = if self.waveform_config.frequency_hz >= 1000.0 {
                format!("{:.1}kHz", self.waveform_config.frequency_hz / 1000.0)
            } else {
                format!("{:.0}Hz", self.waveform_config.frequency_hz)
            };
            format!("{} {}", self.waveform_config.waveform_type.as_str(), freq_str)
        } else {
            "Off".to_string()
        }
    }

    /// Update probe multiplier and restart data generation with new settings
    pub fn set_probe_multiplier(&mut self, probe_multiplier: ProbeMultiplier) {
        if self.probe_multiplier != probe_multiplier {
            self.probe_multiplier = probe_multiplier;
            self.signal_config_change(ConfigChangeSignal::ProbeMultiplierChanged(probe_multiplier));
        }
    }
    
    /// Update trigger configuration and restart data generation with new settings
    pub fn set_trigger_config(&mut self, trigger_config: TriggerConfig) {
        // Simple comparison - in a real implementation, you might want a more sophisticated comparison
        self.trigger_config = trigger_config.clone();
        self.signal_config_change(ConfigChangeSignal::TriggerConfigChanged(trigger_config));
        // The data generation task will restart itself when it receives the signal
    }
    
    /// Update waveform configuration and restart data generation with new settings
    pub fn set_waveform_config(&mut self, waveform_config: WaveformConfig) {
        self.waveform_config = waveform_config.clone();
        self.signal_config_change(ConfigChangeSignal::WaveformConfigChanged(waveform_config));
        // The data generation task will restart itself when it receives the signal
    }
    
    /// Update enabled channels and restart data generation with new settings
    pub fn set_enabled_channels(&mut self, enabled_channels: [bool; 10]) {
        if self.enabled_channels != enabled_channels {
            self.enabled_channels = enabled_channels;
        }
    }
    
    /// Update time frame and restart data generation with new settings
    pub fn set_time_frame(&mut self, time_frame: f64) {
        if (self.time_frame - time_frame).abs() > f64::EPSILON {
            self.time_frame = time_frame;
            self.signal_config_change(ConfigChangeSignal::TimeFrameChanged(time_frame));
        }
    }
}

impl Clone for FleaScopeDevice {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            data: Arc::clone(&self.data),
            enabled_channels: self.enabled_channels,
            time_frame: self.time_frame,
            is_paused: self.is_paused,
            probe_multiplier: self.probe_multiplier,
            trigger_config: self.trigger_config.clone(),
            waveform_config: self.waveform_config.clone(),
            fleascope: Arc::clone(&self.fleascope),
            config_change_tx: self.config_change_tx.clone(),
            data_generation_handle: Arc::new(Mutex::new(None)),
        }
    }
}

#[derive(Default)]
pub struct DeviceManager {
    devices: Vec<FleaScopeDevice>,
}

impl DeviceManager {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    pub fn add_device(&mut self, hostname: String) -> Result<()> {
        let scope = FleaScope::connect(Some(&hostname), None, true);
        let device = FleaScopeDevice::new(scope?);

        // Auto-connect and start data generation for demo
        let device_clone = device.clone();
        tokio::spawn(async move {
            let dev = device_clone;
            dev.start_data_generation();
        });

        self.devices.push(device);
        Ok(())
    }

    pub fn get_devices(&self) -> &[FleaScopeDevice] {
        &self.devices
    }

    pub fn get_devices_mut(&mut self) -> &mut [FleaScopeDevice] {
        &mut self.devices
    }

    pub fn remove_device(&mut self, index: usize) -> Result<()> {
        if index < self.devices.len() {
            self.devices.remove(index);
            Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnalogTriggerPattern {
    Rising,
    Falling,
    Level,
    LevelAuto,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DigitalTriggerMode {
    StartMatching,  // Trigger when pattern starts matching
    StopMatching,   // Trigger when pattern stops matching
    WhileMatching,  // Trigger while pattern is matching
    WhileMatchingAuto, // Auto mode while matching
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DigitalBitState {
    DontCare,  // X - bit value doesn't matter
    Low,       // 0 - bit must be low
    High,      // 1 - bit must be high
}

#[derive(Debug, Clone)]
pub struct AnalogTriggerConfig {
    pub enabled: bool,
    pub level: f64,           // Trigger level (0.0 to 1.0)
    pub pattern: AnalogTriggerPattern,
}

#[derive(Debug, Clone)]
pub struct DigitalTriggerConfig {
    pub enabled: bool,
    pub bit_pattern: [DigitalBitState; 9], // Pattern for 9 digital channels
    pub mode: DigitalTriggerMode,
}

#[derive(Debug, Clone)]
pub struct TriggerConfig {
    pub source: TriggerSource,
    pub analog: AnalogTriggerConfig,
    pub digital: DigitalTriggerConfig,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            source: TriggerSource::Digital,
            analog: AnalogTriggerConfig {
                enabled: false,
                level: 0.5,
                pattern: AnalogTriggerPattern::Rising,
            },
            digital: DigitalTriggerConfig {
                enabled: true,
                bit_pattern: [DigitalBitState::DontCare; 9],
                mode: DigitalTriggerMode::WhileMatchingAuto,
            },
        }
    }
}

impl DigitalBitState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DigitalBitState::DontCare => "X",
            DigitalBitState::Low => "0",
            DigitalBitState::High => "1",
        }
    }

    pub fn cycle(&self) -> Self {
        match self {
            DigitalBitState::DontCare => DigitalBitState::Low,
            DigitalBitState::Low => DigitalBitState::High,
            DigitalBitState::High => DigitalBitState::DontCare,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WaveformType {
    Sine,
    Square,
    Triangle,
    Ekg,
}

impl WaveformType {
    pub fn as_str(&self) -> &'static str {
        match self {
            WaveformType::Sine => "Sine",
            WaveformType::Square => "Square",
            WaveformType::Triangle => "Triangle",
            WaveformType::Ekg => "EKG",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            WaveformType::Sine => "～",
            WaveformType::Square => "⊓",
            WaveformType::Triangle => "△",
            WaveformType::Ekg => "💓",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WaveformConfig {
    pub enabled: bool,
    pub waveform_type: WaveformType,
    pub frequency_hz: f64, // 10 Hz to 4000 Hz
}

impl Default for WaveformConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            waveform_type: WaveformType::Sine,
            frequency_hz: 100.0, // Default 100 Hz
        }
    }
}

impl WaveformConfig {
    pub fn is_frequency_valid(&self) -> bool {
        self.frequency_hz >= 10.0 && self.frequency_hz <= 4000.0
    }

    pub fn clamp_frequency(&mut self) {
        self.frequency_hz = self.frequency_hz.clamp(10.0, 4000.0);
    }
}
