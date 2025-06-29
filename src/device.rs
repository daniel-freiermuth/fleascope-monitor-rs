use anyhow::Result;
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
    ChannelsChanged([bool; 10]),
    TimeFrameChanged(f64),
    FullConfigUpdate {
        probe_multiplier: ProbeMultiplier,
        trigger_config: TriggerConfig,
        waveform_config: WaveformConfig,
        time_frame: f64,
        enabled_channels: [bool; 10],
    },
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
    pub sample_rate: f64,
    pub last_update: Instant,
    pub connected: bool,
    pub read_duration: Duration,      // Last read operation duration
    pub update_rate: f64,             // Current update rate (Hz)
    pub consecutive_failures: u32,    // Number of consecutive read failures
}

impl DeviceData {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            x_values: Vec::new(),
            data_points: Vec::new(),
            sample_rate,
            last_update: Instant::now(),
            connected: false,
            read_duration: Duration::from_millis(0),
            update_rate: 0.0,
            consecutive_failures: 0,
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

struct CaptureConfig {
    pub time_frame: f64,              // Time window in seconds
    pub is_paused: Arc<AtomicBool>,   // Pause/continue state (thread-safe)
    pub probe_multiplier: ProbeMultiplier, // Probe selection
    pub trigger_config: TriggerConfig, // Trigger configuration
}

pub struct FleaScopeDevice {
    pub name: String,
    pub data: Arc<ArcSwap<DeviceData>>, // Changed to Arc<ArcSwap> for sharing between threads
    pub enabled_channels: [bool; 10], // 1 analog + 9 digital
    pub time_frame: f64,             // Time window in seconds
    pub is_paused: Arc<AtomicBool>,   // Pause/continue state (thread-safe)
    pub probe_multiplier: ProbeMultiplier, // Probe selection
    pub trigger_config: TriggerConfig, // Trigger configuration
    pub waveform_config: WaveformConfig, // Waveform generator configuration
    fleascope: Arc<Mutex<Option<FleaScope>>>, // Actual FleaScope connection
    config_change_tx: watch::Sender<ConfigChangeSignal>, // Channel for configuration changes
    data_generation_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>, // Handle to data generation task
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
    pub fn new(name: String) -> Self {
        let (config_change_tx, _) = watch::channel(ConfigChangeSignal::Restart);
        
        Self {
            name,
            data: Arc::new(ArcSwap::new(Arc::new(DeviceData::new(1000.0)))), // Initialize with Arc<ArcSwap>
            enabled_channels: [true; 10], // All channels enabled by default
            time_frame: 2.0,             // Default 2 seconds
            is_paused: Arc::new(AtomicBool::new(false)), // Running by default
            probe_multiplier: ProbeMultiplier::X1, // Default x1 probe
            trigger_config: TriggerConfig::default(), // Default trigger config
            waveform_config: WaveformConfig::default(), // Default waveform config
            fleascope: Arc::new(Mutex::new(None)), // No connection initially
            config_change_tx,
            data_generation_handle: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn connect(&self) -> Result<()> {
        // Try to connect to the actual FleaScope device
        // Try to connect to real FleaScope device
        let connection_result = match FleaScope::connect(Some(&self.name), None, true) {
            Ok(scope) => {
                tracing::info!("Successfully connected to real FleaScope device: {}", self.name);
                Some(scope)
            }
            Err(e) => {
                tracing::warn!("Failed to connect to FleaScope device {}: {}, using dummy mode", self.name, e);
                None
            }
        };

        // Store the connection
        {
            let mut fleascope = self.fleascope.lock().unwrap();
            *fleascope = connection_result;
        }

        // Update connection status in shared data
        {
            let current_data = self.data.load(); // Arc<DeviceData>
            let mut new_data: DeviceData = (**current_data).clone(); // Clone to get owned DeviceData
            new_data.connected = self.fleascope.lock().unwrap().is_some();
            self.data.store(Arc::new(new_data));
        }
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
            let current_data = self.data.load();
            let mut new_data = (&**current_data).clone();
            new_data.connected = false;
            self.data.store(Arc::new(new_data));
        }

        tracing::info!("Disconnected from device: {}", self.name);
        Ok(())
    }

    pub fn start_data_generation(&self) {
        // Create a new receiver for configuration changes
        let mut config_change_rx = self.config_change_tx.subscribe();

        // Clone the Arc<ArcSwap> for use in the async task
        let data_arc = Arc::clone(&self.data);
        let is_paused_arc = Arc::clone(&self.is_paused);
        let fleascope_arc = Arc::clone(&self.fleascope);
        
        // Capture values before moving into async closure
        let probe_multiplier = self.probe_multiplier;
        let trigger_config = self.trigger_config.clone();
        let waveform_config = self.waveform_config.clone();
        let time_frame = self.time_frame;
        
        // Start the cancellation-aware data generation loop
        let handle = tokio::spawn(async move {
            tracing::debug!("Starting cancellation-aware data generation loop");
            
            loop {
                tracing::trace!("First loop");
                let mut _time_offset = 0.0;
                let sample_rate = 1000.0;
                let mut adaptive_delay = Duration::from_millis(200); // Start with high frequency
                let points_per_update = (sample_rate / 20.0) as usize; // Target 20 Hz update rate
                let mut consecutive_failures = 0;
                let mut last_successful_read = Instant::now();
                let mut last_rate_update = Instant::now();
                let mut read_count = 0;
                
                // Inner data acquisition loop with cancellation support
                loop {
                    tracing::trace!("Second loop");
                    
                    // Check if device is paused first
                    if is_paused_arc.load(Ordering::Relaxed) {
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

                    // Check if we have a real FleaScope connection (non-blocking)
                    let use_real_data = {
                        if let Ok(fleascope) = fleascope_arc.try_lock() {
                            fleascope.is_some()
                        } else {
                            // If we can't get the lock immediately, assume we have data and try later
                            true
                        }
                    };

                    if use_real_data {
                        let start_time = Instant::now();
                        
                        // Try to get real data from FleaScope with cancellation support
                        tracing::trace!("Getting data");
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        tokio::select! {
                            data_result = Self::get_real_fleascope_data(
                                &fleascope_arc,
                                probe_multiplier,
                                &trigger_config,
                                &waveform_config,
                                time_frame,
                            ) => {
                                match data_result {
                                    Some(real_data) => {
                                        let read_duration = start_time.elapsed();
                                        consecutive_failures = 0;
                                        last_successful_read = Instant::now();
                                        read_count += 1;
                                        
                                        tracing::trace!("Successfully got real data with {} points in {:?}", 
                                                       real_data.1.len(), read_duration);
                                        
                                        // Update data with lock-free operation using ArcSwap
                                        {
                                            let current_data = data_arc.load();
                                            let mut new_data = (&**current_data).clone();
                                            new_data.x_values = real_data.0;
                                            new_data.data_points = real_data.1;
                                            new_data.last_update = Instant::now();
                                            new_data.read_duration = read_duration;
                                            new_data.consecutive_failures = consecutive_failures;
                                            
                                            // Update read rate every second
                                            if last_rate_update.elapsed() >= Duration::from_secs(1) {
                                                new_data.update_rate = read_count as f64 / last_rate_update.elapsed().as_secs_f64();
                                                read_count = 0;
                                                last_rate_update = Instant::now();
                                            }
                                            
                                            data_arc.store(Arc::new(new_data));
                                        }
                                        
                                        _time_offset += points_per_update as f64 / sample_rate;
                                    }
                                    None => {
                                        consecutive_failures += 1;
                                        tracing::debug!("Failed to get real data (failure #{} consecutive)", consecutive_failures);
                                        
                                        // Update failure count in data (lock-free)
                                        {
                                            let current_data = data_arc.load();
                                            let mut new_data = (&**current_data).clone();
                                            new_data.consecutive_failures = consecutive_failures;
                                            data_arc.store(Arc::new(new_data));
                                        }
                                        
                                        // If no successful reads for a long time, reduce frequency significantly
                                        if last_successful_read.elapsed() > Duration::from_secs(5) {
                                            adaptive_delay = Duration::from_millis(500);
                                            tracing::debug!("No successful reads for >5s, reducing frequency");
                                        }
                                    }
                                }
                            },
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
                        }
                    } else {
                        // No real hardware connected - wait and try again with cancellation support
                        tracing::trace!("No real hardware connected, waiting...");
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                            signal = config_change_rx.changed() => {
                                if signal.is_ok() {
                                    tracing::info!("Configuration changed while waiting, will restart");
                                    break;
                                }
                            }
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
        if let Err(_) = self.config_change_tx.send(signal.clone()) {
            tracing::warn!("Failed to send config change signal: {:?}", signal);
        } else {
            tracing::debug!("Sent config change signal: {:?}", signal);
        }
    }

    async fn get_real_fleascope_data(
        fleascope_arc: &Arc<Mutex<Option<FleaScope>>>,
        probe_multiplier: ProbeMultiplier,
        trigger_config: &TriggerConfig,
        waveform_config: &WaveformConfig,
        time_frame: f64,
    ) -> Option<(Vec<f64>, Vec<DataPoint>)> {
        let fleascope_arc_clone = Arc::clone(fleascope_arc);
        let probe_multiplier = probe_multiplier;
        let trigger_config = trigger_config.clone();
        let _waveform_config = waveform_config.clone();
        
        // Run the potentially blocking hardware operation in a separate thread
        // Use a longer timeout (5 seconds) to allow legitimate reads while preventing infinite hangs
        // This is still much more responsive than allowing 20-second hangs
        let read_task = tokio::task::spawn_blocking(move || {
            // Use try_lock to avoid blocking other devices
            tracing::trace!("Pre match");
            let mut fleascope_guard = match fleascope_arc_clone.try_lock() {
                Ok(guard) => guard,
                Err(_) => {
                    tracing::trace!("My FleaScope device busy, skipping this iteration");
                    return None;
                }
            };
            tracing::trace!("Device available");
            
            if let Some(fleascope) = fleascope_guard.as_mut() {
                // Convert trigger configuration properly
                let trigger = Self::convert_trigger_config(&trigger_config);
                if trigger.is_some() {
                    tracing::trace!("Using configured trigger");
                } else {
                    tracing::trace!("Using auto trigger for continuous data flow");
                }

                // Convert probe type
                let probe_type = match probe_multiplier {
                    ProbeMultiplier::X1 => ProbeType::X1,
                    ProbeMultiplier::X10 => ProbeType::X10,
                };

                // Calculate duration based on time frame, but cap it for responsiveness
                let max_duration = Duration::from_millis(50); // Maximum 50ms to keep GUI responsive
                let frame_duration = Duration::from_secs_f64(time_frame);
                let duration = frame_duration.min(max_duration);
                
                tracing::trace!("Reading from FleaScope with duration: {:?}", duration);
                match fleascope.read(probe_type, duration, trigger, None) {
                    Ok(lazy_frame) => {
                        // Release the lock as early as possible
                        drop(fleascope_guard);
                        
                        // Collect the data (this can take time but doesn't block other devices)
                        match lazy_frame.collect() {
                            Ok(df) => {
                                tracing::trace!("Successfully collected DataFrame with {} rows", df.height());
                                return Self::convert_polars_to_data_points(df);
                            }
                            Err(e) => {
                                tracing::debug!("Failed to collect data frame: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Failed to read from FleaScope: {}", e);
                    }
                }
            }
            None
        });
        
        // Apply a reasonable timeout to prevent indefinite hangs while allowing legitimate long reads
        // 5 seconds should be enough for most triggers while keeping GUI responsive
        let read_timeout = Duration::from_secs(5);
        
        match tokio::time::timeout(read_timeout, read_task).await {
            Ok(Ok(result)) => {
                tracing::debug!("Data arrived");
                tokio::time::sleep(Duration::from_millis(500)).await;
                result
            },
            Ok(Err(e)) => {
                tracing::debug!("Hardware read task failed: {}", e);
                None
            }
            Err(_) => {
                tracing::debug!("Hardware read timed out after {:?}, likely waiting for trigger", read_timeout);
                None
            }
        }
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
        self.data.load().connected || has_fleascope
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

    pub fn get_performance_status(&self) -> String {
        let data = self.data.load();
        let read_time_ms = data.read_duration.as_millis();
        let update_rate = data.update_rate;
        let failures = data.consecutive_failures;
        
        if failures > 0 {
            format!("⚠️ {:.1}Hz, {}ms, {} failures", update_rate, read_time_ms, failures)
        } else if update_rate > 10.0 {
            format!("✅ {:.1}Hz, {}ms", update_rate, read_time_ms)
        } else {
            format!("🐌 {:.1}Hz, {}ms", update_rate, read_time_ms)
        }
    }

    /// Update probe multiplier and restart data generation with new settings
    pub fn set_probe_multiplier(&mut self, probe_multiplier: ProbeMultiplier) {
        if self.probe_multiplier != probe_multiplier {
            self.probe_multiplier = probe_multiplier;
            self.signal_config_change(ConfigChangeSignal::FullConfigUpdate {
                probe_multiplier: self.probe_multiplier,
                trigger_config: self.trigger_config.clone(),
                waveform_config: self.waveform_config.clone(),
                time_frame: self.time_frame,
                enabled_channels: self.enabled_channels,
            });
            // The data generation task will restart itself when it receives the signal
        }
    }
    
    /// Update trigger configuration and restart data generation with new settings
    pub fn set_trigger_config(&mut self, trigger_config: TriggerConfig) {
        // Simple comparison - in a real implementation, you might want a more sophisticated comparison
        self.trigger_config = trigger_config;
        self.signal_config_change(ConfigChangeSignal::FullConfigUpdate {
            probe_multiplier: self.probe_multiplier,
            trigger_config: self.trigger_config.clone(),
            waveform_config: self.waveform_config.clone(),
            time_frame: self.time_frame,
            enabled_channels: self.enabled_channels,
        });
        // The data generation task will restart itself when it receives the signal
    }
    
    /// Update waveform configuration and restart data generation with new settings
    pub fn set_waveform_config(&mut self, waveform_config: WaveformConfig) {
        self.waveform_config = waveform_config;
        self.signal_config_change(ConfigChangeSignal::FullConfigUpdate {
            probe_multiplier: self.probe_multiplier,
            trigger_config: self.trigger_config.clone(),
            waveform_config: self.waveform_config.clone(),
            time_frame: self.time_frame,
            enabled_channels: self.enabled_channels,
        });
        // The data generation task will restart itself when it receives the signal
    }
    
    /// Update enabled channels and restart data generation with new settings
    pub fn set_enabled_channels(&mut self, enabled_channels: [bool; 10]) {
        if self.enabled_channels != enabled_channels {
            self.enabled_channels = enabled_channels;
            self.signal_config_change(ConfigChangeSignal::FullConfigUpdate {
                probe_multiplier: self.probe_multiplier,
                trigger_config: self.trigger_config.clone(),
                waveform_config: self.waveform_config.clone(),
                time_frame: self.time_frame,
                enabled_channels: self.enabled_channels,
            });
            // The data generation task will restart itself when it receives the signal
        }
    }
    
    /// Update time frame and restart data generation with new settings
    pub fn set_time_frame(&mut self, time_frame: f64) {
        if (self.time_frame - time_frame).abs() > f64::EPSILON {
            self.time_frame = time_frame;
            self.signal_config_change(ConfigChangeSignal::FullConfigUpdate {
                probe_multiplier: self.probe_multiplier,
                trigger_config: self.trigger_config.clone(),
                waveform_config: self.waveform_config.clone(),
                time_frame: self.time_frame,
                enabled_channels: self.enabled_channels,
            });
            // The data generation task will restart itself when it receives the signal
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
            is_paused: Arc::clone(&self.is_paused),
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
        let device = FleaScopeDevice::new(hostname);

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
