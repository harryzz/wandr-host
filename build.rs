fn main() {
    let target_os   = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    // ── WasiDrawable C++ shim ────────────────────────────────────────────────
    // A small SkDrawable subclass with a *mutable* sk_sp<SkPicture> field so
    // child layers can swap their picture without invalidating parent
    // recordings that captured `drawDrawable(this)`. Headers are vendored
    // under host/vendor/skia-src/ at the skia-bindings 0.93.1 commit.
    //
    // We compile against vendored skia headers and let the linker resolve
    // SkDrawable / SkCanvas / SkPicture symbols against libskia.a, which
    // skia-bindings already pulls in.
    let skia_include = "vendor/skia-src";

    let mut cc = cc::Build::new();
    cc.cpp(true)
        .file("cpp/wasi_drawable.cpp")
        .include(skia_include)
        .flag_if_supported("-std=c++17")
        // MSVC spells the C++17 switch differently; flag_if_supported drops
        // whichever the active compiler rejects, so both gcc/clang and MSVC get
        // C++17 (the Windows desktop host builds the shim with MSVC).
        .flag_if_supported("/std:c++17")
        .flag_if_supported("-fno-exceptions")
        .flag_if_supported("-fno-rtti");

    if target_os == "android" {
        // Skia on Android is built with libc++. Match that.
        cc.cpp_set_stdlib(Some("c++"));
        // Skia uses these macros for Android builds; mismatching can cause
        // ABI differences in inline methods.
        cc.define("SK_BUILD_FOR_ANDROID", None);

        // cc-rs looks for `aarch64-linux-android-clang++` by default, but
        // NDK r23+ only ships versioned variants (e.g. android35-clang++).
        // Pick the API level via env or default to 35 (matches sysroot lib
        // dir below).
        let ndk = std::env::var("ANDROID_NDK_HOME")
            .or_else(|_| std::env::var("NDK_HOME"))
            .expect("ANDROID_NDK_HOME must be set when cross-compiling for Android");
        let api: u32 = std::env::var("ANDROID_PLATFORM")
            .ok()
            .and_then(|s| s.strip_prefix("android-").map(|x| x.to_string()).or(Some(s)))
            .and_then(|s| s.parse().ok())
            .unwrap_or(35);
        let triple = match target_arch.as_str() {
            "aarch64" => "aarch64-linux-android",
            "x86_64"  => "x86_64-linux-android",
            other     => panic!("unsupported Android arch: {other}"),
        };
        let toolchain_bin = format!(
            "{ndk}/toolchains/llvm/prebuilt/linux-x86_64/bin"
        );
        let cxx = format!("{toolchain_bin}/{triple}{api}-clang++");
        let ar  = format!("{toolchain_bin}/llvm-ar");
        cc.compiler(&cxx);
        cc.archiver(&ar);
    }

    cc.compile("wasi_drawable");

    println!("cargo:rerun-if-changed=cpp/wasi_drawable.cpp");
    println!("cargo:rerun-if-changed=cpp/wasi_drawable.h");

    // ── Android sysroot link config + AIDL codegen ───────────────────────────
    // Gated to a non-Windows *host*: build scripts compile for the host, and the
    // `rsbinder_aidl` codegen crate (a non-Windows build-dep) is only present
    // then. Android is always cross-compiled from a Linux host, so this stays for
    // both linux-desktop and android targets; on a Windows host it compiles out
    // (Windows never targets Android, and the shim above already built).
    #[cfg(not(windows))]
    if target_os == "android" {
        let ndk = std::env::var("ANDROID_NDK_HOME")
            .or_else(|_| std::env::var("NDK_HOME"))
            .expect("ANDROID_NDK_HOME must be set when cross-compiling for Android");

        let api = 35;
        let triple = match target_arch.as_str() {
            "aarch64" => "aarch64-linux-android",
            "x86_64"  => "x86_64-linux-android",
            other     => panic!("unsupported Android arch: {other}"),
        };
        let sysroot_lib = format!(
            "{ndk}/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/{triple}/{api}"
        );

        println!("cargo:rustc-link-search={sysroot_lib}");
        println!("cargo:rustc-link-lib=EGL");
        println!("cargo:rustc-link-lib=android");
        println!("cargo:rustc-link-lib=log");
        println!("cargo:rustc-link-lib=GLESv2");
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=binder_ndk");
        // Task 93 — `--probe-video` (camera → HW VP8 encode) via the NDK C APIs.
        // These NDK libs are themselves the binder clients to cameraserver /
        // media.codec, so the camera/codec path needs no AIDL vendoring.
        println!("cargo:rustc-link-lib=camera2ndk");
        println!("cargo:rustc-link-lib=mediandk");

        // ── rsbinder-aidl codegen for vendored AOSP HALs ─────────────────────
        // Vendored under vendor/aosp-hardware-interfaces/ as a shallow
        // submodule pinned to android-15.0.0_r36. Sparse-checkout limits
        // the working tree to vibrator/aidl, light/aidl, power/aidl, and
        // thermal/aidl (~700 KB). r36 is required for the stable AIDL
        // thermal HAL — earlier versions don't have it.
        use std::path::PathBuf;
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
        let vendor = PathBuf::from("vendor/aosp-hardware-interfaces");
        let vibrator_aidl = vendor.join("vibrator/aidl");
        let light_aidl    = vendor.join("light/aidl");
        let power_aidl    = vendor.join("power/aidl");
        let thermal_aidl  = vendor.join("thermal/aidl");
        let sensors_aidl  = vendor.join("sensors/aidl");
        let fmq_aidl      = vendor.join("common/fmq/aidl");
        let common_aidl   = vendor.join("common/aidl");
        // Frameworks-layer AIDL — separate submodule because it's a
        // different AOSP repo. Used by task 20 (ISensorManager etc).
        let fwk_vendor    = PathBuf::from("vendor/aosp-frameworks-hardware-interfaces");
        let sensorsvc_aidl = fwk_vendor.join("sensorservice/aidl");

        // Upstream IDirectReportChannel.aidl references
        // android.hardware.sensors.ISensors.RateLevel — a nested AIDL
        // enum that rsbinder-aidl 0.7.0 can't resolve. We don't use
        // direct channels in the WIT, so replace the file in place with
        // a body-less interface. Runs every build; cheap; self-healing
        // across `git submodule update`.
        let direct_channel_path = sensorsvc_aidl.join(
            "android/frameworks/sensorservice/IDirectReportChannel.aidl"
        );
        let direct_channel_stub = b"\
// Auto-patched by wandr-host/build.rs because the real definition
// references android.hardware.sensors.ISensors.RateLevel which
// rsbinder-aidl 0.7.0 doesn't resolve. We don't use direct channels.
package android.frameworks.sensorservice;
interface IDirectReportChannel {}
";
        std::fs::write(&direct_channel_path, direct_channel_stub)
            .expect("patch IDirectReportChannel.aidl");
        // Framework-side AIDL types that hardware/interfaces depends on but
        // we don't vendor (the real ones live in frameworks/base). We provide
        // empty `parcelable Foo;` stubs that satisfy the import resolver but
        // are never actually constructed because the methods that use them
        // are not called from our host.
        let stubs = PathBuf::from("vendor/aidl-stubs");

        // ── AAudio AIDL (task 21) ────────────────────────────────────────
        // IAAudioService + supporting parcelables for PCM playback over the
        // `media.aaudio` binder service. The audio/common types
        // (AudioFormatDescription, AudioFormatType, PcmType, ...) live in a
        // separate AOSP repo (system/hardware/interfaces) — that vendor is
        // pinned to android-15.0.0_r36 alongside the others.
        let aaudio_av  = PathBuf::from("vendor/aosp-frameworks-av");
        let aaudio_aidl = aaudio_av.join("media/libaaudio/src/binding/aidl");
        let shmem_aidl  = aaudio_av.join("media/libshmem/aidl");
        // Framework (libaudioclient, native audioserver) AIDL — resolves the
        // `android.media.AudioPortFw` closure for IAudioPolicyService.listAudioPorts
        // (task 76 #6). Listed as an include AFTER `stubs` so the stub's
        // IAudioPolicyService / AudioPolicy* win for those 3 shared FQNs;
        // AudioPortFw & friends (11 framework types, no name clash) resolve here.
        let audioclient_aidl = aaudio_av.join("media/libaudioclient/aidl");
        // frameworks-av/aidl/ — com.android.media.permission.* +
        // VolumeShaper/Interpolator/MicrophoneInfoFw, the rest of the real
        // IAudioPolicyService type closure (task 76 #6).
        let av_aidl = aaudio_av.join("aidl");
        let audio_common_aidl = PathBuf::from(
            "vendor/aosp-system-hardware-interfaces/media/aidl"
        );

        // ── IInputMethodManager AIDL (task 40 sessions 2-3) ──────────────
        // The IMMS proxy lives at the binder name `input_method`
        // exposing descriptor `com.android.internal.view.IInputMethodManager`
        // (the AIDL file's `interface` name + `package` line). The real
        // interface has 37 methods + 15 transitive AIDL imports
        // (InputBindResult, EditorInfo, ImeTracker.Token, ResultReceiver,
        // ...) most of which live in com.android.internal.inputmethod and
        // need their own parcelable vendoring.
        //
        // Methods we use are kept at their real signature so transaction
        // codes match the IMMS dispatch table; everything else is
        // replaced with `void slot_NN_<orig-name>()` placeholders to
        // avoid pulling in transitive types we don't need yet:
        //
        //   - addClient (pos 0, session 3) — un-stubbed: needs
        //     IInputMethodClient + IRemoteInputConnection (also stubbed
        //     below to minimal interfaces; we serve them as Bn-side
        //     binder receivers).
        //   - isImeTraceEnabled (pos 25, session 2) — un-stubbed: read-
        //     only, no args, no permission, returns bool.
        //
        // Self-heals on every build, survives `git submodule update`.
        // Sessions 4-5 will un-stub more methods incrementally
        // (startInputOrWindowGainedFocus, showSoftInput, ...) and vendor
        // the supporting parcelables.
        let imm_vendor = PathBuf::from("vendor/aosp-frameworks-base");
        let imm_aidl_dir = imm_vendor.join("core/java");
        let imm_aidl_path = imm_aidl_dir.join("com/android/internal/view/IInputMethodManager.aidl");
        let imm_aidl_stub = b"\
// Auto-patched by wandr-host/build.rs (task 40 sessions 2-5). See
// build.rs comment block for the policy. Real methods kept at their
// real signatures so transaction codes match IMMS's dispatch:
//   - addClient (pos 0, session 3)
//   - showSoftInput (pos 8, session 5)
//   - startInputOrWindowGainedFocus (pos 11, session 4)
//   - isImeTraceEnabled (pos 25, session 2)
// Everything else is a no-import slot_NN_<orig-name>() placeholder.
package com.android.internal.view;

import android.os.ResultReceiver;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.ImeTracker;
import android.window.ImeOnBackInvokedDispatcher;
import com.android.internal.inputmethod.IInputMethodClient;
import com.android.internal.inputmethod.InputBindResult;
import com.android.internal.inputmethod.IRemoteAccessibilityInputConnection;
import com.android.internal.inputmethod.IRemoteInputConnection;

interface IInputMethodManager {
    void addClient(in IInputMethodClient client, in IRemoteInputConnection inputmethod,
            int untrustedDisplayId);
    void slot_01_getCurrentInputMethodInfoAsUser();
    void slot_02_getInputMethodList();
    void slot_03_getEnabledInputMethodList();
    void slot_04_getInputMethodListLegacy();
    void slot_05_getEnabledInputMethodListLegacy();
    void slot_06_getEnabledInputMethodSubtypeList();
    void slot_07_getLastInputMethodSubtype();
    boolean showSoftInput(in IInputMethodClient client, @nullable IBinder windowToken,
            in ImeTracker statsToken, int flags, int lastClickToolType,
            in @nullable ResultReceiver resultReceiver, int reason, boolean async);
    void slot_09_hideSoftInput();
    void slot_10_hideSoftInputFromServerForTest();
    InputBindResult startInputOrWindowGainedFocus(
            int startInputReason,
            in IInputMethodClient client, in @nullable IBinder windowToken,
            int startInputFlags,
            int softInputMode,
            int windowFlags,
            in @nullable EditorInfo editorInfo, in @nullable IRemoteInputConnection inputConnection,
            in @nullable IRemoteAccessibilityInputConnection remoteAccessibilityInputConnection,
            int unverifiedTargetSdkVersion, int userId,
            in ImeOnBackInvokedDispatcher imeDispatcher);
    void slot_12_startInputOrWindowGainedFocusAsync();
    void slot_13_showInputMethodPickerFromClient();
    void slot_14_showInputMethodPickerFromSystem();
    void slot_15_isInputMethodPickerShownForTest();
    void slot_16_onImeSwitchButtonClickFromSystem();
    void slot_17_getCurrentInputMethodSubtype();
    void slot_18_setAdditionalInputMethodSubtypes();
    void slot_19_setExplicitlyEnabledInputMethodSubtypes();
    void slot_20_getInputMethodWindowVisibleHeight();
    void slot_21_reportPerceptibleAsync();
    void slot_22_removeImeSurface();
    void slot_23_removeImeSurfaceFromWindowAsync();
    void slot_24_startProtoDump();
    boolean isImeTraceEnabled();
}
";
        std::fs::write(&imm_aidl_path, imm_aidl_stub)
            .expect("patch IInputMethodManager.aidl");

        // ── IInputMethodClient AIDL (task 40 session 3) ──────────────────
        // We SERVE this — IMMS calls us back on the client binder we
        // pass to addClient. The real interface has 12 oneway methods
        // with transitive imports (InputBindResult, ImeTracker.Token).
        // We don't need those during the addClient probe — IMMS may
        // synchronously fire oneway state-set calls (setActive,
        // setInteractive) but they're fire-and-forget. Stub the 12
        // method positions as void no-arg slot_NN_<orig-name>() so the
        // Bn-side server we generate logs the dispatch and returns Ok,
        // and doesn't need real parcel layouts.
        let imc_aidl_path = imm_aidl_dir.join("com/android/internal/inputmethod/IInputMethodClient.aidl");
        let imc_aidl_stub = b"\
// Auto-patched by wandr-host/build.rs (task 40 session 3). Real interface
// is `oneway` with 12 methods that take InputBindResult / ImeTracker.Token /
// ints; we stub each as void no-arg to avoid vendoring transitive
// parcelables. Method positions preserved so IMMS's transaction codes
// dispatch correctly into our Bn server (which just logs and drops).
package com.android.internal.inputmethod;

oneway interface IInputMethodClient {
    void slot_00_onBindMethod();
    void slot_01_onStartInputResult();
    void slot_02_onBindAccessibilityService();
    void slot_03_onUnbindMethod();
    void slot_04_onUnbindAccessibilityService();
    void slot_05_setActive();
    void slot_06_setInteractive();
    void slot_07_setImeVisibility();
    void slot_08_scheduleStartInputIfNecessary();
    void slot_09_reportFullscreenMode();
    void slot_10_setImeTraceEnabled();
    void slot_11_throwExceptionFromSystem();
}
";
        std::fs::write(&imc_aidl_path, imc_aidl_stub)
            .expect("patch IInputMethodClient.aidl");

        // ── IRemoteInputConnection AIDL (task 40 session 3) ──────────────
        // Same story as IInputMethodClient — we SERVE this, IMMS holds
        // the binder for later. During addClient, IMMS does NOT call any
        // methods on this binder (it's used later when the IME asks for
        // editor text). Real interface has ~36 oneway methods with a
        // very heavy import surface (AndroidFuture, RectF, KeyEvent,
        // ParcelableHandwritingGesture, ExtractedTextRequest, ...). For
        // session 3 we keep it a one-method empty stub — sessions 4-5
        // will need to un-stub real editor commands.
        let ric_aidl_path = imm_aidl_dir.join("com/android/internal/inputmethod/IRemoteInputConnection.aidl");
        let ric_aidl_stub = b"\
// Auto-patched by wandr-host/build.rs (task 40 session 3). Real interface
// has ~36 oneway methods with very heavy transitive imports
// (AndroidFuture, RectF, KeyEvent, ParcelableHandwritingGesture, ...).
// addClient doesn't synchronously call any of them; we just need a
// valid binder to register. Sessions 4+ will un-stub real editor
// commands as needed.
package com.android.internal.inputmethod;

oneway interface IRemoteInputConnection {
    void slot_00_placeholder();
}
";
        std::fs::write(&ric_aidl_path, ric_aidl_stub)
            .expect("patch IRemoteInputConnection.aidl");

        // ── IRemoteAccessibilityInputConnection AIDL (task 40 session 4) ─
        // Same story as IRemoteInputConnection — stubbed Bn-side server,
        // passed as @nullable, only the binder identity matters during
        // startInputOrWindowGainedFocus.
        let raic_aidl_path = imm_aidl_dir.join("com/android/internal/inputmethod/IRemoteAccessibilityInputConnection.aidl");
        let raic_aidl_stub = b"\
// Auto-patched by wandr-host/build.rs (task 40 session 4). Real interface
// is oneway with ~10 methods importing KeyEvent / TextAttribute /
// AndroidFuture. We pass @nullable IRemoteAccessibilityInputConnection
// as null in startInputOrWindowGainedFocus -- stub is here only so
// the IMM AIDL `import` resolves.
package com.android.internal.inputmethod;

oneway interface IRemoteAccessibilityInputConnection {
    void slot_00_placeholder();
}
";
        std::fs::write(&raic_aidl_path, raic_aidl_stub)
            .expect("patch IRemoteAccessibilityInputConnection.aidl");

        // EditorInfo, InputBindResult, ImeOnBackInvokedDispatcher are
        // already forward-declared `parcelable Foo;` in upstream — no
        // patch needed. rsbinder-aidl generates empty Rust structs for
        // them. We pass null for EditorInfo (the call accepts @nullable),
        // and the empty ImeOnBackInvokedDispatcher serializes to a
        // non-null marker + 0 field bytes — IMMS's readFromParcel
        // either accepts defaults or fails with a clean Status; either
        // is acceptable session-4 signal. The InputBindResult return
        // type either parses to an empty struct (with leftover wire
        // bytes ignored) or fails with a parse error — both prove the
        // call landed at IMMS.

        // ── ImeTracker.aidl rename (task 40 session 5) ────────────────────
        // The upstream file declares `parcelable ImeTracker.Token;` — the
        // dot is Java nested-class syntax. rsbinder-aidl 0.7.0 emits the
        // name verbatim (`pub mod ImeTracker.Token` etc.) which is invalid
        // Rust. We sidestep by replacing the file with a flat-name stub
        // and using `ImeTrackerToken` (no dot) in the IMM stub. The wire
        // format is the same — IMMS deserializes from its own Java
        // ImeTracker.Token class, which doesn't care what name we used
        // client-side. (The stub also serializes as zero payload bytes;
        // IMMS's readStrongBinder + readString8 see null/empty and
        // construct a Token with null binder + null tag — acceptable
        // for showSoftInput's stats path.)
        let ime_tracker_path = imm_aidl_dir.join("android/view/inputmethod/ImeTracker.aidl");
        let ime_tracker_stub = b"\
// Auto-patched by wandr-host/build.rs (task 40 session 5). Upstream is
// `parcelable ImeTracker.Token;` (Java nested-class syntax) which
// rsbinder-aidl 0.7.0 emits as invalid Rust (`pub mod ImeTracker.Token`,
// dot in identifier). We re-declare the type without the nested-class
// syntax (filename = parcelable name = `ImeTracker`); the IMM stub
// refers to it as `ImeTracker`. Wire format identical -- IMMS
// deserializes from its own Java ImeTracker.Token class, which doesn't
// inspect the client-side type name. The empty-parcelable stub still
// serializes as a non-null marker + 0 payload bytes; IMMS's
// readStrongBinder + readString8 see null/empty and construct a Token
// with null binder + null tag, which is acceptable for showSoftInput's
// stats-tracking arg (it only affects metrics, not the bind path).
package android.view.inputmethod;
parcelable ImeTracker;
";
        std::fs::write(&ime_tracker_path, ime_tracker_stub)
            .expect("patch ImeTracker.aidl");

        // ── ISurfaceComposer AIDL (task 22) ──────────────────────────────
        // SurfaceFlingerAIDL service ("android.gui.ISurfaceComposer").
        // Parcelables live in two sibling dirs: most under libs/gui/aidl/,
        // plus a handful (IWindowInfosListener/Publisher,
        // StalledTransactionInfo, WindowInfo, FocusRequest, ...) under
        // libs/gui/android/gui/. Both share package `android.gui` so we
        // include both. Zero imports leave the package. We only call
        // getPhysicalDisplayIds (read-only, no permission) for the §5
        // de-risk round-trip; the rest are emitted but unused.
        let surfaceflinger_aidl_main = PathBuf::from(
            "vendor/aosp-frameworks-native/libs/gui/aidl"
        );
        let surfaceflinger_aidl_extras = PathBuf::from(
            "vendor/aosp-frameworks-native/libs/gui"
        );

        // The upstream ISurfaceComposer.aidl is huge (100+ methods, many
        // referencing types backed by an external `gui_aidl_types_rs`
        // crate that we don't pull in, plus `IWindowInfosPublisher`
        // which lacks a `Default` impl). For the §5 de-risk we only call
        // getPhysicalDisplayIds — the 4th method (transaction code
        // FIRST_CALL_TRANSACTION + 3). Replace the file with a trimmed
        // version that preserves the first 4 method declarations (so
        // transaction codes match the service) and prunes the rest.
        // Self-heals on every build, survives `git submodule update`.
        let surface_composer_path = surfaceflinger_aidl_main
            .join("android/gui/ISurfaceComposer.aidl");
        let surface_composer_stub = b"\
// Auto-patched by wandr-host/build.rs to keep only the first 4 methods
// of android.gui.ISurfaceComposer (so getPhysicalDisplayIds remains at
// FIRST_CALL_TRANSACTION + 3, matching the SurfaceFlingerAIDL service's
// wire protocol). The upstream interface references types
// (IWindowInfosPublisher, WindowInfo via gui_aidl_types_rs) that
// rsbinder-aidl 0.7.0 doesn't resolve. We only call
// getPhysicalDisplayIds (read-only, no permission); the other three
// methods are kept as declarations to preserve transaction codes but
// are never invoked.
package android.gui;

interface ISurfaceComposer {
    void bootFinished();
    @nullable IBinder createConnection();
    void destroyVirtualDisplay(IBinder displayToken);
    long[] getPhysicalDisplayIds();
}
";
        std::fs::write(&surface_composer_path, surface_composer_stub)
            .expect("patch ISurfaceComposer.aidl");

        // ── WMS AIDLs (task 44 session 7) ────────────────────────────────
        // We talk to system_server's `window` service to register a real
        // WMS window from a non-Activity process (the prerequisite for
        // task 40 to summon Gboard — IMMS gates showSoftInput on
        // mCurFocusedWindowClient, which is fed by WMS, not by
        // InputDispatcher's focus tracking).
        //
        // IWindowManager has 154 methods with ~30 transitive imports.
        // Same self-healing pattern as IMM/ISurfaceComposer: rewrite
        // the .aidl in-place with only the methods we need un-stubbed,
        // keeping source order so transaction codes match WMS's
        // dispatch. Session 7 needs ONE method: openSession (pos 3).
        //
        // IWindowSession (42 methods) — fully slot-stubbed; session 7
        // only needs the Sp-side return value from openSession.
        // addToDisplay/remove get un-stubbed in session 8.
        //
        // IWindow (16 oneway methods) — slot-stubbed; we serve this
        // Bn-side starting session 8. Vendored now so IWindowSession's
        // method imports (which reference IWindow) forward-resolve.
        //
        // IWindowSessionCallback (1 oneway method, no imports) —
        // upstream is already minimal; passed through as-is. We serve
        // its Bn-side server during the openSession probe.
        //
        // WindowManager.aidl carries WindowManager.LayoutParams as a
        // Java nested-class parcelable (rsbinder-aidl can't handle the
        // dot in the identifier — same issue as ImeTracker.Token).
        // Session 7 doesn't construct LayoutParams; we just leave that
        // file out of the Builder source list. Session 9 will patch
        // it with a flat-name parcelable + hand-roll the wire format.
        let iwm_methods: &[&str] = &[
            "startViewServer", "stopViewServer", "isViewServerRunning",
            "openSession", "getInitialDisplaySize", "getBaseDisplaySize",
            "setForcedDisplaySize", "clearForcedDisplaySize",
            "getInitialDisplayDensity", "getBaseDisplayDensity",
            "getDisplayIdByUniqueId", "setForcedDisplayDensityForUser",
            "clearForcedDisplayDensityForUser", "setForcedDisplayScalingMode",
            "setEventDispatching", "isWindowToken", "addWindowToken",
            "removeWindowToken", "setDisplayChangeWindowController",
            "addShellRoot", "setShellRootAccessibilityWindow",
            "overridePendingAppTransitionMultiThumbFuture",
            "overridePendingAppTransitionRemote", "endProlongedAnimations",
            "startFreezingScreen", "stopFreezingScreen", "disableKeyguard",
            "reenableKeyguard", "exitKeyguardSecurely", "isKeyguardLocked",
            "isKeyguardSecure", "dismissKeyguard",
            "addKeyguardLockedStateListener",
            "removeKeyguardLockedStateListener", "setSwitchingUser",
            "closeSystemDialogs", "getAnimationScale", "getAnimationScales",
            "setAnimationScale", "setAnimationScales",
            "getCurrentAnimatorScale", "setInTouchMode",
            "setInTouchModeOnAllDisplays", "isInTouchMode",
            "showStrictModeViolation", "setStrictModeVisualIndicatorPreference",
            "refreshScreenCaptureDisabled", "getDefaultDisplayRotation",
            "getDisplayUserRotation", "watchRotation", "removeRotationWatcher",
            "registerProposedRotationListener",
            "getPreferredOptionsPanelGravity", "freezeRotation",
            "thawRotation", "isRotationFrozen", "freezeDisplayRotation",
            "thawDisplayRotation", "isDisplayRotationFrozen",
            "setFixedToUserRotation", "setIgnoreOrientationRequest",
            "screenshotWallpaper", "mirrorWallpaperSurface",
            "registerWallpaperVisibilityListener",
            "unregisterWallpaperVisibilityListener",
            "registerSystemGestureExclusionListener",
            "unregisterSystemGestureExclusionListener",
            "requestAssistScreenshot", "hideTransientBars",
            "setRecentsVisibility", "updateStaticPrivacyIndicatorBounds",
            "setNavBarVirtualKeyHapticFeedbackEnabled", "hasNavigationBar",
            "lockNow", "isSafeModeEnabled", "clearWindowContentFrameStats",
            "getWindowContentFrameStats", "getDockedStackSide",
            "registerPinnedTaskListener", "requestAppKeyboardShortcuts",
            "requestImeKeyboardShortcuts", "getStableInsets",
            "registerShortcutKey", "createInputConsumer",
            "destroyInputConsumer", "getCurrentImeTouchRegion",
            "registerDisplayFoldListener", "unregisterDisplayFoldListener",
            "registerDisplayWindowListener", "unregisterDisplayWindowListener",
            "startWindowTrace", "stopWindowTrace", "saveWindowTraceToFile",
            "isWindowTraceEnabled", "startTransitionTrace",
            "stopTransitionTrace", "isTransitionTraceEnabled",
            "getWindowingMode", "setWindowingMode", "getRemoveContentMode",
            "setRemoveContentMode", "shouldShowWithInsecureKeyguard",
            "setShouldShowWithInsecureKeyguard", "shouldShowSystemDecors",
            "setShouldShowSystemDecors", "getDisplayImePolicy",
            "setDisplayImePolicy", "syncInputTransactions", "isLayerTracing",
            "setLayerTracing", "mirrorDisplay",
            "setDisplayWindowInsetsController",
            "updateDisplayWindowRequestedVisibleTypes", "getWindowInsets",
            "getPossibleDisplayInfo", "showGlobalActions",
            "setLayerTracingFlags", "setActiveTransactionTracing",
            "requestScrollCapture", "holdLock",
            "getSupportedDisplayHashAlgorithms", "verifyDisplayHash",
            "setDisplayHashThrottlingEnabled",
            "attachWindowContextToDisplayArea",
            "attachWindowContextToWindowToken",
            "attachWindowContextToDisplayContent", "detachWindowContext",
            "reparentWindowContextToDisplayArea",
            "registerCrossWindowBlurEnabledListener",
            "unregisterCrossWindowBlurEnabledListener",
            "isTaskSnapshotSupported", "getImeDisplayId",
            "setTaskSnapshotEnabled", "registerTaskFpsCallback",
            "unregisterTaskFpsCallback", "snapshotTaskForRecents",
            "setRecentsAppBehindSystemBars",
            "getLetterboxBackgroundColorInArgb",
            "isLetterboxBackgroundMultiColored", "captureDisplay",
            "isGlobalKey", "addToSurfaceSyncGroup",
            "markSurfaceSyncGroupReady", "notifyScreenshotListeners",
            "replaceContentOnDisplay", "registerDecorViewGestureListener",
            "unregisterDecorViewGestureListener",
            "registerTrustedPresentationListener",
            "unregisterTrustedPresentationListener",
            "registerScreenRecordingCallback",
            "unregisterScreenRecordingCallback", "setGlobalDragListener",
            "transferTouchGesture",
            "getApplicationLaunchKeyboardShortcuts",
        ];
        assert_eq!(iwm_methods.len(), 154, "IWindowManager method count drift");
        assert_eq!(iwm_methods[3], "openSession", "openSession must be at pos 3");

        let mut iwm_body = String::from(
            "// Auto-patched by wandr-host/build.rs (task 44 session 7).\n\
             // Source order preserved -- WMS dispatches by transaction code.\n\
             // Only openSession (pos 3) un-stubbed.\n\
             package android.view;\n\
             \n\
             import android.view.IWindowSession;\n\
             import android.view.IWindowSessionCallback;\n\
             \n\
             interface IWindowManager {\n",
        );
        for (i, name) in iwm_methods.iter().enumerate() {
            if i == 3 {
                iwm_body.push_str(
                    "    IWindowSession openSession(in IWindowSessionCallback callback);\n",
                );
            } else {
                iwm_body.push_str(&format!("    void slot_{:03}_{}();\n", i, name));
            }
        }
        iwm_body.push_str("}\n");
        let iwm_aidl_path = imm_aidl_dir.join("android/view/IWindowManager.aidl");
        std::fs::write(&iwm_aidl_path, iwm_body)
            .expect("patch IWindowManager.aidl");

        // IWindowSession — 42 methods, all slot-stubbed for session 7.
        // Session 8 un-stubs addToDisplay (pos 0) + remove (pos 3).
        let iws_methods: &[&str] = &[
            "addToDisplay", "addToDisplayAsUser",
            "addToDisplayWithoutInputChannel", "remove", "relayout",
            "relayoutAsync", "outOfMemory", "setInsets", "finishDrawing",
            "performDrag", "dropForAccessibility", "reportDropResult",
            "cancelDragAndDrop", "dragRecipientEntered",
            "dragRecipientExited", "setWallpaperPosition",
            "setWallpaperZoomOut", "setShouldZoomOutWallpaper",
            "wallpaperOffsetsComplete", "setWallpaperDisplayOffset",
            "sendWallpaperCommand", "wallpaperCommandComplete",
            "onRectangleOnScreenRequested", "getWindowId", "pokeDrawLock",
            "startMovingTask", "finishMovingTask", "updateTapExcludeRegion",
            "updateRequestedVisibleTypes",
            "reportSystemGestureExclusionChanged",
            "reportDecorViewGestureInterceptionChanged",
            "reportKeepClearAreasChanged", "grantInputChannel",
            "updateInputChannel", "grantEmbeddedWindowFocus",
            "generateDisplayHash", "setOnBackInvokedCallbackInfo",
            "clearTouchableRegion", "cancelDraw",
            "moveFocusToAdjacentWindow",
            "notifyImeWindowVisibilityChangedFromClient",
            "notifyInsetsAnimationRunningStateChanged",
        ];
        assert_eq!(iws_methods.len(), 42, "IWindowSession method count drift");
        let mut iws_body = String::from(
            "// Auto-patched by wandr-host/build.rs (task 44 session 7).\n\
             // Source order preserved. All methods slot-stubbed; session 8\n\
             // un-stubs addToDisplay + remove.\n\
             package android.view;\n\
             \n\
             interface IWindowSession {\n",
        );
        for (i, name) in iws_methods.iter().enumerate() {
            iws_body.push_str(&format!("    void slot_{:03}_{}();\n", i, name));
        }
        iws_body.push_str("}\n");
        let iws_aidl_path = imm_aidl_dir.join("android/view/IWindowSession.aidl");
        std::fs::write(&iws_aidl_path, iws_body)
            .expect("patch IWindowSession.aidl");

        // IWindow — 16 oneway methods, all slot-stubbed. We serve this
        // Bn-side starting session 8 (WMS calls back on resize/inset/
        // focus changes); session 7 doesn't need it, but it's an
        // import of WindowManager-adjacent AIDLs so we vendor it now.
        let iw_methods: &[&str] = &[
            "executeCommand", "resized", "insetsControlChanged",
            "showInsets", "hideInsets", "moved", "dispatchAppVisibility",
            "dispatchGetNewSurface", "closeSystemDialogs",
            "dispatchWallpaperOffsets", "dispatchWallpaperCommand",
            "dispatchDragEvent", "dispatchWindowShown",
            "requestAppKeyboardShortcuts", "requestScrollCapture",
            "dumpWindow",
        ];
        assert_eq!(iw_methods.len(), 16, "IWindow method count drift");
        let mut iw_body = String::from(
            "// Auto-patched by wandr-host/build.rs (task 44 session 7).\n\
             // Source order preserved. All methods slot-stubbed; session 8\n\
             // un-stubs the ones WMS actually calls back on after addToDisplay.\n\
             package android.view;\n\
             \n\
             oneway interface IWindow {\n",
        );
        for (i, name) in iw_methods.iter().enumerate() {
            iw_body.push_str(&format!("    void slot_{:03}_{}();\n", i, name));
        }
        iw_body.push_str("}\n");
        let iw_aidl_path = imm_aidl_dir.join("android/view/IWindow.aidl");
        std::fs::write(&iw_aidl_path, iw_body)
            .expect("patch IWindow.aidl");

        // IWindowSessionCallback is already minimal upstream (1 oneway
        // method, no imports). Used as-is — no patch needed.

        // ── HeadTracking.aidl float-default patch (task 76 #6) ───────────────
        // The real IAudioPolicyService pulls HeadTracking (via the spatializer
        // methods) whose `float[6] headToStage = {0f, 0f, 0f, 0f, 0f, 0f}`
        // default trips rsbinder-aidl 0.9.0's expression parser (the `0f` float
        // literal). We never call the spatializer; strip the default so the file
        // parses. In-place + idempotent → self-heals on submodule update (same
        // pattern as the IMM/WMS stubs), keeps the submodule pristine.
        {
            let ht = audio_common_aidl.join("android/media/audio/common/HeadTracking.aidl");
            if let Ok(s) = std::fs::read_to_string(&ht) {
                if s.contains("headToStage = {0f") {
                    let patched = s.replace(
                        "float[6] headToStage = {0f, 0f, 0f, 0f, 0f, 0f};",
                        "float[6] headToStage;");
                    std::fs::write(&ht, patched).expect("patch HeadTracking.aidl");
                }
            }
        }

        // Pass only the interface .aidl files; parcelables/enums in the same
        // package are resolved automatically via include_dir. Passing the full
        // dir causes the package modules to be re-emitted once per file (~3×).
        rsbinder_aidl::Builder::new()
            .source(vibrator_aidl.join("android/hardware/vibrator/IVibrator.aidl"))
            .source(light_aidl.join("android/hardware/light/ILights.aidl"))
            .source(power_aidl.join("android/hardware/power/IPower.aidl"))
            .source(thermal_aidl.join("android/hardware/thermal/IThermal.aidl"))
            // Sensors (ISensorManager closure) moved to the shared `wandr-hal-sensors`
            // crate (task 77) — codegen'd there now, consumed by both host + arbiter.
            .source(aaudio_aidl.join("aaudio/IAAudioService.aidl"))
            .source(aaudio_aidl.join("aaudio/IAAudioClient.aidl"))
            // Call-audio control (Signal VoIP / future Phone app): the REAL
            // libaudioclient IAudioPolicyService — all 106 methods, codegen-
            // derived (correct) transaction indices, full type closure resolves
            // from the vendored AIDLs (audioclient + audio-common + av/aidl).
            // Replaced the brittle hand-maintained positional slot-stub once
            // rsbinder 0.9.0 could decode AudioPortFw (task 76 #6).
            .source(audioclient_aidl.join("android/media/IAudioPolicyService.aidl"))
            .source(surfaceflinger_aidl_main.join("android/gui/ISurfaceComposer.aidl"))
            .source(imm_aidl_dir.join("android/view/IWindowManager.aidl"))
            .source(imm_aidl_dir.join("android/view/IWindowSession.aidl"))
            .source(imm_aidl_dir.join("android/view/IWindow.aidl"))
            .source(imm_aidl_dir.join("android/view/IWindowSessionCallback.aidl"))
            .source(imm_aidl_path.clone())
            .include_dir(vibrator_aidl.clone())
            .include_dir(light_aidl.clone())
            .include_dir(power_aidl.clone())
            .include_dir(thermal_aidl.clone())
            .include_dir(sensors_aidl.clone())
            .include_dir(sensorsvc_aidl.clone())
            .include_dir(fmq_aidl.clone())
            .include_dir(common_aidl.clone())
            .include_dir(aaudio_aidl.clone())
            .include_dir(shmem_aidl.clone())
            .include_dir(audio_common_aidl.clone())
            .include_dir(surfaceflinger_aidl_main.clone())
            .include_dir(surfaceflinger_aidl_extras.clone())
            .include_dir(imm_aidl_dir.clone())
            .include_dir(stubs.clone())
            // Real IAudioPolicyService closure (task 76 #6): libaudioclient/aidl
            // (AudioPortFw, AudioPolicyForceUse, …) + frameworks-av/aidl
            // (com.android.media.permission.*, VolumeShaper, MicrophoneInfoFw).
            .include_dir(audioclient_aidl.clone())
            .include_dir(av_aidl.clone())
            .set_async_support(true)
            .output(PathBuf::from(&out_dir).join("aosp_hal_bindings.rs"))
            .generate()
            .expect("rsbinder-aidl codegen failed");

        println!("cargo:rerun-if-changed={}", vibrator_aidl.display());
        println!("cargo:rerun-if-changed={}", light_aidl.display());
        println!("cargo:rerun-if-changed={}", power_aidl.display());
        println!("cargo:rerun-if-changed={}", thermal_aidl.display());
        println!("cargo:rerun-if-changed={}", sensors_aidl.display());
        println!("cargo:rerun-if-changed={}", sensorsvc_aidl.display());
        println!("cargo:rerun-if-changed={}", fmq_aidl.display());
        println!("cargo:rerun-if-changed={}", common_aidl.display());
        println!("cargo:rerun-if-changed={}", aaudio_aidl.display());
        println!("cargo:rerun-if-changed={}", shmem_aidl.display());
        println!("cargo:rerun-if-changed={}", audio_common_aidl.display());
        println!("cargo:rerun-if-changed={}", audioclient_aidl.display());
        println!("cargo:rerun-if-changed={}", av_aidl.display());
        println!("cargo:rerun-if-changed={}", surfaceflinger_aidl_main.display());
        println!("cargo:rerun-if-changed={}", surfaceflinger_aidl_extras.display());
        println!("cargo:rerun-if-changed={}", imm_aidl_dir.display());
        println!("cargo:rerun-if-changed={}", stubs.display());
    }

    println!("cargo:rerun-if-changed=build.rs");
}
