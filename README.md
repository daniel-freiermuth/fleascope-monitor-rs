# FleaScope Live Oscilloscope (Rust)

A modern, real-time oscilloscope GUI built in Rust using the egui framework. This application provides a pleasing and user-friendly interface for visualizing data from multiple FleaScope devices.

## Features

### 🔧 Device Management
- **Easy Connection**: Quick-add buttons for common device hostnames
- **Device Rack**: Visual representation of connected devices
- **Channel Configuration**: Enable/disable individual channels
- **Trigger Settings**: Configurable trigger modes and parameters
- **Waveform Generator**: Built-in signal generator with 4 waveform types
- **Real-time Statistics**: Monitor sample rates and data freshness

### 📊 Visualization Features
- **Analog Plots**: Smooth waveform display with customizable colors
- **Digital Plots**: Stacked digital channel visualization
- **Time Window Control**: Adjustable time window (0.1-10 seconds)
- **Grid Options**: Toggle grid display
- **Auto-scaling**: Automatic or manual plot scaling
- **Color Coding**: Distinct colors for each channel
- Design inspired by analog oscilloscope.

### 🌊 Waveform Generator
- **4 Waveform Types**: Sine, Square, Triangle, and EKG patterns
- **Frequency Range**: 10 Hz to 4 kHz with logarithmic control
- **Quick Presets**: Common frequency values (10Hz, 50Hz, 100Hz, 500Hz, 1kHz, 2kHz)
- **Visual Indicators**: Waveform type icons and frequency display in device rack
- **Enable/Disable**: Easy on/off control per device

## Getting Started

### Prerequisites
- Rust 1.70+ (2021 edition)
- Cargo

### Installation
```bash
```

### Quick Start
1. Launch the application
2. Click the ➕ button in the control panel
3. Use quick-add buttons (scope-001, scope-002, etc.) or enter a custom hostname
4. Configure channels and waveform generator in the device rack
5. Watch real-time data visualization
