# FleaScope Live Oscilloscope - Technical Documentation

## Overview
This Rust-based oscilloscope GUI provides a modern, high-performance interface for visualizing real-time data from multiple FleaScope devices. Built with the egui framework, it offers excellent performance and a pleasing user experience.

## Architecture

### Core Components

#### 1. Main Application (`main.rs`)
- **FleaScopeApp**: Main application struct managing the overall UI
- **Event Loop**: 60+ FPS rendering with automatic repaint requests
- **Layout Management**: Split-pane layout with plots on left, controls on right
- **Menu System**: Standard menu bar with File, View, and Help menus

#### 2. Device Management (`device.rs`)
- **FleaScopeDevice**: Represents a single oscilloscope device
- **DeviceManager**: Manages multiple devices and their lifecycle
- **DataPoint**: Single measurement with analog and digital channels
- **DeviceData**: Time-series data buffer for each device

#### 3. Plot Visualization (`plot_area.rs`)
- **PlotArea**: Main plotting component for real-time visualization
- **Analog Plots**: Smooth waveform rendering for 12-bit analog signals
- **Digital Plots**: Stacked binary channel visualization
- **Interactive Controls**: Zoom, pan, time window adjustment

#### 4. Control Interface (`control_panel.rs`)
- **ControlPanel**: Device configuration and management UI
- **Device Rack**: Visual representation of connected devices
- **Channel Configuration**: Enable/disable individual channels
- **Trigger Settings**: Configurable trigger parameters

### Data Flow Architecture

```
┌─────────────────┐    ┌──────────────────┐    ┌─────────────────┐
│   Device        │    │   Device         │    │   Plot          │
│   Simulation    │───▶│   Manager        │───▶│   Rendering     │
│   (30Hz)        │    │   (Thread-Safe)  │    │   (60+ FPS)     │
└─────────────────┘    └──────────────────┘    └─────────────────┘
         │                       │                       │
         ▼                       ▼                       ▼
   Async Tasks           Arc<Mutex<Data>>          Interactive UI
   Generate Data         Shared State             Real-time Updates
```

### Thread Safety
- **Arc<Mutex<DeviceData>>**: Thread-safe data sharing between simulation and UI
- **Tokio Runtime**: Async device management and data generation
- **Non-blocking UI**: UI remains responsive during data operations

## Performance Characteristics

### Real-time Capabilities
- **Update Rate**: 30Hz per device data generation
- **Rendering**: 60+ FPS smooth UI updates
- **Buffer Size**: 2000 samples per update
- **Channels**: 1 analog (12-bit) + 9 digital (binary)

### Memory Management
- **Efficient Data Structures**: Minimal allocation during runtime
- **Circular Buffers**: Time-windowed data display
- **Thread-safe Sharing**: Arc/Mutex for data synchronization

### CPU Optimization
- **Immediate Mode GUI**: Efficient rendering with egui
- **Filtered Data**: Only render data within time window
- **Async Operations**: Non-blocking device simulation

## Signal Generation (Demo Mode)

### Analog Channel Simulation
```rust
let analog_signal = (0.5 + 0.3 * (2π * 10.0 * t).sin()
    + 0.1 * (2π * 50.0 * t).sin()
    + 0.05 * random()).clamp(0.0, 1.0);
```
- **Base Frequency**: 10 Hz sine wave
- **Harmonic**: 50 Hz secondary frequency
- **Noise**: 5% random noise
- **Range**: 0.0 to 1.0 (representing 0-4095 12-bit ADC)

### Digital Channel Simulation
```rust
for ch in 0..9 {
    let freq = 1.0 + ch as f64 * 0.5;
    digital_channels[ch] = ((2π * freq * t).sin() > 0.0)
        && (random() > 0.1); // 10% dropout simulation
}
```
- **Frequency Range**: 1.0 to 5.5 Hz across channels
- **Dropout Simulation**: 10% random signal loss
- **Phase Relationships**: Different frequencies create interesting patterns

## User Interface Design

