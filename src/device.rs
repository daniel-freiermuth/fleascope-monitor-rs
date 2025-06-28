use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use fleascope_rs::{FleaScope, ProbeType, Waveform, Trigger, AnalogTrigger, DigitalTrigger, BitState};
use polars::prelude::*;

pub type ChannelData = Vec<f64>;
pub type BinaryChannelData = Vec<bool>;

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
    pub sample_rate: f64,
    pub last_update: Instant,
    pub connected: bool,
}

impl DeviceData {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            x_values: Vec::new(),
            data_points: Vec::new(),
            sample_rate,
            last_update: Instant::now(),
            connected: false,
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
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub data: Arc<Mutex<DeviceData>>,
    pub enabled_channels: [bool; 10], // 1 analog + 9 digital
    pub time_frame: f64,              // Time window in seconds
    pub is_paused: Arc<AtomicBool>,   // Pause/continue state (thread-safe)
    pub probe_multiplier: ProbeMultiplier, // Probe selection
    pub trigger_config: TriggerConfig, // Trigger configuration
    pub waveform_config: WaveformConfig, // Waveform generator configuration
    fleascope: Arc<Mutex<Option<FleaScope>>>, // Actual FleaScope connection
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProbeMultiplier {
    X1,
    X10,
}

impl ProbeMultiplier {
    pub fn get_factor(&self) -> f64 {
        match self {
            ProbeMultiplier::X1 => 1.0,
            ProbeMultiplier::X10 => 10.0,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ProbeMultiplier::X1 => "x1",
            ProbeMultiplier::X10 => "x10",
        }
    }
}

impl FleaScopeDevice {
    pub fn new(id: String, name: String, hostname: String) -> Self {
        Self {
            id,
            name,
            hostname,
            data: Arc::new(Mutex::new(DeviceData::new(1000.0))),
            enabled_channels: [true; 10], // All channels enabled by default
            time_frame: 2.0,             // Default 2 seconds
            is_paused: Arc::new(AtomicBool::new(false)), // Running by default
            probe_multiplier: ProbeMultiplier::X1, // Default x1 probe
            trigger_config: TriggerConfig::default(), // Default trigger config
            waveform_config: WaveformConfig::default(), // Default waveform config
            fleascope: Arc::new(Mutex::new(None)), // No connection initially
        }
    }

    pub async fn connect(&self) -> Result<()> {
        // Try to connect to the actual FleaScope device
        let connection_result = if self.hostname.contains(':') {
            // For hostnames with port, we can't connect directly - use dummy mode
            tracing::warn!("Port-based hostname detected, using dummy mode for: {}", self.hostname);
            None
        } else {
            // Try to connect to real FleaScope device
            match FleaScope::connect(Some(&self.hostname), None, true) {
                Ok(scope) => {
                    tracing::info!("Successfully connected to real FleaScope device: {}", self.hostname);
                    Some(scope)
                }
                Err(e) => {
                    tracing::warn!("Failed to connect to FleaScope device {}: {}, using dummy mode", self.hostname, e);
                    None
                }
            }
        };

        // Store the connection
        {
            let mut fleascope = self.fleascope.lock().unwrap();
            *fleascope = connection_result;
        }

        // Update connection status in shared data
        {
            let mut data = self.data.lock().unwrap();
            data.connected = self.fleascope.lock().unwrap().is_some();
        }

        // Simulate connection delay
        sleep(Duration::from_millis(500)).await;

        tracing::info!("Connected to device: {}", self.name);
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
        // Close the FleaScope connection
        {
            let mut fleascope = self.fleascope.lock().unwrap();
            *fleascope = None;
        }

        // Update connection status in shared data
        {
            let mut data = self.data.lock().unwrap();
            data.connected = false;
        }

        tracing::info!("Disconnected from device: {}", self.name);
        Ok(())
    }

