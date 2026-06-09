// Intercepts bundled-bionic symbols that crash on LineageOS 22.2.
//
// Root cause: the NDK's libc.a bundles full bionic implementations of
// getauxval and pthread_create. The bundled getauxval calls
// __libc_shared_globals() whose auxv pointer is NULL in the bundled copy,
// causing SIGSEGV. Rust's compiler_builtins (init_have_lse_atomics etc.)
// and Scudo both call getauxval via UNDEFINED references — so
// -Wl,--wrap=getauxval intercepts every call at link time.
//
// Similarly, the bundled pthread_create crashes in __init_tcb; we locate
// the system version by scanning /proc/self/maps instead.
//
// Activated by -Wl,--wrap=getauxval and -Wl,--wrap=pthread_create in
// .cargo/config.toml.

use core::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

// ── Malloc redirect ───────────────────────────────────────────────────────────
//
// NDK libc.a bundles a full Scudo allocator that crashes on LineageOS 22.2
// because the bundled bionic globals are never properly initialised.
// The NDK sysroot stub libc.so intentionally does NOT export malloc/free/realloc
// so the linker always falls back to libc.a, pulling in the broken Scudo.
//
// Fix: --wrap=malloc/free/realloc/calloc/memalign intercepts all calls and
// redirects them to the system libc.so's allocator found via dlsym.
// dlsym lives in libdl.so (the dynamic linker) and uses a private arena,
// so there is no malloc recursion during the lookup.
//
// After --wrap=malloc the original libc.a malloc is renamed __real_malloc
// and is NOT in the .so's dynamic symbol table, so dlsym(RTLD_DEFAULT, "malloc")
// finds the system libc.so version, not our wrapper.

extern "C" {
    fn dlsym(handle: *const c_void, symbol: *const u8) -> *const c_void;
}

const RTLD_DEFAULT: *const c_void = core::ptr::null();
// Sentinel: "dlsym lookup is in progress on this thread".
// A recursive malloc call (dlsym calling malloc) will see this and use the
// bootstrap arena instead of spinning.
const INIT: usize = 1;

static REAL_MALLOC:   AtomicUsize = AtomicUsize::new(0);
static REAL_FREE:     AtomicUsize = AtomicUsize::new(0);
static REAL_REALLOC:  AtomicUsize = AtomicUsize::new(0);
static REAL_CALLOC:   AtomicUsize = AtomicUsize::new(0);
static REAL_MEMALIGN: AtomicUsize = AtomicUsize::new(0);

// ── Bootstrap arena ───────────────────────────────────────────────────────────
// Used for the tiny allocations dlsym makes while we are looking up system malloc.
// Never freed — it is only active during the first few milliseconds of startup.
#[repr(align(16))]
struct Arena([u8; 131072]); // 128 KB
static ARENA: Arena = Arena([0u8; 131072]);
static ARENA_POS: AtomicUsize = AtomicUsize::new(0);

fn is_bootstrap(ptr: *const c_void) -> bool {
    let p = ptr as usize;
    let base = ARENA.0.as_ptr() as usize;
    p >= base && p < base + core::mem::size_of::<Arena>()
}

fn bootstrap_alloc(size: usize) -> *mut c_void {
    let aligned = (size + 15) & !15;
    let pos = ARENA_POS.fetch_add(aligned, Ordering::Relaxed);
    if pos + aligned <= core::mem::size_of::<Arena>() {
        unsafe { ARENA.0.as_ptr().add(pos) as *mut c_void }
    } else {
        core::ptr::null_mut()
    }
}

// ── System malloc lookup ──────────────────────────────────────────────────────

