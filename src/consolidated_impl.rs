//! Phase A of the consolidation event (docs/ui-shell-consolidation.md):
//! the my:skiko-gfx platform remainder served under its REAL homes —
//! wandr:ui-shell, wandr:{device,chrome,assets}, wandr:ime/keyboard-send,
//! wasi:logging. Every impl DELEGATES to the existing my:skiko-gfx trait
//! impl (`<HostState as old::Host>::method(self, …)`) with mechanical
//! enum/record maps — semantics stay byte-identical; my:skiko-gfx keeps
//! serving until Phase C deletes it.

use crate::HostState;
use crate::bindings::my::skiko_gfx as old;

// ─── wandr:ui-shell ──────────────────────────────────────────────────────────

mod shell {
    pub use crate::ui_shell_bindings::wandr::ui_shell::*;
}

impl shell::metrics::Host for HostState {
    fn get_density(&mut self) -> f32 {
        <HostState as old::window::Host>::get_density(self)
    }
    fn get_font_scale(&mut self) -> f32 {
        <HostState as old::window::Host>::get_font_scale(self)
    }
    fn get_dpi(&mut self) -> u32 {
        <HostState as old::window::Host>::get_dpi(self)
    }
}

impl shell::theme::Host for HostState {
    fn get_night_mode(&mut self) -> shell::theme::NightMode {
        match <HostState as old::theme::Host>::get_night_mode(self) {
            old::theme::NightMode::Auto => shell::theme::NightMode::Auto,
            old::theme::NightMode::Off => shell::theme::NightMode::Off,
            old::theme::NightMode::On => shell::theme::NightMode::On,
        }
    }
    fn get_accent_color(&mut self) -> u32 {
        <HostState as old::theme::Host>::get_accent_color(self)
    }
}

impl shell::locale::Host for HostState {
    fn primary_locale(&mut self) -> String {
        <HostState as old::locale::Host>::primary_locale(self)
    }
    // WIT name is `is-twenty-four-hour-format`: a digit can't start a WIT
    // identifier word, so the legacy `is-24-hour-format` spelling can't
    // cross into the spec-clean package (guest wit-parsers reject it).
    fn is_twenty_four_hour_format(&mut self) -> bool {
        <HostState as old::locale::Host>::is_24_hour_format(self)
    }
    fn get_layout_direction(&mut self) -> shell::locale::LayoutDirection {
        match <HostState as old::locale::Host>::get_layout_direction(self) {
            old::locale::LayoutDirection::Ltr => shell::locale::LayoutDirection::Ltr,
            old::locale::LayoutDirection::Rtl => shell::locale::LayoutDirection::Rtl,
        }
    }
}

impl shell::clipboard::Host for HostState {
    fn get_text(&mut self) -> String {
        <HostState as old::clipboard::Host>::get_text(self)
    }
    fn set_text(&mut self, text: String) {
        <HostState as old::clipboard::Host>::set_text(self, text)
    }
    fn has_text(&mut self) -> bool {
        <HostState as old::clipboard::Host>::has_text(self)
    }
    fn clear(&mut self) {
        <HostState as old::clipboard::Host>::clear(self)
    }
}

impl shell::ime::Host for HostState {
    fn notify_editor_attached(
        &mut self,
        input_type: String,
        hint: String,
        initial_text: String,
        selection_start: u32,
        selection_end: u32,
    ) {
        <HostState as old::ime::Host>::notify_editor_attached(
            self, input_type, hint, initial_text, selection_start, selection_end,
        )
    }
    fn notify_editor_detached(&mut self) {
        <HostState as old::ime::Host>::notify_editor_detached(self)
    }
}

impl shell::lifecycle::Host for HostState {
    fn get_state(&mut self) -> shell::lifecycle::State {
        use shell::lifecycle::State as N;
        match <HostState as old::lifecycle::Host>::get_state(self) {
            old::lifecycle::State::Initialized => N::Initialized,
            old::lifecycle::State::Created => N::Created,
            old::lifecycle::State::Started => N::Started,
            old::lifecycle::State::Resumed => N::Resumed,
            old::lifecycle::State::Paused => N::Paused,
            old::lifecycle::State::Stopped => N::Stopped,
            old::lifecycle::State::Destroyed => N::Destroyed,
        }
    }
}

