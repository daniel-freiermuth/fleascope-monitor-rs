# FleaScope Live Oscilloscope Makefile
.PHONY: build run clean release help dev-check watch

# Default target
all: build

# Build the application in debug mode
build:
	cargo build

# Run the application
run:
	cargo run

# Build and run in one command
dev: build run

# Development workflow: format, lint, build, then run
dev-check: fmt lint build run

# Watch for changes and rebuild (requires cargo-watch: cargo install cargo-watch)
watch:
	cargo watch -x build

# Build and run in one command
dev: build run

# Build for release (optimized)
release:
	cargo build --release

# Run the release version
run-release: release
	./target/release/fleascope-live-rs

# Clean build artifacts
clean:
	cargo clean

# Check code without building
check:
	cargo check

# Run tests
test:
	cargo test

# Format code
fmt:
	cargo fmt

# Run clippy linter
lint:
	cargo clippy

# Install dependencies
deps:
	cargo fetch

# Show help
help:
	@echo "Available targets:"
	@echo "  build        - Build the application in debug mode"
	@echo "  run          - Run the application"
	@echo "  dev          - Build and run in development mode"
	@echo "  dev-check    - Format, lint, build, and run (full dev workflow)"
	@echo "  watch        - Watch for changes and rebuild (requires cargo-watch)"
	@echo "  release      - Build optimized release version"
	@echo "  run-release  - Run the release version"
	@echo "  clean        - Clean build artifacts"
	@echo "  check        - Check code without building"
	@echo "  test         - Run tests"
	@echo "  fmt          - Format code"
	@echo "  lint         - Run clippy linter"
	@echo "  deps         - Install dependencies"
	@echo "  help         - Show this help message"
