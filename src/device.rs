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
    config_sender: Option<tokio::sync::mpsc::UnboundedSender<ConfigUpdate>>, // NEW: config update channel
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
            config_sender: None, // No config sender initially
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

    pub fn start_data_generation(&mut self) {
        let data_arc = Arc::clone(&self.data);
        let is_paused_arc = Arc::clone(&self.is_paused);
        let fleascope_arc = Arc::clone(&self.fleascope);
        // Create channel for config updates
        let (config_sender, mut config_receiver) = tokio::sync::mpsc::unbounded_channel::<ConfigUpdate>();
        self.config_sender = Some(config_sender);
        // Initial config
        let mut current_config = ConfigUpdate {
            probe_multiplier: self.probe_multiplier,
            trigger_config: self.trigger_config.clone(),
            waveform_config: self.waveform_config.clone(),
            time_frame: self.time_frame,
        };
        tokio::spawn(async move {
            let mut time_offset = 0.0;
            let sample_rate = 1000.0;
            let mut adaptive_delay = std::time::Duration::from_millis(50);
            let points_per_update = (sample_rate / 10.0) as usize;
            let mut consecutive_failures = 0;
            let mut last_successful_read = Instant::now();
            let mut last_rate_update = Instant::now();
            let mut read_count = 0;
            let mut ongoing_read: Option<tokio::task::JoinHandle<Option<(Vec<f64>, Vec<DataPoint>)>>> = None;
            loop {
                if is_paused_arc.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                // Apply config updates
                while let Ok(new_config) = config_receiver.try_recv() {
                    if let Some(handle) = ongoing_read.take() { handle.abort(); }
                    current_config = new_config;
                }
                let use_real_data = {
                    let fleascope = fleascope_arc.lock().unwrap();
                    fleascope.is_some()
                };
                if use_real_data {
                    let start_time = Instant::now();
                    let fleascope_clone = Arc::clone(&fleascope_arc);
                    let config_clone = current_config.clone();
                    let read_handle = tokio::spawn(async move {
                        FleaScopeDevice::get_real_fleascope_data(
                            &fleascope_clone,
                            config_clone.probe_multiplier,
                            &config_clone.trigger_config,
                            &config_clone.waveform_config,
                            config_clone.time_frame,
                        ).await
                    });
                    ongoing_read = Some(read_handle);
                    tokio::select! {
                        read_result = ongoing_read.as_mut().unwrap() => {
                            ongoing_read = None;
                            match read_result {
                                Ok(Some(real_data)) => { // Ok from JoinHandle, Some from get_real_fleascope_data
                                    let read_duration = start_time.elapsed();
                                    consecutive_failures = 0;
                                    last_successful_read = Instant::now();
                                    read_count += 1;
                                    if let Ok(mut data) = data_arc.lock() {
                                        data.x_values = real_data.0;
                                        data.data_points = real_data.1;
                                        data.last_update = Instant::now();
                                    }
                                    time_offset += points_per_update as f64 / sample_rate;
                                    adaptive_delay = if read_duration < Duration::from_millis(50) {
                                        Duration::from_millis(20)
                                    } else if read_duration < Duration::from_millis(200) {
                                        Duration::from_millis(50)
                                    } else if read_duration < Duration::from_millis(1000) {
                                        Duration::from_millis(100)
                                    } else {
                                        Duration::from_millis(200)
                                    };
                                }
                                Ok(None) => {
                                    consecutive_failures += 1;
                                    let backoff_delay = match consecutive_failures {
                                        1..=3 => Duration::from_millis(100),
                                        4..=10 => Duration::from_millis(250),
                                        _ => Duration::from_millis(500),
                                    };
                                    adaptive_delay = backoff_delay;
                                    if last_successful_read.elapsed() > Duration::from_secs(10) {
                                        adaptive_delay = Duration::from_millis(1000);
                                    }
                                }
                                Err(_) => {
                                    // Task was aborted or panicked
                                    continue;
                                }
                            }
                        }
                        new_config = config_receiver.recv() => {
                            if let Some(new_config) = new_config {
                                if let Some(handle) = ongoing_read.take() { handle.abort(); }
                                current_config = new_config;
                                continue;
                            }
                        }
                    }
                } else {
                    adaptive_delay = Duration::from_millis(500);
                }
                tokio::time::sleep(adaptive_delay).await;
            }
        });
    }

    // Add a helper to send config updates
    fn send_config_update(&self) {
        if let Some(sender) = &self.config_sender {
            let _ = sender.send(ConfigUpdate {
                probe_multiplier: self.probe_multiplier,
                trigger_config: self.trigger_config.clone(),
                waveform_config: self.waveform_config.clone(),
                time_frame: self.time_frame,
            });
        }
    }

    // Update all config setters to call send_config_update
    pub fn set_waveform(&mut self, waveform_type: WaveformType, frequency_hz: f64) {
        self.waveform_config.waveform_type = waveform_type;
        self.waveform_config.frequency_hz = frequency_hz.clamp(10.0, 4000.0);
        self.waveform_config.enabled = true;
        self.send_config_update();
    }
    pub fn disable_waveform(&mut self) {
        self.waveform_config.enabled = false;
        self.send_config_update();
    }
    pub fn set_trigger_config(&mut self, config: TriggerConfig) {
        self.trigger_config = config;
        self.send_config_update();
    }
    pub fn set_probe_multiplier(&mut self, multiplier: ProbeMultiplier) {
        self.probe_multiplier = multiplier;
        self.send_config_update();
    }
    pub fn set_time_frame(&mut self, time_frame: f64) {
        self.time_frame = time_frame;
        self.send_config_update();
    }

    async fn get_real_fleascope_data(
        fleascope_arc: &Arc<Mutex<Option<FleaScope>>>,
        probe_multiplier: ProbeMultiplier,
        trigger_config: &TriggerConfig,
        waveform_config: &WaveformConfig,
        time_frame: f64,
    ) -> Option<(Vec<f64>, Vec<DataPoint>)> {
        // Take the FleaScope out of the mutex, use it, then put it back
        let mut fleascope_opt = {
            let mut guard = fleascope_arc.lock().unwrap();
            guard.take()
        };
        let result = if let Some(mut fleascope) = fleascope_opt {
            // Set up waveform generator if enabled
            if waveform_config.enabled {
                let waveform = match waveform_config.waveform_type {
                    WaveformType::Sine => Waveform::Sine,
                    WaveformType::Square => Waveform::Square,
                    WaveformType::Triangle => Waveform::Triangle,
                    WaveformType::Ekg => Waveform::Ekg,
                };
                let freq = waveform_config.frequency_hz.round() as i32;
                let _ = fleascope.set_waveform(waveform, freq);
            }
            // let trigger = Self::convert_trigger_config(trigger_config); // Enable when needed
            let trigger = None;
            let probe_type = match probe_multiplier {
                ProbeMultiplier::X1 => ProbeType::X1,
                ProbeMultiplier::X10 => ProbeType::X10,
            };
            let duration = Duration::from_millis(100);
            // Now call hardware read without holding any lock
            let out = match fleascope.read(probe_type, duration, trigger, None) {
                Ok(lazy_frame) => {
                    match lazy_frame.collect() {
                        Ok(df) => Self::convert_polars_to_data_points(df),
                        Err(_) => None,
                    }
                }
                Err(_) => None,
            };
            fleascope_opt = Some(fleascope); // Put it back
            out
        } else {
            None
        };
        // Put the FleaScope back in the mutex
        let mut guard = fleascope_arc.lock().unwrap();
        *guard = fleascope_opt;
        result
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
            config_sender: None, // Config sender is not cloned
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
        let mut device_clone = device.clone();
        tokio::spawn(async move {
            if device_clone.connect().await.is_ok() {
                device_clone.start_data_generation();
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

#[derive(Debug, Clone)]
pub struct ConfigUpdate {
    pub probe_multiplier: ProbeMultiplier,
    pub trigger_config: TriggerConfig,
    pub waveform_config: WaveformConfig,
    pub time_frame: f64,
}