impl shell::scheduler::Host for HostState {
    fn schedule_delayed(&mut self, delay_ms: u32, callback_id: u32) -> u32 {
        <HostState as old::scheduler::Host>::schedule_delayed(self, delay_ms, callback_id)
    }
    fn cancel(&mut self, handle: u32) {
        <HostState as old::scheduler::Host>::cancel(self, handle)
    }
}

impl shell::text_segmentation::Host for HostState {
    fn next_boundary(
        &mut self,
        text: String,
        kind: shell::text_segmentation::BoundaryKind,
        start_offset: u32,
    ) -> u32 {
        <HostState as old::text_segmentation::Host>::next_boundary(
            self, text, seg_kind(kind), start_offset,
        )
    }
    fn prev_boundary(
        &mut self,
        text: String,
        kind: shell::text_segmentation::BoundaryKind,
        end_offset: u32,
    ) -> u32 {
        <HostState as old::text_segmentation::Host>::prev_boundary(
            self, text, seg_kind(kind), end_offset,
        )
    }
}

fn seg_kind(k: shell::text_segmentation::BoundaryKind) -> old::text_segmentation::BoundaryKind {
    use shell::text_segmentation::BoundaryKind as N;
    use old::text_segmentation::BoundaryKind as O;
    match k {
        N::Grapheme => O::Grapheme,
        N::Word => O::Word,
        N::Line => O::Line,
        N::Sentence => O::Sentence,
    }
}

// ─── wasi:logging ────────────────────────────────────────────────────────────

impl crate::logging_bindings::wasi::logging::logging::Host for HostState {
    fn log(
        &mut self,
        level: crate::logging_bindings::wasi::logging::logging::Level,
        context: String,
        message: String,
    ) {
        use crate::logging_bindings::wasi::logging::logging::Level as L;
        let lvl = match level {
            L::Trace => log::Level::Trace,
            L::Debug => log::Level::Debug,
            L::Info => log::Level::Info,
            L::Warn => log::Level::Warn,
            L::Error | L::Critical => log::Level::Error,
        };
        if context.is_empty() {
            log::log!(lvl, "guest: {message}");
        } else {
            log::log!(lvl, "guest[{context}]: {message}");
        }
    }
}

// ─── wandr:device ────────────────────────────────────────────────────────────

mod dev {
    pub use crate::device_bindings::wandr::device::*;
}

impl dev::haptics::Host for HostState {
    fn perform(&mut self, feedback: dev::haptics::Feedback) -> bool {
        use dev::haptics::Feedback as N;
        use old::haptics::Feedback as O;
        let f = match feedback {
            N::Tap => O::Tap,
            N::LongPress => O::LongPress,
            N::VirtualKey => O::VirtualKey,
            N::Click => O::Click,
            N::DoubleClick => O::DoubleClick,
        };
        <HostState as old::haptics::Host>::perform(self, f)
    }
    fn vibrate_ms(&mut self, duration_ms: u32) -> bool {
        <HostState as old::haptics::Host>::vibrate_ms(self, duration_ms)
    }
}

fn power_hint(k: dev::power::Hint) -> old::power::Hint {
    match k {
        dev::power::Hint::Interaction => old::power::Hint::Interaction,
        dev::power::Hint::DisplayUpdateImminent => old::power::Hint::DisplayUpdateImminent,
    }
}

fn power_mode(k: dev::power::Mode) -> old::power::Mode {
    use dev::power::Mode as N;
    use old::power::Mode as O;
    match k {
        N::LowPower => O::LowPower,
        N::SustainedPerformance => O::SustainedPerformance,
        N::FixedPerformance => O::FixedPerformance,
        N::ExpensiveRendering => O::ExpensiveRendering,
        N::Game => O::Game,
        N::Interactive => O::Interactive,
    }
}

