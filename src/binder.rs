// rsbinder ProcessState init — one-shot for the process lifetime.
//
// rsbinder 0.8.0 changed `ProcessState::init_default` from `&'static ProcessState`
// to `Result<&'static ProcessState, Box<dyn Error>>` (PR #118's panic→Result
// hardening). We wrap it so callers get a stable `Result<(), &'static str>`
// and the host degrades gracefully (sysfs fallback for haptics, return-false
// for lights) when /dev/binder isn't reachable.

#[cfg(target_os = "android")]
pub fn init() -> Result<(), &'static str> {
    use std::sync::OnceLock;
    static RESULT: OnceLock<Result<(), &'static str>> = OnceLock::new();
    *RESULT.get_or_init(|| {
        if !std::path::Path::new("/dev/binder").exists() {
            return Err("/dev/binder not present");
        }
        if let Err(_e) = rsbinder::ProcessState::init_default() {
            return Err("rsbinder ProcessState::init_default failed");
        }
        rsbinder::ProcessState::start_thread_pool();
        Ok(())
    })
}

#[cfg(not(target_os = "android"))]
pub fn init() -> Result<(), &'static str> { Ok(()) }
