use std::sync::Arc;

use arc_swap::ArcSwap;
use fleascope_rs::{ProbeType, Waveform};
use tokio::sync::watch::{self, Sender};

use crate::device::{CaptureConfig, ControlCommand, DeviceData, Notification, TriggerConfig, WaveformConfig, MAX_TIME_FRAME, MIN_TIME_FRAME};


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