impl dev::power::Host for HostState {
    fn boost(&mut self, kind: dev::power::Hint, duration_ms: u32) {
        <HostState as old::power::Host>::boost(self, power_hint(kind), duration_ms)
    }
    fn set_mode(&mut self, kind: dev::power::Mode, enabled: bool) {
        <HostState as old::power::Host>::set_mode(self, power_mode(kind), enabled)
    }
    fn is_hint_supported(&mut self, kind: dev::power::Hint) -> bool {
        <HostState as old::power::Host>::is_hint_supported(self, power_hint(kind))
    }
    fn is_mode_supported(&mut self, kind: dev::power::Mode) -> bool {
        <HostState as old::power::Host>::is_mode_supported(self, power_mode(kind))
    }
}

fn thermal_kind_new(k: old::thermal::Kind) -> dev::thermal::Kind {
    use dev::thermal::Kind as N;
    use old::thermal::Kind as O;
    match k {
        O::Cpu => N::Cpu,
        O::Gpu => N::Gpu,
        O::Battery => N::Battery,
        O::Skin => N::Skin,
        O::Modem => N::Modem,
        O::Npu => N::Npu,
        O::Display => N::Display,
        O::Soc => N::Soc,
        O::Wifi => N::Wifi,
        O::Camera => N::Camera,
        O::Speaker => N::Speaker,
        O::Ambient => N::Ambient,
    }
}

fn thermal_kind_old(k: dev::thermal::Kind) -> old::thermal::Kind {
    use dev::thermal::Kind as N;
    use old::thermal::Kind as O;
    match k {
        N::Cpu => O::Cpu,
        N::Gpu => O::Gpu,
        N::Battery => O::Battery,
        N::Skin => O::Skin,
        N::Modem => O::Modem,
        N::Npu => O::Npu,
        N::Display => O::Display,
        N::Soc => O::Soc,
        N::Wifi => O::Wifi,
        N::Camera => O::Camera,
        N::Speaker => O::Speaker,
        N::Ambient => O::Ambient,
    }
}

fn throttle_new(t: old::thermal::Throttle) -> dev::thermal::Throttle {
    use dev::thermal::Throttle as N;
    use old::thermal::Throttle as O;
    match t {
        O::None => N::None,
        O::Light => N::Light,
        O::Moderate => N::Moderate,
        O::Severe => N::Severe,
        O::Critical => N::Critical,
        O::Emergency => N::Emergency,
        O::Shutdown => N::Shutdown,
    }
}

fn temperature_new(t: old::thermal::Temperature) -> dev::thermal::Temperature {
    dev::thermal::Temperature {
        kind: thermal_kind_new(t.kind),
        celsius: t.celsius,
        throttle: throttle_new(t.throttle),
    }
}

impl dev::thermal::Host for HostState {
    fn list_temperatures(&mut self) -> Vec<dev::thermal::Temperature> {
        <HostState as old::thermal::Host>::list_temperatures(self)
            .into_iter()
            .map(temperature_new)
            .collect()
    }
    fn list_temperatures_of(&mut self, kind: dev::thermal::Kind) -> Vec<dev::thermal::Temperature> {
        <HostState as old::thermal::Host>::list_temperatures_of(self, thermal_kind_old(kind))
            .into_iter()
            .map(temperature_new)
            .collect()
    }
    fn overall_throttle(&mut self) -> dev::thermal::Throttle {
        throttle_new(<HostState as old::thermal::Host>::overall_throttle(self))
    }
}

fn sensor_kind_new(k: old::sensors::Kind) -> dev::sensors::Kind {
    use dev::sensors::Kind as N;
    use old::sensors::Kind as O;
    match k {
        O::Unknown => N::Unknown,
        O::Accelerometer => N::Accelerometer,
        O::MagneticField => N::MagneticField,
        O::Gyroscope => N::Gyroscope,
        O::Light => N::Light,
        O::Pressure => N::Pressure,
        O::Proximity => N::Proximity,
        O::Gravity => N::Gravity,
        O::LinearAcceleration => N::LinearAcceleration,
        O::RotationVector => N::RotationVector,
        O::RelativeHumidity => N::RelativeHumidity,
        O::AmbientTemperature => N::AmbientTemperature,
        O::GameRotationVector => N::GameRotationVector,
    }
}

