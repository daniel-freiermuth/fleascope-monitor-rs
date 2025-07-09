use anyhow::{Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch::{self};
use fleascope_rs::{AnalogTrigger, BitState, DigitalTrigger, IdleFleaScope, ProbeType, Trigger, Waveform};
use arc_swap::ArcSwap;

use crate::device_worker::FleaWorker;
use crate::worker_interface::FleaScopeDevice;

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
