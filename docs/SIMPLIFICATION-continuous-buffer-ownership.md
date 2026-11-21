# Simplification plan: move the continuous-mode buffer onto `FleaScopeDevice`

Status: RFC. No production code changed. A human must answer the question at the end.

## What exists

`PlotArea` (src/plot_area.rs) keeps a second per-device collection next to
`DeviceManager::devices`:

- src/plot_area.rs:191 — field `continuous_buffers: HashMap<String, ContinuousBuffer>`, keyed by `device.name`
- src/plot_area.rs:212 — initialised empty in `PlotArea::default`
- src/plot_area.rs:301-308 — `get_analog_data` creates the entry lazily with
  `entry(device_name.clone()).or_insert_with(|| ContinuousBuffer::new(51_436))`
- src/plot_area.rs:310-317 — drains `device.batch_rx` into it, then `cleanup_old_batches`
- src/plot_area.rs:321-333 — looks the same key up again with `get(device_name)`; the
  `else { (vec![], vec![]) }` branch at :331-333 cannot be taken, the entry was inserted at :302-308

Nothing removes entries. `DeviceManager::remove_device` (src/device.rs:105-116) drops the
`FleaScopeDevice` (and with it `batch_rx`) but does not know the map exists.

## Why it was built

Added in 95943b6 "feat: Support stream command!!" together with `batch_rx` on
`FleaScopeDevice`. `PlotArea` was where the drawing code lived, so the buffer was attached
there by name. `git log -S continuous_buffers` lists only 95943b6; nothing has touched it since.

## Payers today (re-verified)

Search over the whole worktree (sources, tests, Cargo.toml, CI, README.md,
TECHNICAL_DOCUMENTATION.md) for `continuous_buffers` / `ContinuousBuffer` / `batch_rx`:

- `continuous_buffers`: only src/plot_area.rs:191, :212, :303, :324 — all inside `PlotArea`,
  used only by `PlotArea::get_analog_data`, which already receives `&mut FleaScopeDevice`.
- `ContinuousBuffer`: defined src/plot_area.rs:13-185; constructed at :307 and in the test
  `int32_overflow_in_time_bin_after_24_min` (:455), which builds a buffer directly and does not
  touch the map.
- `batch_rx`: created src/device.rs:52, passed into `FleaScopeDevice::new`
  (src/worker_interface.rs:57, field :39). Its only reader is src/plot_area.rs:310.
- No serialization, persisted state, string-keyed lookup or docs refer to the map.
- The key's uniqueness is enforced only by the add-device UI: src/control_panel.rs:240-245 skips
  hostnames already in `device_manager.get_devices()`. `add_device` has no other caller
  (src/control_panel.rs:247).

## Simpler design

`FleaScopeDevice` owns `continuous_buffer: ContinuousBuffer` next to `batch_rx` (its only
feeder), created in `FleaScopeDevice::new` with `ContinuousBuffer::new(51_436)`.
`get_analog_data` becomes:

```rust
CaptureModeFlat::Continuous => {
    while let Ok(batch) = device.batch_rx.try_recv() {
        device.continuous_buffer.add_batch(batch);
    }
    let buffer_time = device.get_continuous_config().buffer_time;
    device.continuous_buffer.cleanup_old_batches(buffer_time);
    device.continuous_buffer.get_data_in_window(buffer_time, device.wrap, self.width)
}
```

The calls stay in `plot_area.rs`, so the private `cleanup_old_batches` needs no visibility
change; `ContinuousBuffer` stays where it is and `worker_interface.rs` imports it. No new type,
helper or dependency; polars is already a crate dependency.

## What would be deleted (production code only, estimate)

- `continuous_buffers` field + init: −2
- `entry(...).or_insert_with(...)` block: −7
- second `get(device_name)` lookup and its unreachable `else` branch: −5
- two `let device_name = &device.name;` bindings: −2

Added: field, initialiser, import (+3). Net ≈ −13 production lines.

Concepts removed: one name-keyed collection parallel to `DeviceManager::devices`; one
unreachable branch; one lifetime (buffer) not synchronised with the device it belongs to.
Tests: none deleted; `int32_overflow_in_time_bin_after_24_min` is unaffected.

## What changes, and who sees it

All differences are on the remove/re-add path. A device that stays connected behaves the same,
including Triggered → Continuous → Triggered → Continuous toggles (the buffer survives the
toggle in both designs, since in the new one it lives as long as the device).

1. **Removing a device frees its buffer.** Today the `DataFrame` (up to `buffer_time` ×
   51 436 samples) stays in the map for the lifetime of the app.
2. **Re-adding the same hostname starts empty.** Today, after remove + re-add (the device comes
   back in Triggered mode, src/device.rs:34-40), the first switch to Continuous shows the
   previous session's trace (cleanup keeps the last `buffer_time` worth) until new samples push
   it out. `last_t` also continues from the old session. With `wrap` on (the default,
   src/worker_interface.rs:90) the x-axis is `time % buffer_time` (src/plot_area.rs:154-156), so
   only the stale trace is visible; with `wrap` off the x-axis is absolute `last_t`, so it also
   continues from the old session's time instead of restarting at 0. A user sees this.
3. **Allocation moves** from the first continuous-mode frame to device connect (one
   one-row `DataFrame`). Not visible.

## Question for the human

Is a device that is removed and re-added under the same hostname meant to resume the previous
session's continuous-mode trace and time axis (current behavior), or to start fresh (new
behavior)? If "start fresh" — i.e. the current behavior is an accident of the map never being
pruned — the change above can be applied as written.
