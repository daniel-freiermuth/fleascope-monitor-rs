use arc_swap::ArcSwap;
use fleascope_rs::{
    AnalogTrigger, BitState, DigitalTrigger, FleaConnectorError, IdleFleaScope, ProbeType, Trigger,
    Waveform,
};
use std::{sync::Arc, time::Instant};
use tokio::sync::watch;

use crate::{device_worker::FleaWorker, worker_interface::FleaScopeDevice};

// Time frame constants for consistent validation
pub const MIN_TIME_FRAME: f64 = 0.000122; // 122μs
pub const MAX_TIME_FRAME: f64 = 3.49; // 3.49s

#[derive(Default)]
pub struct DeviceManager {
    devices: Vec<FleaScopeDevice>,
}

impl DeviceManager {
    pub fn add_device(&mut self, hostname: String) -> Result<(), FleaConnectorError> {
        let (scope, x1, x10) = IdleFleaScope::connect(Some(&hostname), None, true)?;
        let initial_config = CaptureConfig {
            probe_multiplier: ProbeType::X1,
            trigger_config: TriggerConfig::default(),
            time_frame: 0.1, // Default 2 seconds
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
            update_rate: 0.0,
            connected: true,
            running: true,
        })));

        let mut worker = FleaWorker {
            data: data.clone(),
            config_change_rx: capture_config_rx,
            control_rx: calibration_rx,
            notification_tx,
            x1,
            x10,
            waveform_rx, // Channel for waveform configuration
            running: true,
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
        let _handle = tokio::spawn(async move {
            if let Err(e) = worker.run(scope).await {
                tracing::error!("Worker error: {}", e);
            };
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

    pub fn remove_device(&mut self, index: usize) {
        if index < self.devices.len() {
            let d = self.devices.remove(index);
            d.stop();
        } else {
            panic!("Device index out of bounds");
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub probe_multiplier: ProbeType,
    pub trigger_config: TriggerConfig,
    pub time_frame: f64,
}

pub enum Notification {
    Success(String),
    Error(String),
}

#[derive(Debug)]
pub enum ControlCommand {
    Calibrate0V(ProbeType),
    Calibrate3V(ProbeType),
    StoreCalibration(),
    Pause,
    Resume,
    Step,
    Exit,
}

#[derive(Debug, Clone)]
pub struct DataPoint {
    pub analog_channel: f64,
    pub digital_channels: [bool; 9],
}

#[derive(Debug, Clone)]
pub struct DeviceData {
    pub x_values: Vec<f64>,
    pub data_points: Vec<DataPoint>,
    pub last_update: Instant,
    pub update_rate: f64,
    pub connected: bool,
    pub running: bool,
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

impl From<TriggerConfig> for Trigger {
    fn from(tc: TriggerConfig) -> Self {
        match tc.source {
            TriggerSource::Analog => tc.analog.into(),
            TriggerSource::Digital => tc.digital.into(),
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

pub fn cycle_bitstate(state: BitState) -> BitState {
    match state {
        BitState::DontCare => BitState::High,
        BitState::High => BitState::Low,
        BitState::Low => BitState::DontCare,
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
    pub fn clamp_frequency(&mut self) {
        self.frequency_hz = self.frequency_hz.clamp(10, 4000);
    }
}
