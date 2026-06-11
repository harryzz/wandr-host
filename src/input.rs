use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use wasmtime::Store;
use crate::HostState;
use crate::bindings::SkikoUi;
use crate::bindings::exports::my::skiko_gfx::renderer::{KeyKind, PointerKind};
use crate::input_handlers_bindings as ih;

/// Per-guest wasi:input-handlers probes (proposals/wasi-input-handlers).
/// Per input type, dispatch routes EXCLUSIVELY to a bound handler — a
/// guest that exports the new model never also receives the legacy
/// renderer events, so no guest-side de-dup is needed.
#[derive(Default)]
pub struct GuestInput {
    pub pointer: Option<ih::pointer::PointerHandlerWorld>,
    pub key: Option<ih::key::KeyHandlerWorld>,
    pub frame: Option<ih::frame::FrameHandlerWorld>,
}

pub type PointerEventV4 = ih::pointer::exports::wasi::input_handlers::pointer_handler::PointerEvent;
pub type PointerKindV4 = ih::pointer::exports::wasi::input_handlers::pointer_handler::Kind;
pub type KeyEventV4 = ih::key::exports::wasi::input_handlers::key_handler::KeyEvent;

/// Touch-suppression gate (task 79). When set, all pointer dispatch is dropped
/// — used during a proximity screen-off (call at the ear) so a cheek/ear touch
/// can't trigger taps on the invisible UI. The arbiter pushes `input-suppress
/// <0|1>` (parsed in `ime_inbound`) to toggle it, tied to the panel-blank state.
/// Process-global: set on the control-socket thread, read on the render thread.
/// Hardware keys are deliberately NOT gated (volume etc. stay usable in a call).
static TOUCH_SUPPRESSED: AtomicBool = AtomicBool::new(false);
/// Touches dropped in the current suppression episode (reset on each engage) —
/// just enough to log the first drop as on-device proof without per-event spam.
static SUPPRESSED_DROPS: AtomicU32 = AtomicU32::new(0);

/// Toggle touch suppression. Logs only on an actual state change.
pub fn set_touch_suppressed(on: bool) {
    if TOUCH_SUPPRESSED.swap(on, Ordering::Relaxed) != on {
        if on {
            SUPPRESSED_DROPS.store(0, Ordering::Relaxed);
        }
        log::info!("input: touch {}", if on { "SUPPRESSED (proximity blank)" } else { "resumed" });
    }
}

/// The dispatch gate: true if touch is suppressed. Counts + logs the first drop
/// of each episode so a real cheek/finger touch during the blank is visible.
fn touch_suppressed() -> bool {
    if TOUCH_SUPPRESSED.load(Ordering::Relaxed) {
        if SUPPRESSED_DROPS.fetch_add(1, Ordering::Relaxed) == 0 {
            log::info!("input: dropped touch while suppressed (first of episode)");
        }
        true
    } else {
        false
    }
}

pub fn dispatch_pointer(
    bindings: &SkikoUi,
    store: &mut Store<HostState>,
    kind: u8,
    x: f32, y: f32,
) -> anyhow::Result<()> {
    // Task 79 — drop touch while the panel is blanked for proximity.
    if touch_suppressed() {
        return Ok(());
    }
    let kind = match kind {
        0 => PointerKind::Down,
        1 => PointerKind::Up,
        2 => PointerKind::Move,
        _ => PointerKind::Scroll,
    };
    bindings.my_skiko_gfx_renderer()
        .call_on_pointer_event(store, kind, x, y)?;
    Ok(())
}

/// Enriched dispatch: also delivers pointer-id (multi-touch) and pressure.
/// Calls both v1 (for backward compat) and v2 (for callers that want the
/// extras). Single-touch / mouse callers should pass pointer_id=0.
pub fn dispatch_pointer_v2(
    bindings: &SkikoUi,
    store: &mut Store<HostState>,
    kind: u8,
    pointer_id: u32,
    x: f32, y: f32,
    pressure: f32,
) -> anyhow::Result<()> {
    dispatch_pointer_routed(bindings, store, &GuestInput::default(), kind, pointer_id, x, y, pressure, [false; 4])
}