impl dev::sensors::Host for HostState {
    fn list_sensors(&mut self) -> Vec<dev::sensors::SensorInfo> {
        <HostState as old::sensors::Host>::list_sensors(self)
            .into_iter()
            .map(|s| dev::sensors::SensorInfo {
                handle: s.handle,
                kind: sensor_kind_new(s.kind),
                max_range: s.max_range,
                resolution: s.resolution,
                min_delay_ms: s.min_delay_ms,
                power_ma: s.power_ma,
            })
            .collect()
    }
    fn enable(&mut self, handle: u32, rate_hz: u32) -> bool {
        <HostState as old::sensors::Host>::enable(self, handle, rate_hz)
    }
    fn disable(&mut self, handle: u32) {
        <HostState as old::sensors::Host>::disable(self, handle)
    }
    fn poll_latest(&mut self, handle: u32) -> dev::sensors::SensorSample {
        let s = <HostState as old::sensors::Host>::poll_latest(self, handle);
        dev::sensors::SensorSample { timestamp_ns: s.timestamp_ns, x: s.x, y: s.y, z: s.z }
    }
}

impl dev::lights::Host for HostState {
    fn set(&mut self, kind: dev::lights::LightType, state: dev::lights::LightState) -> bool {
        use dev::lights::{FlashMode as NF, LightType as N};
        use old::lights::{FlashMode as OF, LightType as O};
        let k = match kind {
            N::Backlight => O::Backlight,
            N::Keyboard => O::Keyboard,
            N::Buttons => O::Buttons,
            N::Battery => O::Battery,
            N::Notifications => O::Notifications,
            N::Attention => O::Attention,
            N::Bluetooth => O::Bluetooth,
            N::Wifi => O::Wifi,
            N::Microphone => O::Microphone,
        };
        let st = old::lights::LightState {
            color_argb: state.color_argb,
            flash_on_ms: state.flash_on_ms,
            flash_off_ms: state.flash_off_ms,
            flash_mode: match state.flash_mode {
                NF::None => OF::None,
                NF::Timed => OF::Timed,
                NF::Hardware => OF::Hardware,
            },
        };
        <HostState as old::lights::Host>::set(self, k, st)
    }
    fn supports(&mut self, kind: dev::lights::LightType) -> bool {
        use dev::lights::LightType as N;
        use old::lights::LightType as O;
        let k = match kind {
            N::Backlight => O::Backlight,
            N::Keyboard => O::Keyboard,
            N::Buttons => O::Buttons,
            N::Battery => O::Battery,
            N::Notifications => O::Notifications,
            N::Attention => O::Attention,
            N::Bluetooth => O::Bluetooth,
            N::Wifi => O::Wifi,
            N::Microphone => O::Microphone,
        };
        <HostState as old::lights::Host>::supports(self, k)
    }
}

// ─── wandr:chrome ────────────────────────────────────────────────────────────

mod chrome {
    pub use crate::chrome_bindings::wandr::chrome::*;
}

impl chrome::launcher::Host for HostState {
    fn list_apps(&mut self) -> String {
        <HostState as old::launcher::Host>::list_apps(self)
    }
    fn launch_app(&mut self, app_id: String) {
        <HostState as old::launcher::Host>::launch_app(self, app_id)
    }
    fn go_home(&mut self) {
        <HostState as old::launcher::Host>::go_home(self)
    }
    fn go_back(&mut self) {
        <HostState as old::launcher::Host>::go_back(self)
    }
    fn recents(&mut self) {
        <HostState as old::launcher::Host>::recents(self)
    }
}

impl chrome::status::Host for HostState {
    fn clock_text(&mut self) -> String {
        <HostState as old::status::Host>::clock_text(self)
    }
    fn battery_text(&mut self) -> String {
        <HostState as old::status::Host>::battery_text(self)
    }
    fn bar_height(&mut self) -> u32 {
        <HostState as old::status::Host>::bar_height(self)
    }
}

