use crate::device_bindings::wandr::device::sensors::{Host, Kind, SensorInfo, SensorSample};
use wandr_sensors_client::{SensorEvent, SensorDesc};

// ── Sensor WIT impl ──────────────────────────────────────────────────────────
//
// The binder mechanism (ISensorManager event-queue path) was extracted into the
// shared `wandr-sensors-client` crate (task 77) so the arbiter's sensor-driver
// thread and this guest-facing WIT impl share one HAL owner. This file is now a
// thin adapter: it maps the crate's neutral `SensorDesc`/`SensorEvent` structs to
// the `skiko-gfx` `sensors` WIT types and keeps the task-43 device-orientation
// helpers. All cross-platform gating lives in `wandr-sensors-client` (off-android the
// API is no-op stubs), so this file needs no `cfg` of its own.

/// Map AIDL `SensorType` i32 → our WIT `Kind`. `Unknown` covers anything we
/// don't recognize so `list_sensors` keeps device-private sensors visible.
fn aidl_type_to_wit(t: i32) -> Kind {
    match t {
        1 => Kind::Accelerometer,
        2 => Kind::MagneticField,
        4 => Kind::Gyroscope,
        5 => Kind::Light,
        6 => Kind::Pressure,
        8 => Kind::Proximity,
        9 => Kind::Gravity,
        10 => Kind::LinearAcceleration,
        11 => Kind::RotationVector,
        12 => Kind::RelativeHumidity,
        13 => Kind::AmbientTemperature,
        15 => Kind::GameRotationVector,
        _ => Kind::Unknown,
    }
}

fn to_wit_info(s: SensorDesc) -> SensorInfo {
    SensorInfo {
        handle: s.handle,
        kind: aidl_type_to_wit(s.aidl_type),
        max_range: s.max_range,
        resolution: s.resolution,
        // minDelayUs is microseconds; convert to ms, clamp to >= 1 so a guest
        // divider never sees 0. On-change sensors return 0 → we expose 1 ms.
        min_delay_ms: ((s.min_delay_us / 1000).max(1)) as u32,
        power_ma: s.power_ma,
    }
}

fn to_wit_sample(s: SensorEvent) -> SensorSample {
    SensorSample { timestamp_ns: s.ts_ns, x: s.x, y: s.y, z: s.z }
}

// Task 94 — the host-internal device-orientation API (task 43:
// device_orientation_handle / enable_sensor / poll_device_rotation) was REMOVED.
// The arbiter's sensor-driver is now the sole device-orientation consumer (it
// reads the HAL sensor and pushes the decided content orient down to the host via
// `geometry`), so the host no longer reads or reports rotation. The guest-facing
// `sensors` WIT below stays — apps can still enumerate/enable/poll raw sensors.

impl Host for crate::HostState {
    fn list_sensors(&mut self) -> Vec<SensorInfo> {
        wandr_sensors_client::enumerate().into_iter().map(to_wit_info).collect()
    }

    fn enable(&mut self, handle: u32, rate_hz: u32) -> bool {
        wandr_sensors_client::enable(handle, rate_hz)
    }

    fn disable(&mut self, handle: u32) {
        wandr_sensors_client::disable(handle);
    }

    fn poll_latest(&mut self, handle: u32) -> SensorSample {
        wandr_sensors_client::poll_latest(handle)
            .map(to_wit_sample)
            .unwrap_or(SensorSample { timestamp_ns: 0, x: 0.0, y: 0.0, z: 0.0 })
    }
}