/// Pointer dispatch with wasi:input-handlers routing: a bound
/// pointer-handler receives the event EXCLUSIVELY (kind=cancel and
/// scroll deltas only exist on this path); otherwise legacy v1+v2.
/// `mods` = [alt, ctrl, meta, shift] (desktop; touch passes false).
pub fn dispatch_pointer_routed(
    bindings: &SkikoUi,
    store: &mut Store<HostState>,
    guest_input: &GuestInput,
    kind: u8,
    pointer_id: u32,
    x: f32, y: f32,
    pressure: f32,
    mods: [bool; 4],
) -> anyhow::Result<()> {
    // Task 79 — drop touch while the panel is blanked for proximity.
    if touch_suppressed() {
        return Ok(());
    }
    if let Some(ph) = &guest_input.pointer {
        let ev = PointerEventV4 {
            id: pointer_id,
            kind: match kind {
                0 => PointerKindV4::Down,
                1 => PointerKindV4::Up,
                2 => PointerKindV4::Move,
                4 => PointerKindV4::Cancel,
                _ => PointerKindV4::Scroll,
            },
            x, y,
            pressure,
            scroll_dx: 0.0,
            scroll_dy: 0.0,
            alt: mods[0], ctrl: mods[1], meta: mods[2], shift: mods[3],
        };
        ph.wasi_input_handlers_pointer_handler().call_on_pointer(store, ev)?;
        return Ok(());
    }
    let pk = match kind {
        0 => PointerKind::Down,
        1 => PointerKind::Up,
        2 => PointerKind::Move,
        _ => PointerKind::Scroll,
    };
    let r = bindings.my_skiko_gfx_renderer();
    r.call_on_pointer_event(&mut *store, pk, x, y)?;
    r.call_on_pointer_event_v2(store, pointer_id, pk, x, y, pressure)?;
    Ok(())
}

/// Key dispatch with wasi:input-handlers routing: a bound key-handler
/// receives the W3C-model event EXCLUSIVELY; otherwise legacy v1+v2 (+
/// my:skiko-gfx key-input when that probe bound).
pub fn dispatch_key_routed(
    guest_input: &GuestInput,
    store: &mut Store<HostState>,
    ev: &KeyEventV4,
) -> anyhow::Result<bool> {
    if let Some(kh) = &guest_input.key {
        kh.wasi_input_handlers_key_handler().call_on_key(store, ev)?;
        return Ok(true);
    }
    Ok(false)
}

/// Frame dispatch: a bound frame-handler owns render driving; else the
/// legacy renderer.render-frame. Returns the call result either way.
pub fn dispatch_frame(
    bindings: &SkikoUi,
    store: &mut Store<HostState>,
    guest_input: &GuestInput,
    nanos: u64,
) -> anyhow::Result<()> {
    if let Some(fh) = &guest_input.frame {
        fh.wasi_input_handlers_frame_handler().call_on_frame(store, nanos)?;
    } else {
        bindings.my_skiko_gfx_renderer().call_render_frame(store, nanos)?;
    }
    Ok(())
}

/// Resize dispatch: a bound frame-handler receives it EXCLUSIVELY.
pub fn dispatch_resize_routed(
    bindings: &SkikoUi,
    store: &mut Store<HostState>,
    guest_input: &GuestInput,
    w: u32, h: u32,
) -> anyhow::Result<()> {
    if let Some(fh) = &guest_input.frame {
        fh.wasi_input_handlers_frame_handler().call_on_resize(store, w, h)?;
    } else {
        bindings.my_skiko_gfx_renderer().call_on_resize(store, w, h)?;
    }
    Ok(())
}

pub fn dispatch_key(
    bindings: &SkikoUi,
    store: &mut Store<HostState>,
    kind: u8, key_code: u32,
) -> anyhow::Result<()> {
    let kind = if kind == 0 { KeyKind::Down } else { KeyKind::Up };
    bindings.my_skiko_gfx_renderer()
        .call_on_key_event(store, kind, key_code)?;
    Ok(())
}

/// Enriched key dispatch: carries the resolved UTF-32 codepoint AND a
/// Compose-compatible key-id. Hosts emit both v1 (`on-key-event`) and v2
/// (`on-key-event-v2`) for every keystroke. Guests can ignore whichever
/// they don't need.
pub fn dispatch_key_v2(
    bindings: &SkikoUi,
    store: &mut Store<HostState>,
    kind: u8, code_point: u32, key_id: u32,
) -> anyhow::Result<()> {
    let kk = if kind == 0 { KeyKind::Down } else { KeyKind::Up };
    bindings.my_skiko_gfx_renderer()
        .call_on_key_event_v2(store, kk, code_point, key_id)?;
    Ok(())
}