impl chrome::display::Host for HostState {
    fn display_size(&mut self) -> chrome::display::Size {
        let s = <HostState as old::display::Host>::display_size(self);
        chrome::display::Size { width: s.width, height: s.height }
    }
    fn content_size(&mut self) -> chrome::display::Size {
        let s = <HostState as old::display::Host>::content_size(self);
        chrome::display::Size { width: s.width, height: s.height }
    }
    fn safe_size(&mut self) -> chrome::display::Size {
        let s = <HostState as old::display::Host>::safe_size(self);
        chrome::display::Size { width: s.width, height: s.height }
    }
    fn current_orientation(&mut self) -> chrome::display::Orientation {
        match <HostState as old::display::Host>::current_orientation(self) {
            old::display::Orientation::Portrait => chrome::display::Orientation::Portrait,
            old::display::Orientation::Landscape => chrome::display::Orientation::Landscape,
        }
    }
}

impl chrome::pointer_icon::Host for HostState {
    fn set(&mut self, kind: chrome::pointer_icon::Kind) {
        use chrome::pointer_icon::Kind as N;
        use old::pointer_icon::Kind as O;
        let k = match kind {
            N::Default => O::Default,
            N::Text => O::Text,
            N::Hand => O::Hand,
            N::Crosshair => O::Crosshair,
            N::Wait => O::Wait,
            N::Help => O::Help,
            N::Progress => O::Progress,
            N::NotAllowed => O::NotAllowed,
            N::Grab => O::Grab,
            N::Grabbing => O::Grabbing,
            N::Copy => O::Copy,
            N::Move => O::Move,
            N::ResizeNs => O::ResizeNs,
            N::ResizeEw => O::ResizeEw,
            N::ResizeNesw => O::ResizeNesw,
            N::ResizeNwse => O::ResizeNwse,
            N::AllScroll => O::AllScroll,
            N::ZoomIn => O::ZoomIn,
            N::ZoomOut => O::ZoomOut,
        };
        <HostState as old::pointer_icon::Host>::set(self, k)
    }
}

// ─── wandr:assets ────────────────────────────────────────────────────────────

impl crate::assets_pkg_bindings::wandr::assets::assets::Host for HostState {
    fn read(&mut self, name: String) -> Option<Vec<u8>> {
        <HostState as old::assets::Host>::read(self, name)
    }
}

// ─── wandr:ime/keyboard-send ─────────────────────────────────────────────────

impl crate::keyboard_send_bindings::wandr::ime::keyboard_send::Host for HostState {
    fn send_key_event(&mut self, code_point: u32, key_id: u32, action: u8) {
        <HostState as old::keyboard::Host>::send_key_event(self, code_point, key_id, action)
    }
    fn request_overlay_height(&mut self, height_px: u32) {
        <HostState as old::keyboard::Host>::request_overlay_height(self, height_px)
    }
}

// ─── linker registration (both app_loader sites) ─────────────────────────────

pub fn add_to_linker(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> wasmtime::Result<()> {
    use wasmtime::component::HasSelf;
    crate::ui_shell_bindings::UiShellImports::add_to_linker::<_, HasSelf<HostState>>(linker, |s| s)?;
    crate::logging_bindings::Imports::add_to_linker::<_, HasSelf<HostState>>(linker, |s| s)?;
    crate::device_bindings::DeviceImports::add_to_linker::<_, HasSelf<HostState>>(linker, |s| s)?;
    crate::chrome_bindings::ChromeImports::add_to_linker::<_, HasSelf<HostState>>(linker, |s| s)?;
    crate::assets_pkg_bindings::AssetsImports::add_to_linker::<_, HasSelf<HostState>>(linker, |s| s)?;
    crate::keyboard_send_bindings::KeyboardSendImports::add_to_linker::<_, HasSelf<HostState>>(linker, |s| s)?;
    Ok(())
}
