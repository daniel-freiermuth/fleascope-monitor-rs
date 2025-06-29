use anyhow::Result;
use std::{panic};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch::{self, Sender};
use fleascope_rs::{FleaScope, ProbeType, Trigger, AnalogTrigger, DigitalTrigger, BitState};
use polars::prelude::*;
use arc_swap::ArcSwap;

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub probe_multiplier: ProbeMultiplier,
    pub trigger_config: TriggerConfig,
    pub time_frame: f64,
}

#[derive(Debug, Clone)]
pub enum ConfigChangeSignal {
    NewConfigSignal(CaptureConfig),
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
    pub probe_multiplier: ProbeMultiplier, // Probe selection
    pub trigger_config: TriggerConfig, // Trigger configuration
    pub waveform_config: WaveformConfig, // Waveform generator configuration
    config_change_tx: watch::Sender<ConfigChangeSignal>, // Channel for configuration changes
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProbeMultiplier {
    X1,
    X10,
}

impl Into<ProbeType> for ProbeMultiplier {
    fn into(self) -> ProbeType {
        match self {
            ProbeMultiplier::X1 => ProbeType::X1,
            ProbeMultiplier::X10 => ProbeType::X10,
        }
    }
}

struct FleaWorker {
    fleascope: Arc<Mutex<FleaScope>>,
    data: Arc<ArcSwap<DeviceData>>, // Changed to Arc<ArcSwap> for sharing between threads
    is_paused: bool,   // Pause/continue state (thread-safe)
    config_change_rx: watch::Receiver<ConfigChangeSignal>, // Channel for configuration changes
}

impl FleaWorker {
    pub fn start_data_generation(self) -> tokio::task::JoinHandle<()> {
        // Create a new receiver for configuration changes
        let mut config_change_rx = self.config_change_rx;
        let mut update_rate = 0.0;
        let mut last_rate_update = Instant::now();
        let mut read_count = 0;

        // Start the cancellation-aware data generation loop
        tokio::spawn(async move {
            tracing::debug!("Starting cancellation-aware data generation loop");
            
            loop {
                tracing::debug!("Starting new data generation iteration");
                    // Check if device is paused first
                    if self.is_paused {
                        tracing::debug!("Device is paused, skipping data generation");
                        
                        // During pause, still check for config changes but less frequently
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                            signal = config_change_rx.changed() => {
                                if signal.is_ok() {
                                    tracing::info!("Configuration changed while paused, will restart");
                                }
                            }
                        }
                        continue;
                    }

                    tracing::debug!("Device is running, starting data generation");
                    let start_time = Instant::now();
                    let capture_config = match config_change_rx.borrow_and_update().clone() {
                        ConfigChangeSignal::NewConfigSignal(config) => config,
                    };

                    
                    // Try to get real data from FleaScope with cancellation support
                    tokio::select! {
                        result = Self::get_real_fleascope_data(
                            &self.fleascope,
                            capture_config.probe_multiplier,
                            &capture_config.trigger_config,
                            capture_config.time_frame,
                        ) => {
                            match result {
                                Ok(real_data) => {
                                    let read_duration = start_time.elapsed();
                                    
                                    tracing::trace!("Successfully got real data with {} points in {:?}", 
                                                    real_data.1.len(), read_duration);
                                    
                                    // Update data with lock-free operation using ArcSwap
                                    {
                                        let new_data = DeviceData {
                                            x_values : real_data.0,
                                            data_points : real_data.1,
                                            last_update : Instant::now(),
                                            read_duration : read_duration,
                                            update_rate,
                                        };
                                        self.data.store(Arc::new(new_data));
                                    }
                                    if last_rate_update.elapsed() >= Duration::from_secs(1) {
                                        update_rate = read_count as f64 / last_rate_update.elapsed().as_secs_f64();
                                        read_count = 0;
                                        last_rate_update = Instant::now();
                                    }
                                    read_count += 1;
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
                        tracing::info!("Config changed: {:?}", signal);
                        
                        match signal.clone() {
                            ConfigChangeSignal::NewConfigSignal(capture_config) => {},
                        }
                }
                
                // Inner loop ended due to config change - restart the inner loop with new config
                // Don't return here, just continue the outer loop to restart data acquisition
                tracing::info!("Restarting data generation loop with updated configuration");
            }
        })
        
    }

    async fn get_real_fleascope_data(
        fleascope_arc: &Arc<Mutex<FleaScope>>,
        probe_multiplier: ProbeMultiplier,
        trigger_config: &TriggerConfig,
        time_frame: f64,
    ) -> Result<(Vec<f64>, Vec<DataPoint>),> {
        let fleascope_arc_clone = Arc::clone(fleascope_arc);
        let trigger_config = trigger_config.clone();
        
        // Run the potentially blocking hardware operation in a separate thread
        // Use a longer timeout (5 seconds) to allow legitimate reads while preventing infinite hangs
        // This is still much more responsive than allowing 20-second hangs
        tokio::task::spawn_blocking(move || {
            // Use try_lock to avoid blocking other devices
            tracing::trace!("Pre match");
            let mut fleascope = fleascope_arc_clone.try_lock().unwrap();
            tracing::trace!("Device available");
            
            let duration = Duration::from_secs_f64(time_frame);
            
            tracing::trace!("Reading from FleaScope with duration: {:?}", duration);
            let lf = fleascope.read(probe_multiplier.into(), duration, Some(trigger_config.into()), None)?;
            // Release the lock as early as possible
            drop(fleascope);
            
            // Collect the data (this can take time but doesn't block other devices)
            let df = lf.collect()?;
            tracing::trace!("Successfully collected DataFrame with {} rows", df.height());
            return Ok(Self::convert_polars_to_data_points(df));
        }).await?
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
    pub fn new(name: String, config_change_tx: Sender<ConfigChangeSignal>, data: Arc<ArcSwap<DeviceData>>) -> Self {
        Self {
            name,
            data,
            enabled_channels: [true; 10], // All channels enabled by default
            time_frame: 2.0,             // Default 2 seconds
            is_paused: false, // Running by default
            probe_multiplier: ProbeMultiplier::X1, // Default x1 probe
            trigger_config: TriggerConfig::default(), // Default trigger config
            waveform_config: WaveformConfig::default(), // Default waveform config
            config_change_tx,
        }
    }

    /// Signal that configuration has changed and data generation should restart
    fn signal_config_change(&self) {
        self.config_change_tx.send(ConfigChangeSignal::NewConfigSignal(CaptureConfig {
            probe_multiplier: self.probe_multiplier,
            trigger_config: self.trigger_config.clone(),
            time_frame: self.time_frame,
        })).expect("Failed to send config change signal");
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

    pub fn set_probe_multiplier(&mut self, multiplier: ProbeMultiplier) {
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
        self.time_frame = time_frame;
        self.signal_config_change();
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
        let (config_change_tx, rx) = watch::channel(ConfigChangeSignal::NewConfigSignal(CaptureConfig {
            probe_multiplier: ProbeMultiplier::X1,
            trigger_config: TriggerConfig::default(),
            time_frame: 0.1, // Default 2 seconds
        }));

        let data = Arc::new(ArcSwap::new(Arc::new(DeviceData {
                x_values: Vec::new(),
                data_points: Vec::new(),
                last_update: Instant::now(),
                read_duration: Duration::ZERO,
                update_rate: 0.0,
            })));

        let worker = FleaWorker {
            fleascope: Arc::new(Mutex::new(scope?)),
            data: data.clone(),
            is_paused: false,
            config_change_rx: rx,
        };

        let device = FleaScopeDevice::new(hostname, config_change_tx, data);
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

impl Into<BitState> for &DigitalBitState {
    fn into(self) -> BitState {
        match self {
            DigitalBitState::DontCare => BitState::DontCare,
            DigitalBitState::Low => BitState::Low,
            DigitalBitState::High => BitState::High,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnalogTriggerConfig {
    pub level: f64,           // Trigger level (0.0 to 1.0)
    pub pattern: AnalogTriggerPattern,
}

impl Into<Trigger> for AnalogTriggerConfig {
    fn into(self) -> Trigger {
        let analog_trigger = match self.pattern {
            AnalogTriggerPattern::Rising => AnalogTrigger::start_capturing_when().rising_edge(self.level),
            AnalogTriggerPattern::Falling => AnalogTrigger::start_capturing_when().falling_edge(self.level),
            AnalogTriggerPattern::Level => AnalogTrigger::start_capturing_when().level(self.level),
            AnalogTriggerPattern::LevelAuto => AnalogTrigger::start_capturing_when().auto(self.level),
        };
        Trigger::from(analog_trigger)
    }
}

#[derive(Debug, Clone)]
pub struct DigitalTriggerConfig {
    pub bit_pattern: [DigitalBitState; 9], // Pattern for 9 digital channels
    pub mode: DigitalTriggerMode,
}

impl Into<Trigger> for DigitalTriggerConfig {
    fn into(self) -> Trigger {
        let mut digital_trigger = DigitalTrigger::start_capturing_when();
        
        // Set bit patterns
        for (i, bit_state) in self.bit_pattern.iter().enumerate() {
            digital_trigger = digital_trigger.set_bit(i, bit_state.into());
        }
        
        // Set trigger mode
        Trigger::from(match self.mode {
            DigitalTriggerMode::StartMatching => digital_trigger.starts_matching(),
            DigitalTriggerMode::StopMatching => digital_trigger.stops_matching(),
            DigitalTriggerMode::WhileMatching => digital_trigger.is_matching(),
            DigitalTriggerMode::WhileMatchingAuto => digital_trigger.is_matching(), // Note: auto not directly supported
        })
    }
}
#[derive(Debug, Clone)]
pub struct TriggerConfig {
    pub source: TriggerSource,
    pub analog: AnalogTriggerConfig,
    pub digital: DigitalTriggerConfig,
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
            analog: AnalogTriggerConfig {
                level: 0.5,
                pattern: AnalogTriggerPattern::Rising,
            },
            digital: DigitalTriggerConfig {
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