/// Standalone-path key dispatch: takes raw Android `AKEYCODE_*` + meta-state
/// from the InputFlinger `KeyEvent`, maps to (code_point, key_id) the way
/// the NativeActivity path's winit branch does, and fires both v1 + v2.
///
/// Mapping covers the keys most NativeActivity testing exercised: letters,
/// digits, space, common editing keys (Backspace, Enter, Tab, Esc, Arrow*,
/// PageUp/Down, Home/End, Insert/Delete) and a handful of punctuation.
/// Unmapped keys still fire as `code_point = 0, key_id = 0` so the guest
/// at least sees a keystroke.
pub fn dispatch_android_key(
    bindings: &SkikoUi,
    store: &mut Store<HostState>,
    guest_input: &GuestInput,
    key_input: Option<&crate::key_input_bindings::KeyInputWorld>,
    kind: u8, key_code: i32, meta_state: i32,
) -> anyhow::Result<()> {
    let shift = (meta_state & AMETA_SHIFT_ON) != 0;
    let (code_point, key_id) = map_android_keycode(key_code, shift);
    // wasi:input-handlers key-handler supersedes every legacy path.
    let ev4 = KeyEventV4 {
        down: kind == 0,
        repeat: false,
        code: android_keycode_to_w3c(key_code),
        text: char::from_u32(code_point).filter(|c| *c != '\0').map(String::from)
            .unwrap_or_default(),
        alt: (meta_state & AMETA_ALT_ON) != 0,
        ctrl: (meta_state & AMETA_CTRL_ON) != 0,
        meta: (meta_state & AMETA_META_ON) != 0,
        shift,
    };
    if dispatch_key_routed(guest_input, store, &ev4)? {
        return Ok(());
    }
    let r = bindings.my_skiko_gfx_renderer();
    // v1 carries the raw AKEYCODE so callers that wired against it still work.
    let kk = if kind == 0 { KeyKind::Down } else { KeyKind::Up };
    r.call_on_key_event(&mut *store, kk, key_code.max(0) as u32)?;
    r.call_on_key_event_v2(&mut *store, kk, code_point, key_id)?;
    // v3 (optional): the W3C model — physical code token + full meta bits.
    let ev = crate::key_input_bindings::exports::my::skiko_gfx::key_input::KeyEvent {
        down: kind == 0,
        repeat: false, // InputFlinger repeat events arrive as fresh downs here
        code: android_keycode_to_w3c(key_code),
        text: char::from_u32(code_point).filter(|c| *c != '\0').map(String::from)
            .unwrap_or_default(),
        alt: (meta_state & AMETA_ALT_ON) != 0,
        ctrl: (meta_state & AMETA_CTRL_ON) != 0,
        meta: (meta_state & AMETA_META_ON) != 0,
        shift,
    };
    dispatch_key_v3(key_input, store, &ev)?;
    Ok(())
}

// AMETA_* bits from <android/input.h>.
const AMETA_SHIFT_ON: i32 = 0x01;
const AMETA_ALT_ON: i32 = 0x02;
const AMETA_CTRL_ON: i32 = 0x1000;
const AMETA_META_ON: i32 = 0x10000;

/// The optional v3 key dispatch — the W3C UIEvents model (`key-input`
/// interface: code token + modifiers + text). No-op when the guest
/// doesn't export it; emitted IN ADDITION to v1/v2 by every key path.
pub fn dispatch_key_v3(
    key_input: Option<&crate::key_input_bindings::KeyInputWorld>,
    store: &mut Store<HostState>,
    ev: &crate::key_input_bindings::exports::my::skiko_gfx::key_input::KeyEvent,
) -> anyhow::Result<()> {
    if let Some(ki) = key_input {
        ki.my_skiko_gfx_key_input().call_on_key(store, ev)?;
    }
    Ok(())
}

/// W3C UIEvents `code` token for an Android `AKEYCODE_*`
/// (https://w3c.github.io/uievents-code/). "" = unidentified.
fn android_keycode_to_w3c(code: i32) -> String {
    // Letters: AKEYCODE_A=29 .. AKEYCODE_Z=54 → "KeyA".."KeyZ"
    if (29..=54).contains(&code) {
        return format!("Key{}", (b'A' + (code as u8 - 29)) as char);
    }
    // Digits: AKEYCODE_0=7 .. AKEYCODE_9=16 → "Digit0".."Digit9"
    if (7..=16).contains(&code) {
        return format!("Digit{}", (b'0' + (code as u8 - 7)) as char);
    }
    match code {
        62 => "Space",
        67 => "Backspace",
        66 => "Enter",
        160 => "NumpadEnter", // AKEYCODE_NUMPAD_ENTER
        61 => "Tab",
        111 => "Escape",
        21 => "ArrowLeft",
        19 => "ArrowUp",
        22 => "ArrowRight",
        20 => "ArrowDown",
        92 => "PageUp",
        93 => "PageDown",
        122 => "Home",
        123 => "End",
        124 => "Insert",
        112 => "Delete",
        55 => "Comma",
        56 => "Period",
        74 => "Semicolon",
        75 => "Quote",
        76 => "Slash",
        73 => "Backslash",
        69 => "Minus",
        70 => "Equal",
        68 => "Backquote",
        71 => "BracketLeft",
        72 => "BracketRight",
        115 => "CapsLock",
        59 => "ShiftLeft",
        60 => "ShiftRight",
        57 => "AltLeft",
        58 => "AltRight",
        113 => "ControlLeft",
        114 => "ControlRight",
        117 => "MetaLeft",
        118 => "MetaRight",
        24 => "AudioVolumeUp",
        25 => "AudioVolumeDown",
        164 => "AudioVolumeMute",
        26 => "Power",
        _ => "",
    }
    .to_string()
}

