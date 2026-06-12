//! Consolidation linker hub (Phase C state): every interface impl now
//! lives in its own `*_impl.rs` targeting the NEW bindgen traits directly
//! (the Phase A delegations to my:skiko-gfx inverted when the legacy
//! bindgen died). This file keeps the cross-package bits: the wasi:logging
//! impl (born new — no legacy ancestor) and the one `add_to_linker`
//! registering all six packages at both app_loader sites.

use crate::HostState;

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
