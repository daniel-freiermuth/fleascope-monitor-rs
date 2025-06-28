# FleaScope Live Oscilloscope (Rust)

A modern, real-time oscilloscope GUI built in Rust using the egui framework. This application provides a pleasing and user-friendly interface for visualizing data from multiple FleaScope devices.

## Features

### 🎯 Core Functionality
- **Real-time Visualization**: Display live data from multiple FleaScope devices at 30Hz update rate
- **Multi-device Support**: Connect and manage multiple devices simultaneously
- **Dual Channel Types**: 
  - 1 Analog channel (12-bit, 0-4095 range)
  - 9 Digital channels (binary)
- **High Performance**: Handle 2000 data points per update per device

### 🎨 User Interface
- **Modern GUI**: Clean, responsive interface built with egui
- **Split Layout**: 
  - Left side: Real-time oscilloscope plots
  - Right side: Device control panel (rack-style)
- **Interactive Plots**: Zoom, pan, and configure display options
- **Status Indicators**: Live connection status and data update monitoring

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
# Clone or navigate to the project directory
cd fleascope-live-rs

# Install dependencies
cargo build

# Run the application
cargo run
```

### Quick Start
1. Launch the application
2. Click the ➕ button in the control panel
3. Use quick-add buttons (scope-001, scope-002, etc.) or enter a custom hostname
4. Configure channels and waveform generator in the device rack
5. Watch real-time data visualization

## Architecture

### Components
- **Device Manager**: Handles device connections and data management
- **Plot Area**: Renders real-time oscilloscope displays
- **Control Panel**: Provides device configuration and management
- **Data Generation**: Simulates realistic FleaScope data patterns

### Data Flow
1. Devices generate 2000-point datasets at 30Hz
2. Data includes 1 analog channel (sine waves + noise) and 9 digital channels
3. Real-time visualization updates automatically
4. Configurable time windows and channel selection

### Thread Safety
- Async/await for device operations
- Arc<Mutex> for safe data sharing
- Non-blocking UI updates

## Configuration

### Dummy Data Patterns
- **Analog Channel**: Multi-frequency sine waves with noise
- **Digital Channels**: Various frequency square waves with random dropout
- **Sample Rate**: 1000 Hz simulation
- **Update Rate**: 30 Hz (configurable)

### Customization
- Modify `src/device.rs` for different data patterns
- Adjust colors in `src/plot_area.rs`
- Customize UI layout in `src/main.rs`

## Development

### Project Structure
```
src/
├── main.rs           # Application entry point and main window
├── device.rs         # Device management and data generation
├── plot_area.rs      # Oscilloscope plotting functionality
└── control_panel.rs  # Device control and configuration UI
```

### Building for Release
```bash
cargo build --release
```

### Adding Features
- Extend `FleaScopeDevice` for additional device capabilities
- Add new plot types in `PlotArea`
- Enhance control panel with more device settings

## Dependencies

- **eframe/egui**: Modern immediate-mode GUI framework
- **egui_plot**: Real-time plotting capabilities
- **tokio**: Async runtime for device operations
- **serde**: Data serialization (future features)
- **tracing**: Logging and diagnostics

## Future Enhancements

- [ ] Real device communication protocols
- [ ] Data export functionality
- [ ] Advanced trigger configurations
- [ ] Measurement cursors and calculations
- [ ] Waveform math operations
- [ ] Settings persistence
- [ ] Plugin architecture

## Performance

Optimized for:
- Real-time data rates up to 30Hz per device
- Multiple simultaneous devices
- Smooth 60+ FPS UI rendering
- Low CPU usage through efficient data structures

## License

This project is part of the FleaScope ecosystem. See the main project license for details.