/// W3C UIEvents `code` token for a Compose-webMain `key-id` (the soft-
/// keyboard / IME wire format — no physical key, so only the named keys
/// map; printable text rides the code-point instead).
pub fn key_id_to_w3c(key_id: u32) -> &'static str {
    match key_id {
        8 => "Backspace",
        9 => "Tab",
        13 => "Enter",
        27 => "Escape",
        32 => "Space",
        33 => "PageUp",
        34 => "PageDown",
        35 => "End",
        36 => "Home",
        37 => "ArrowLeft",
        38 => "ArrowUp",
        39 => "ArrowRight",
        40 => "ArrowDown",
        45 => "Insert",
        46 => "Delete",
        _ => "",
    }
}

/// Translate Android `AKEYCODE_*` into (code-point, key-id) for the guest's
/// `on-key-event-v2`. Mirrors the winit `KeyboardInput` branch in
/// `lib.rs` so both code paths feed Compose the same numeric IDs.
fn map_android_keycode(code: i32, shift: bool) -> (u32, u32) {
    // Letters: AKEYCODE_A=29 .. AKEYCODE_Z=54
    if (29..=54).contains(&code) {
        let base = if shift { b'A' } else { b'a' };
        return ((base + (code as u8 - 29)) as u32, 0);
    }
    // Digits: AKEYCODE_0=7 .. AKEYCODE_9=16
    if (7..=16).contains(&code) {
        return ((b'0' + (code as u8 - 7)) as u32, 0);
    }
    match code {
        62  => (b' ' as u32, 32), // AKEYCODE_SPACE         → ' ' + Space key-id
        67  => (0, 8),            // AKEYCODE_DEL           → Backspace
        66  => (0, 13),           // AKEYCODE_ENTER         → Enter
        61  => (0, 9),            // AKEYCODE_TAB           → Tab
        111 => (0, 27),           // AKEYCODE_ESCAPE        → Escape
        21  => (0, 37),           // AKEYCODE_DPAD_LEFT     → ArrowLeft
        19  => (0, 38),           // AKEYCODE_DPAD_UP       → ArrowUp
        22  => (0, 39),           // AKEYCODE_DPAD_RIGHT    → ArrowRight
        20  => (0, 40),           // AKEYCODE_DPAD_DOWN     → ArrowDown
        92  => (0, 33),           // AKEYCODE_PAGE_UP
        93  => (0, 34),           // AKEYCODE_PAGE_DOWN
        122 => (0, 36),           // AKEYCODE_MOVE_HOME
        123 => (0, 35),           // AKEYCODE_MOVE_END
        124 => (0, 45),           // AKEYCODE_INSERT
        112 => (0, 46),           // AKEYCODE_FORWARD_DEL   → Delete
        55  => (b',' as u32, 0),  // AKEYCODE_COMMA
        56  => (b'.' as u32, 0),  // AKEYCODE_PERIOD
        74  => (b';' as u32, 0),  // AKEYCODE_SEMICOLON
        75  => (b'\'' as u32, 0), // AKEYCODE_APOSTROPHE
        76  => (b'/' as u32, 0),  // AKEYCODE_SLASH
        69  => (b'-' as u32, 0),  // AKEYCODE_MINUS
        70  => (b'=' as u32, 0),  // AKEYCODE_EQUALS
        _   => (0, 0),
    }
}

pub fn dispatch_resize(
    bindings: &SkikoUi,
    store: &mut Store<HostState>,
    w: u32, h: u32,
) -> anyhow::Result<()> {
    bindings.my_skiko_gfx_renderer()
        .call_on_resize(store, w, h)?;
    Ok(())
}