    pub fn start_data_generation(&self) {
        let data_arc = Arc::clone(&self.data);
        let is_paused_arc = Arc::clone(&self.is_paused);
        let fleascope_arc = Arc::clone(&self.fleascope);
        let probe_multiplier = self.probe_multiplier;
        let trigger_config = self.trigger_config.clone();
        let waveform_config = self.waveform_config.clone();
        let time_frame = self.time_frame;

        tokio::spawn(async move {
            let mut time_offset = 0.0;
            let sample_rate = 1000.0;
            let update_rate = 10.0; // Reduce to 10 Hz for real hardware to give more time
            let points_per_update = (sample_rate / update_rate) as usize;

            tracing::info!("Starting data generation loop with update rate: {} Hz", update_rate);

            loop {
                // sleep(Duration::from_millis((1000.0 / update_rate) as u64)).await;

                // Check if device is paused
                if is_paused_arc.load(Ordering::Relaxed) {
                    tracing::debug!("Device is paused, skipping data generation");
                    continue; // Skip data generation if paused
                }

                // Check if we have a real FleaScope connection
                let use_real_data = {
                    let fleascope = fleascope_arc.lock().unwrap();
                    fleascope.is_some()
                };

                tracing::info!("Data generation loop: use_real_data = {}", use_real_data);

                if use_real_data {
                    tracing::info!("Attempting to get real data from FleaScope");
                    // Try to get real data from FleaScope
                    match Self::get_real_fleascope_data(
                        &fleascope_arc,
                        probe_multiplier,
                        &trigger_config,
                        &waveform_config,
                        time_frame,
                    ).await {
                        Some(real_data) => {
                            tracing::info!("Successfully got real data with {} points", real_data.1.len());
                            // Update data without holding the guard across await
                            {
                                let mut data = data_arc.lock().unwrap();
                                data.x_values = real_data.0;
                                data.data_points = real_data.1;
                                data.last_update = Instant::now();
                            }
                            time_offset += points_per_update as f64 / sample_rate;
                        }
                        None => {
                            // Real data failed - just skip this iteration and try again
                            tracing::info!("Failed to get real data, skipping iteration");
                        }
                    }
                } else {
                    // No real hardware connected - wait and try again
                    tracing::info!("No real hardware connected, waiting...");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        });
    }

    async fn get_real_fleascope_data(
        fleascope_arc: &Arc<Mutex<Option<FleaScope>>>,
        probe_multiplier: ProbeMultiplier,
        trigger_config: &TriggerConfig,
        waveform_config: &WaveformConfig,
        time_frame: f64,
    ) -> Option<(Vec<f64>, Vec<DataPoint>)> {
        let mut fleascope_guard = fleascope_arc.lock().unwrap();
        if let Some(fleascope) = fleascope_guard.as_mut() {
            // Set up waveform generator if enabled
            if waveform_config.enabled {
                let waveform = match waveform_config.waveform_type {
                    WaveformType::Sine => Waveform::Sine,
                    WaveformType::Square => Waveform::Square,
                    WaveformType::Triangle => Waveform::Triangle,
                    WaveformType::Ekg => Waveform::Ekg,
                };
                let freq = waveform_config.frequency_hz.round() as i32;
                if let Err(e) = fleascope.set_waveform(waveform, freq) {
                    tracing::warn!("Failed to set waveform: {}", e);
                }
            }

            // Convert our trigger config to fleascope-rs trigger (use None for auto-trigger in demo)
            let trigger = None; // Use auto trigger for continuous data flow
            // let trigger = Self::convert_trigger_config(trigger_config); // Enable when trigger config is working

            // Convert probe type
            let probe_type = match probe_multiplier {
                ProbeMultiplier::X1 => ProbeType::X1,
                ProbeMultiplier::X10 => ProbeType::X10,
            };

            // Read data from the actual device
            let duration = Duration::from_millis(100); // Use shorter 100ms reads for faster response
            tracing::debug!("Reading from FleaScope with duration: {:?}", duration);
            match fleascope.read(probe_type, duration, trigger, None) {
                Ok(lazy_frame) => {
                    tracing::debug!("Successfully read lazy frame from FleaScope");
                    match lazy_frame.collect() {
                        Ok(df) => {
                            tracing::debug!("Successfully collected DataFrame with {} rows", df.height());
                            return Self::convert_polars_to_data_points(df);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to collect data frame: {}", e);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read from FleaScope: {}", e);
                }
            }
        }
        None
    }

    fn convert_trigger_config(trigger_config: &TriggerConfig) -> Option<Trigger> {
        match trigger_config.source {
            TriggerSource::Analog => {
                if trigger_config.analog.enabled {
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
                    Some(Trigger::from(analog_trigger))
                } else {
                    None
                }
            }
            TriggerSource::Digital => {
                if trigger_config.digital.enabled {
                    let mut digital_trigger = DigitalTrigger::start_capturing_when();
                    
                    // Set bit patterns
                    for (i, bit_state) in trigger_config.digital.bit_pattern.iter().enumerate() {
                        let bit_value = match bit_state {
                            DigitalBitState::DontCare => BitState::DontCare,
                            DigitalBitState::Low => BitState::Low,
                            DigitalBitState::High => BitState::High,
                        };
                        
                        digital_trigger = match i {
                            0 => digital_trigger.bit0(bit_value),
                            1 => digital_trigger.bit1(bit_value),
                            2 => digital_trigger.bit2(bit_value),
                            3 => digital_trigger.bit3(bit_value),
                            4 => digital_trigger.bit4(bit_value),
                            5 => digital_trigger.bit5(bit_value),
                            6 => digital_trigger.bit6(bit_value),
                            7 => digital_trigger.bit7(bit_value),
                            8 => digital_trigger.bit8(bit_value),
                            _ => digital_trigger,
                        };
                    }
                    
                    // Set trigger mode
                    let final_trigger = match trigger_config.digital.mode {
                        DigitalTriggerMode::StartMatching => digital_trigger.starts_matching(),
                        DigitalTriggerMode::StopMatching => digital_trigger.stops_matching(),
                        DigitalTriggerMode::WhileMatching => digital_trigger.is_matching(),
                        DigitalTriggerMode::WhileMatchingAuto => digital_trigger.is_matching(), // Note: auto not directly supported
                    };
                    
                    Some(Trigger::from(final_trigger))
                } else {
                    None
                }
            }
        }
    }

    fn convert_polars_to_data_points(df: DataFrame) -> Option<(Vec<f64>, Vec<DataPoint>)> {
        tracing::debug!("Converting DataFrame with columns: {:?}", df.get_column_names());
        tracing::debug!("DataFrame shape: {} rows, {} columns", df.height(), df.width());
        
        // Extract columns from the DataFrame
        let time_col = match df.column("time") {
            Ok(col) => col,
            Err(e) => {
                tracing::error!("Failed to get time column: {}", e);
                return None;
            }
        };
        
        let bnc_col = match df.column("bnc") {
            Ok(col) => col,
            Err(e) => {
                tracing::error!("Failed to get bnc column: {}", e);
                return None;
            }
        };
        
        let bitmap_col = match df.column("bitmap") {
            Ok(col) => col,
            Err(e) => {
                tracing::error!("Failed to get bitmap column: {}", e);
                return None;
            }
        };

        let time_values: Vec<f64> = match time_col.f64() {
            Ok(chunked) => chunked.into_no_null_iter().collect(),
            Err(e) => {
                tracing::error!("Failed to convert time column to f64: {}", e);
                return None;
            }
        };
        
        let bnc_values: Vec<f64> = match bnc_col.f64() {
            Ok(chunked) => chunked.into_no_null_iter().collect(),
            Err(e) => {
                tracing::error!("Failed to convert bnc column to f64: {}", e);
                return None;
            }
        };
        
        // Convert bitmap column - handle both string and numeric formats
        let bitmap_values: Vec<u16> = if bitmap_col.dtype() == &polars::datatypes::DataType::String {
            // Handle string bitmap data (e.g., "0x1ff", "0101010101", or "255")
            match bitmap_col.str() {
                Ok(chunked) => {
                    let mut values = Vec::new();
                    for opt_str in chunked.into_iter() {
                        match opt_str {
                            Some(s) => {
                                if s.starts_with("0x") || s.starts_with("0X") {
                                    // Hexadecimal string like "0x1ff"
                                    match u16::from_str_radix(&s[2..], 16) {
                                        Ok(val) => values.push(val),
                                        Err(e) => {
                                            tracing::error!("Failed to parse hex string '{}': {}", s, e);
                                            return None;
                                        }
                                    }
                                } else if s.chars().all(|c| c == '0' || c == '1') {
                                    // Binary string like "0101010101"
                                    match u16::from_str_radix(s, 2) {
                                        Ok(val) => values.push(val),
                                        Err(e) => {
                                            tracing::error!("Failed to parse binary string '{}': {}", s, e);
                                            return None;
                                        }
                                    }
                                } else {
                                    // Decimal string like "255"
                                    match s.parse::<u16>() {
                                        Ok(val) => values.push(val),
                                        Err(e) => {
                                            tracing::error!("Failed to parse decimal string '{}': {}", s, e);
                                            return None;
                                        }
                                    }
                                }
                            }
                            None => {
                                tracing::error!("Found null bitmap value");
                                return None;
                            }
                        }
                    }
                    values
                }
                Err(e) => {
                    tracing::error!("Failed to convert bitmap column to string: {}", e);
                    return None;
                }
            }
        } else {
            // Handle numeric bitmap data
            match bitmap_col.u16() {
                Ok(chunked) => chunked.into_no_null_iter().collect(),
                Err(e) => {
                    tracing::error!("Failed to convert bitmap column to u16: {}", e);
                    return None;
                }
            }
        };

        tracing::debug!("Extracted {} time values, {} BNC values, {} bitmap values", 
                       time_values.len(), bnc_values.len(), bitmap_values.len());

        // Verify we have data
        if time_values.is_empty() || bnc_values.is_empty() || bitmap_values.is_empty() {
            tracing::error!("One or more data vectors are empty");
            return None;
        }

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
        Some((x_values, data_points))
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

    pub fn is_connected(&self) -> bool {
        // Check both data connection status and actual FleaScope connection
        let has_fleascope = {
            let fleascope = self.fleascope.lock().unwrap();
            fleascope.is_some()
        };
        
        // Return true if we have data connection (even if using dummy data)
        self.data.lock().unwrap().connected || has_fleascope
    }

    pub fn pause(&self) {
        self.is_paused.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.is_paused.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.is_paused.load(Ordering::Relaxed)
    }

    pub fn set_waveform(&mut self, waveform_type: WaveformType, frequency_hz: f64) {
        self.waveform_config.waveform_type = waveform_type;
        self.waveform_config.frequency_hz = frequency_hz.clamp(10.0, 4000.0);
        self.waveform_config.enabled = true;
    }

    pub fn disable_waveform(&mut self) {
        self.waveform_config.enabled = false;
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
}

impl Clone for FleaScopeDevice {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            name: self.name.clone(),
            hostname: self.hostname.clone(),
            data: Arc::clone(&self.data),
            enabled_channels: self.enabled_channels,
            time_frame: self.time_frame,
            is_paused: Arc::clone(&self.is_paused),
            probe_multiplier: self.probe_multiplier,
            trigger_config: self.trigger_config.clone(),
            waveform_config: self.waveform_config.clone(),
            fleascope: Arc::clone(&self.fleascope),
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
        let id = format!("device_{}", self.devices.len());
        let name = format!("FleaScope {}", hostname);
        let device = FleaScopeDevice::new(id, name, hostname);

        // Auto-connect and start data generation for demo
        let device_clone = device.clone();
        tokio::spawn(async move {
            let dev = device_clone;
            if dev.connect().await.is_ok() {
                dev.start_data_generation();
            }
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
            source: TriggerSource::Analog,
            analog: AnalogTriggerConfig {
                enabled: true,
                level: 0.5,
                pattern: AnalogTriggerPattern::Rising,
            },
            digital: DigitalTriggerConfig {
                enabled: false,
                bit_pattern: [DigitalBitState::DontCare; 9],
                mode: DigitalTriggerMode::StartMatching,
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