### Layout Philosophy
- **Split Interface**: 65% plots, 35% controls
- **Responsive Design**: Adapts to window resizing
- **Professional Appearance**: Clean, modern aesthetic
- **Intuitive Controls**: Familiar oscilloscope-style interface

### Color Scheme
- **Analog Signals**: Bright yellow (classic oscilloscope)
- **Digital Channels**: Distinct colors per channel
- **Status Indicators**: Green (connected), Red (disconnected)
- **Background**: Dark theme for reduced eye strain

### Interactive Features
- **Plot Zoom/Pan**: Mouse wheel and drag operations
- **Time Window**: 0.1 to 10 seconds adjustable
- **Channel Toggle**: Individual channel enable/disable
- **Device Management**: Add/remove devices dynamically

## Configuration Options

### Plot Settings
- **Grid Display**: Toggle coordinate grid
- **Auto Scaling**: Automatic or manual axis scaling
- **Plot Height**: Adjustable plot area size
- **Time Window**: Configurable data display duration

### Device Settings
- **Connection Management**: Add/remove devices
- **Channel Configuration**: Enable/disable specific channels
- **Trigger Settings**: Mode, level, and slope configuration
- **Statistics Display**: Real-time performance metrics

## Development Guidelines

### Code Organization
- **Modular Design**: Separate files for major components
- **Clear Interfaces**: Well-defined public APIs
- **Error Handling**: Comprehensive error propagation
- **Documentation**: Inline comments and external docs

### Performance Considerations
- **Minimize Allocations**: Use efficient data structures
- **Thread Safety**: Proper synchronization primitives
- **UI Responsiveness**: Non-blocking operations
- **Memory Usage**: Reasonable buffer sizes

### Testing Strategy
- **Unit Tests**: Test individual components
- **Integration Tests**: Test component interactions
- **Performance Tests**: Measure rendering performance
- **Visual Tests**: Verify UI appearance

## Future Enhancements

### Real Device Integration
- **Serial Communication**: USB/Serial device protocols
- **Network Support**: TCP/UDP device communication
- **Device Discovery**: Automatic device detection
- **Configuration Persistence**: Save/load device settings

### Advanced Features
- **Measurement Cursors**: Voltage/time measurements
- **Math Operations**: Signal processing functions
- **Data Export**: CSV/JSON data export
- **Waveform Recording**: Continuous data logging

### Performance Improvements
- **GPU Acceleration**: Hardware-accelerated rendering
- **Multi-threading**: Parallel data processing
- **Memory Optimization**: Reduced allocation overhead
- **Compression**: Efficient data storage

## Troubleshooting

### Common Issues
- **Build Errors**: Ensure Rust toolchain is up to date
- **Performance Issues**: Check system resources
- **Display Problems**: Update graphics drivers
- **Connection Issues**: Verify device connectivity

### Debug Information
- **Logging**: Tracing framework for debug output
- **Performance Metrics**: FPS and update rate monitoring
- **Memory Usage**: Runtime memory consumption
- **Thread Status**: Async task monitoring

## API Reference

### Device Management
```rust
pub struct DeviceManager {
    devices: Vec<FleaScopeDevice>,
}

impl DeviceManager {
    pub fn add_device(&mut self, hostname: String) -> Result<()>
    pub fn get_devices(&self) -> &[FleaScopeDevice]
    pub fn remove_device(&mut self, index: usize) -> Result<()>
}
```

### Data Structures
```rust
pub struct DataPoint {
    pub timestamp: f64,
    pub analog_channel: f64,
    pub digital_channels: [bool; 9],
}

pub struct DeviceData {
    pub x_values: Vec<f64>,
    pub data_points: Vec<DataPoint>,
    pub sample_rate: f64,
    pub last_update: Instant,
}
```

### Plot Configuration
```rust
pub struct PlotArea {
    plot_height: f32,
    colors: Vec<Color32>,
    show_grid: bool,
    auto_scale: bool,
    time_window: f64,
}
```

This documentation provides a comprehensive overview of the oscilloscope application's architecture, performance characteristics, and usage guidelines.