unsafe fn get_real_malloc() -> usize {
    let v = REAL_MALLOC.load(Ordering::Acquire);
    if v > INIT { return v; }
    if v == 0 && REAL_MALLOC.compare_exchange(0, INIT, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        // We own the lookup.  dlsym may call malloc recursively; those calls
        // land in the bootstrap arena (see __wrap_malloc below).
        let m = dlsym(RTLD_DEFAULT, b"malloc\0".as_ptr()) as usize;
        let f = dlsym(RTLD_DEFAULT, b"free\0".as_ptr()) as usize;
        let r = dlsym(RTLD_DEFAULT, b"realloc\0".as_ptr()) as usize;
        let c = dlsym(RTLD_DEFAULT, b"calloc\0".as_ptr()) as usize;
        let a = dlsym(RTLD_DEFAULT, b"memalign\0".as_ptr()) as usize;
        REAL_FREE.store(f, Ordering::Release);
        REAL_REALLOC.store(r, Ordering::Release);
        REAL_CALLOC.store(c, Ordering::Release);
        REAL_MEMALIGN.store(a, Ordering::Release);
        REAL_MALLOC.store(m, Ordering::Release); // publish last — other callers spin on this
        return m;
    }
    // Another thread (or recursive call) is in lookup; spin until done.
    loop {
        let v = REAL_MALLOC.load(Ordering::Acquire);
        if v > INIT { return v; }
        core::hint::spin_loop();
    }
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_malloc(size: usize) -> *mut c_void {
    let v = REAL_MALLOC.load(Ordering::Acquire);
    if v == INIT {
        // Recursive call from inside dlsym — use bootstrap arena.
        return bootstrap_alloc(size);
    }
    if v > INIT {
        let f: unsafe extern "C" fn(usize) -> *mut c_void = core::mem::transmute(v);
        return f(size);
    }
    // v == 0: we are the first caller — trigger lookup.
    let addr = get_real_malloc();
    let f: unsafe extern "C" fn(usize) -> *mut c_void = core::mem::transmute(addr);
    f(size)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_free(ptr: *mut c_void) {
    if ptr.is_null() || is_bootstrap(ptr) { return; }
    let v = REAL_FREE.load(Ordering::Acquire);
    if v <= INIT { return; } // bootstrap or init — leaking is fine
    let f: unsafe extern "C" fn(*mut c_void) = core::mem::transmute(v);
    f(ptr)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    if is_bootstrap(ptr) {
        // Old block is in the bootstrap arena — cannot resize it in place.
        // Allocate new (this will go through system malloc once it is ready).
        let new = __wrap_malloc(size);
        if !new.is_null() && !ptr.is_null() {
            // Copy up to the old bootstrap-allocated size (unknown, so copy up to size).
            core::ptr::copy_nonoverlapping(ptr as *const u8, new as *mut u8, size.min(4096));
        }
        return new;
    }
    let v = REAL_REALLOC.load(Ordering::Acquire);
    if v <= INIT {
        return __wrap_malloc(size); // fallback
    }
    let f: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void = core::mem::transmute(v);
    f(ptr, size)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_calloc(nmemb: usize, size: usize) -> *mut c_void {
    let v = REAL_CALLOC.load(Ordering::Acquire);
    if v == INIT {
        let p = bootstrap_alloc(nmemb * size);
        if !p.is_null() {
            core::ptr::write_bytes(p as *mut u8, 0, nmemb * size);
        }
        return p;
    }
    if v > INIT {
        let f: unsafe extern "C" fn(usize, usize) -> *mut c_void = core::mem::transmute(v);
        return f(nmemb, size);
    }
    get_real_malloc(); // ensure all slots are filled
    let f: unsafe extern "C" fn(usize, usize) -> *mut c_void =
        core::mem::transmute(REAL_CALLOC.load(Ordering::Acquire));
    f(nmemb, size)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_memalign(align: usize, size: usize) -> *mut c_void {
    let v = REAL_MEMALIGN.load(Ordering::Acquire);
    if v == INIT {
        // Aligned allocation in bootstrap arena — simple bump, ignore alignment
        return bootstrap_alloc(size);
    }
    if v > INIT {
        let f: unsafe extern "C" fn(usize, usize) -> *mut c_void = core::mem::transmute(v);
        return f(align, size);
    }
    get_real_malloc();
    let f: unsafe extern "C" fn(usize, usize) -> *mut c_void =
        core::mem::transmute(REAL_MEMALIGN.load(Ordering::Acquire));
    f(align, size)
}

// AT_PAGESZ=6 → 4096, AT_CLKTCK=17 → 100, everything else → 0.
// Returning 0 for AT_HWCAP/AT_HWCAP2 safely disables LSE probing.
// Returning 0 for AT_RANDOM is fine for Scudo (falls back to a fixed seed).
#[no_mangle]
pub unsafe extern "C" fn __wrap_getauxval(typ: u64) -> u64 {
    match typ {
        6  => 4096,
        17 => 100,
        _  => 0,
    }
}

static REAL: AtomicUsize = AtomicUsize::new(0);

type StartFn = Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>;
type RealFn = unsafe extern "C" fn(*mut c_void, *const c_void, StartFn, *mut c_void) -> c_int;

#[no_mangle]
pub unsafe extern "C" fn __wrap_pthread_create(
    thread: *mut c_void,
    attr:   *const c_void,
    start:  StartFn,
    arg:    *mut c_void,
) -> c_int {
    let mut ptr = REAL.load(Ordering::Acquire);
    if ptr == 0 {
        // dlsym searches the dynamic symbol tables; with --wrap=pthread_create
        // our .so has no dynamic "pthread_create" export, so this finds the
        // system libc.so version (the one we actually want).
        ptr = dlsym(RTLD_DEFAULT, b"pthread_create\0".as_ptr()) as usize;
        assert!(ptr != 0, "bionic_compat: dlsym(pthread_create) returned NULL");
        REAL.store(ptr, Ordering::Release);
    }
    let real: RealFn = core::mem::transmute(ptr);
    real(thread, attr, start, arg)
}
