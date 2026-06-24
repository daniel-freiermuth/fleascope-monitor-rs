use anyhow::Result;
use arc_swap::ArcSwap;
use fleascope_rs::{ProbeType, Waveform};
use std::sync::Arc;
use tokio::sync::watch::{self, Sender};

use crate::device::{
    CaptureConfig, CaptureMode, ControlCommand, DeviceData, Notification, TriggerConfig,
    WaveformConfig, MAX_TIME_FRAME, MIN_TIME_FRAME,
};

#[derive(Clone)]
pub struct TriggeredCaptureConfig {
    pub time_frame: f64,
    pub trigger_config: TriggerConfig,
}
#[derive(Clone)]
pub struct ContinuousCaptureConfig {
    pub buffer_time: f64,
}

#[derive(Copy, Clone, PartialEq)]
pub enum CaptureModeFlat {
    Triggered,
    Continuous,
    DigitalStream,
}

pub struct FleaScopeDevice {
    pub name: String,
    pub data: Arc<ArcSwap<DeviceData>>, // Changed to Arc<ArcSwap> for sharing between threads
    pub enabled_channels: [bool; 10],   // 1 analog + 9 digital
    probe_multiplier: ProbeType,        // Probe selection
    waveform_config: WaveformConfig,    // Waveform generator configuration
    config_change_tx: watch::Sender<CaptureConfig>, // Channel for configuration changes
    control_signal_tx: tokio::sync::mpsc::Sender<ControlCommand>, // Channel for calibration commands
    pub notification_rx: tokio::sync::mpsc::Receiver<Notification>, // Channel for calibration results
    waveform_tx: Sender<WaveformConfig>, // Channel for waveform configuration
    pub batch_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<f64>>, // Channel for continuous batches
    pub digital_batch_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u16>>, // Channel for digital-stream batches
    triggered_config: TriggeredCaptureConfig,
    continuous_config: ContinuousCaptureConfig,
    capture_mode: CaptureModeFlat,
    digital_stream_channel_mask: u16,
    digital_stream_sample_interval_us: u32,
    digital_stream_buffer_window_s: f64,
    digital_stream_memory_bytes: usize,
    /// Whether the plot is following new data (auto-scroll). Unfollowed on user zoom/pan.
    pub digital_stream_follow_mode: bool,
    /// Live sample rate measured from batch arrival timing; updated by the plot area.
    pub measured_digital_sample_rate_hz: f64,
    /// Buffer bytes currently used; updated by the plot area.
    pub digital_stream_bytes_used: usize,
    pub wrap: bool,
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
        batch_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<f64>>,
        digital_batch_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u16>>,
    ) -> Self {
        let mut triggered_config = TriggeredCaptureConfig {
            time_frame: 0.1,
            trigger_config: TriggerConfig::default(),
        };
        let continuous_config = ContinuousCaptureConfig { buffer_time: 1.0 };
        let mode = match initial_config.mode {
            CaptureMode::Triggered {
                time_frame,
                trigger_config,
            } => {
                triggered_config.time_frame = time_frame;
                triggered_config.trigger_config = trigger_config;
                CaptureModeFlat::Triggered
            }
            CaptureMode::Continuous {} => CaptureModeFlat::Continuous,
            CaptureMode::DigitalStream { .. } => CaptureModeFlat::DigitalStream,
        };
        Self {
            name,
            data,
            enabled_channels: [true; 10], // All channels enabled by default
            triggered_config,
            continuous_config,
            capture_mode: mode,
            digital_stream_channel_mask: 0x1ff, // all channels by default
            digital_stream_sample_interval_us: 10, // 100 ksps default
            digital_stream_buffer_window_s: 1.0,
            digital_stream_memory_bytes: 256 * 1024 * 1024, // 256 MB default
            digital_stream_follow_mode: true,
            measured_digital_sample_rate_hz: 0.0,
            digital_stream_bytes_used: 0,
            probe_multiplier: initial_config.probe_multiplier,
            waveform_config: initial_waveform,
            config_change_tx,
            control_signal_tx: calibration_tx,
            notification_rx,
            waveform_tx,
            batch_rx,
            digital_batch_rx,
            wrap: true,
        }
    }

    /// Signal that configuration has changed and data generation should restart
    fn signal_config_change(&self) {
        let cm = match self.capture_mode {
            CaptureModeFlat::Triggered => CaptureMode::Triggered {
                trigger_config: self.triggered_config.trigger_config.clone(),
                time_frame: self.triggered_config.time_frame,
            },
            CaptureModeFlat::Continuous => CaptureMode::Continuous {},
            CaptureModeFlat::DigitalStream => CaptureMode::DigitalStream {
                channel_mask: self.digital_stream_channel_mask,
                us_per_sample: self.digital_stream_sample_interval_us,
            },
        };
        self.config_change_tx
            .send(CaptureConfig {
                probe_multiplier: self.probe_multiplier,
                mode: cm,
            })
            .expect("Failed to send config change signal");
    }

    pub fn pause(&mut self) {
        self.control_signal_tx
            .try_send(ControlCommand::Pause)
            .expect("Failed to send resume command");
    }

    pub fn stop(self) {
        self.control_signal_tx
            .try_send(ControlCommand::Exit)
            .expect("Failed to send exit command");
    }

    pub fn resume(&mut self) {
        self.control_signal_tx
            .try_send(ControlCommand::Resume)
            .expect("Failed to send resume command");
    }

    pub fn set_waveform(&mut self, waveform_type: Waveform, frequency_hz: i32) {
        self.waveform_config.waveform_type = waveform_type;
        self.waveform_config.frequency_hz = frequency_hz.clamp(10, 4000);
        self.waveform_config.enabled = true;
        self.waveform_tx
            .send(self.waveform_config.clone())
            .expect("Failed to send waveform configuration");
    }

    pub fn set_probe_multiplier(&mut self, multiplier: ProbeType) {
        self.probe_multiplier = multiplier;
        self.signal_config_change();
    }

    pub fn set_trigger_config(&mut self, trigger_config: TriggerConfig) {
        tracing::debug!("Setting trigger config: {:?}", trigger_config);
        self.triggered_config.trigger_config = trigger_config;
        self.signal_config_change();
    }

    pub fn get_capture_mode(&self) -> CaptureModeFlat {
        self.capture_mode
    }

    pub fn get_continuous_config(&self) -> ContinuousCaptureConfig {
        self.continuous_config.clone()
    }

    pub fn get_triggered_config(&self) -> TriggeredCaptureConfig {
        self.triggered_config.clone()
    }

    pub fn get_waveform_config(&self) -> WaveformConfig {
        self.waveform_config.clone()
    }

    pub fn get_probe_multiplier(&self) -> ProbeType {
        self.probe_multiplier
    }

    pub fn get_mut_trigger_time_handle(&mut self) -> &mut f64 {
        &mut self.triggered_config.time_frame
    }

    pub fn get_mut_buffer_time_handle(&mut self) -> &mut f64 {
        &mut self.continuous_config.buffer_time
    }

    pub fn set_capture_mode(&mut self, mode: CaptureModeFlat) {
        self.capture_mode = mode;
        self.signal_config_change();
    }

    pub fn set_enabled_channels(&mut self, enabled: [bool; 10]) {
        self.enabled_channels = enabled;
    }

    pub fn set_time_frame(&mut self, time_frame: f64) {
        // Clamp time frame to valid range: 122μs to 3.49s
        self.triggered_config.time_frame = time_frame.clamp(MIN_TIME_FRAME, MAX_TIME_FRAME);
        self.signal_config_change();
    }

    pub fn get_digital_stream_channel_mask(&self) -> u16 {
        self.digital_stream_channel_mask
    }

    pub fn set_digital_stream_channel_mask(&mut self, mask: u16) {
        self.digital_stream_channel_mask = mask & 0x1ff;
        if self.capture_mode == CaptureModeFlat::DigitalStream {
            self.signal_config_change();
        }
    }

    pub fn get_digital_stream_sample_interval_us(&self) -> u32 {
        self.digital_stream_sample_interval_us
    }

    pub fn set_digital_stream_sample_interval_us(&mut self, us: u32) {
        self.digital_stream_sample_interval_us = us;
        if self.capture_mode == CaptureModeFlat::DigitalStream {
            self.signal_config_change();
        }
    }

    pub fn get_digital_stream_buffer_window_s(&self) -> f64 {
        self.digital_stream_buffer_window_s
    }

    pub fn get_mut_digital_stream_buffer_window_s(&mut self) -> &mut f64 {
        &mut self.digital_stream_buffer_window_s
    }

    pub fn get_digital_stream_memory_bytes(&self) -> usize {
        self.digital_stream_memory_bytes
    }

    pub fn set_digital_stream_memory_bytes(&mut self, bytes: usize) {
        self.digital_stream_memory_bytes = bytes;
    }

    pub fn get_digital_stream_memory_mb(&self) -> f64 {
        self.digital_stream_memory_bytes as f64 / (1024.0 * 1024.0)
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
