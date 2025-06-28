use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;

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

#[derive(Debug)]
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
        }
    }

    pub async fn connect(&self) -> Result<()> {
        // Simulate connection delay
        sleep(Duration::from_millis(500)).await;

        // Update connection status in shared data
        {
            let mut data = self.data.lock().unwrap();
            data.connected = true;
        }

        tracing::info!("Connected to device: {}", self.name);
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
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

        tokio::spawn(async move {
            let mut time_offset = 0.0;
            let sample_rate = 1000.0;
            let update_rate = 30.0; // 30 Hz
            let points_per_update = (sample_rate / update_rate) as usize;

            loop {
                sleep(Duration::from_millis((1000.0 / update_rate) as u64)).await;

                // Check if device is paused
                if is_paused_arc.load(Ordering::Relaxed) {
                    continue; // Skip data generation if paused
                }

                let mut data = data_arc.lock().unwrap();

                // Generate 2000 points of dummy data
                let mut new_x_values = Vec::with_capacity(2000);
                let mut new_data_points = Vec::with_capacity(2000);

                for i in 0..2000 {
                    let t = time_offset + (i as f64) / sample_rate;
                    new_x_values.push(t);

                    // Generate analog signal (12-bit, 0-4095 range, normalized to 0-1)
                    let analog_signal = (0.5
                        + 0.3 * (2.0 * std::f64::consts::PI * 10.0 * t).sin()
                        + 0.1 * (2.0 * std::f64::consts::PI * 50.0 * t).sin()
                        + 0.05 * rand::random::<f64>())
                    .clamp(0.0, 1.0);

                    // Generate digital signals
                    let mut digital_channels = [false; 9];
                    for ch in 0..9 {
                        let freq = 1.0 + ch as f64 * 0.5;
                        digital_channels[ch] = ((2.0 * std::f64::consts::PI * freq * t).sin()
                            > 0.0)
                            && (rand::random::<f64>() > 0.1); // Add some noise
                    }

                    new_data_points.push(DataPoint {
                        timestamp: t,
                        analog_channel: analog_signal,
                        digital_channels,
                    });
                }

                data.x_values = new_x_values;
                data.data_points = new_data_points;
                data.last_update = Instant::now();
                drop(data);

                time_offset += points_per_update as f64 / sample_rate;
            }
        });
    }

    pub fn is_connected(&self) -> bool {
        self.data.lock().unwrap().connected
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
