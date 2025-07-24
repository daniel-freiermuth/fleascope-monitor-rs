# FleaScope Live Oscilloscope - Technical Documentation

## Overview
This Rust-based oscilloscope GUI provides a high-performance interface for visualizing real-time data from multiple FleaScope devices.

## Design Perks

- **Double-Buffered Data**: Data is passed via ArcSwap. This enables lock-free hot loops in the data collector (max 20Hz) and the GUI (60Hz).
- **Command passing via tokio::watch::{watch,mpsc}**: Async message passing for lock-free hot loops.
- Hardware device is owned by dedicated worker thread.
- Tokio for async handling.
- Post-processing in async for tightest hot loop.
- Type-guarded device state machine (typestates) that allows fearless cancellation of running readings.
