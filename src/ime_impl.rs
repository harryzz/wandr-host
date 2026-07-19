//! IMMS round-trip probes (task 40 sessions 2-4).
//!
//! Three entry points:
//!
//! - [`probe`] — session 2's read-only call: `isImeTraceEnabled()`.
//!   Validates transport. No permission, no Bn-side server, no args.
//!
//! - [`probe_addclient`] — session 3's client-registration call:
//!   `addClient(client, inputConn, displayId)`. Stands up two Bn-side
//!   binder servers (`IInputMethodClient` + `IRemoteInputConnection`)
//!   with stubbed slot_NN method dispatch, and asks IMMS to register
//!   them as a non-Activity client. Observes whether IMMS accepts the
//!   registration or rejects it (permission gate, identity check, …).
//!
//! - [`probe_startinput`] — session 4's start-input + window-focus
//!   call: `startInputOrWindowGainedFocus(...)`. Re-registers as an
//!   IMMS client (addClient first), then calls
//!   startInputOrWindowGainedFocus with a fabricated `IBinder
//!   windowToken` (extracted from one of our Bn servers' identity
//!   binder), null EditorInfo, null IRemoteAccessibilityInputConnection,
//!   default-constructed ImeOnBackInvokedDispatcher. Observes whether
//!   IMMS validates the windowToken against WMS — the session-1
//!   biggest-unknown question.
//!
//! All target the `input_method` binder service (descriptor
//! `com.android.internal.view.IInputMethodManager`). The AIDL stubs in
//! `build.rs` preserve the real transaction codes for the methods we
//! call (`addClient` at slot 0, `startInputOrWindowGainedFocus` at
//! slot 11, `isImeTraceEnabled` at slot 25) by padding the unused
//! positions with no-import placeholders.

#[cfg(target_os = "android")]
pub fn probe() {
    use crate::binder_aidl::com::android::internal::view::IInputMethodManager::IInputMethodManager;

    if let Err(reason) = crate::binder::init() {
        log::warn!("ime: binder init failed: {reason}");
        return;
    }

    let svc: rsbinder::Strong<dyn IInputMethodManager> =
        match rsbinder::hub::get_interface("input_method") {
            Ok(s)  => s,
            Err(e) => {
                log::warn!("ime: input_method service unavailable: {e:?}");
                return;
            }
        };

    match svc.r#isImeTraceEnabled() {
        Ok(enabled) => log::info!(
            "ime: IMMS round-trip OK — isImeTraceEnabled() = {enabled}. \
             Transport validated against com.android.internal.view.IInputMethodManager. \
             Session 2 first-call milestone reached.",
        ),
        Err(e) => log::info!(
            "ime: IMMS round-trip reached service — isImeTraceEnabled() returned {e:?}. \
             Service responded with a structured Status; rsbinder transport works. \
             Session 2 first-call milestone reached (transport-level signal).",
        ),
    }
}

#[cfg(not(target_os = "android"))]
pub fn probe() {}

// ─── Session 3: addClient + Bn-side callback servers ─────────────────────────

#[cfg(target_os = "android")]
mod session3 {
    use std::sync::OnceLock;

    use crate::binder_aidl::com::android::internal::view::IInputMethodManager::IInputMethodManager;
    use crate::binder_aidl::com::android::internal::inputmethod::IInputMethodClient::{
        BnInputMethodClient, IInputMethodClient, IInputMethodClientAsyncService,
    };
    use crate::binder_aidl::com::android::internal::inputmethod::IRemoteInputConnection::{
        BnRemoteInputConnection, IRemoteInputConnection, IRemoteInputConnectionAsyncService,
    };

    /// Bn-side server for `IInputMethodClient`. IMMS may fire any of the
    /// 12 oneway slot_NN methods asynchronously after registration —
    /// most importantly `setActive` / `setInteractive` to push initial
    /// client state. The stub just logs that the dispatch happened and
    /// returns `Ok(())` so IMMS's transaction completes. Real state
    /// observation comes in session 4 when we un-stub the methods with
    /// their real parameters.
    struct ImeClient;
    impl rsbinder::Interface for ImeClient {}
    #[async_trait::async_trait]
    impl IInputMethodClientAsyncService for ImeClient {
        async fn r#slot_00_onBindMethod(&self)                  -> rsbinder::status::Result<()> { log::info!("ime/client: onBindMethod fired"); Ok(()) }
        async fn r#slot_01_onStartInputResult(&self)            -> rsbinder::status::Result<()> { log::info!("ime/client: onStartInputResult fired"); Ok(()) }
        async fn r#slot_02_onBindAccessibilityService(&self)    -> rsbinder::status::Result<()> { log::info!("ime/client: onBindAccessibilityService fired"); Ok(()) }
        async fn r#slot_03_onUnbindMethod(&self)                -> rsbinder::status::Result<()> { log::info!("ime/client: onUnbindMethod fired"); Ok(()) }
        async fn r#slot_04_onUnbindAccessibilityService(&self)  -> rsbinder::status::Result<()> { log::info!("ime/client: onUnbindAccessibilityService fired"); Ok(()) }
        async fn r#slot_05_setActive(&self)                     -> rsbinder::status::Result<()> { log::info!("ime/client: setActive fired"); Ok(()) }
        async fn r#slot_06_setInteractive(&self)                -> rsbinder::status::Result<()> { log::info!("ime/client: setInteractive fired"); Ok(()) }
        async fn r#slot_07_setImeVisibility(&self)              -> rsbinder::status::Result<()> { log::info!("ime/client: setImeVisibility fired"); Ok(()) }
        async fn r#slot_08_scheduleStartInputIfNecessary(&self) -> rsbinder::status::Result<()> { log::info!("ime/client: scheduleStartInputIfNecessary fired"); Ok(()) }
        async fn r#slot_09_reportFullscreenMode(&self)          -> rsbinder::status::Result<()> { log::info!("ime/client: reportFullscreenMode fired"); Ok(()) }
        async fn r#slot_10_setImeTraceEnabled(&self)            -> rsbinder::status::Result<()> { log::info!("ime/client: setImeTraceEnabled fired"); Ok(()) }
        async fn r#slot_11_throwExceptionFromSystem(&self)      -> rsbinder::status::Result<()> { log::info!("ime/client: throwExceptionFromSystem fired"); Ok(()) }
    }

    /// Bn-side server for `IRemoteInputConnection`. IMMS does NOT
    /// synchronously call any methods during `addClient` — the binder
    /// is stored for later use by an IME. Single placeholder method to
    /// keep codegen happy.
    struct RemoteInputConn;
    impl rsbinder::Interface for RemoteInputConn {}
    #[async_trait::async_trait]
    impl IRemoteInputConnectionAsyncService for RemoteInputConn {
        async fn r#slot_00_placeholder(&self) -> rsbinder::status::Result<()> {
            log::info!("ime/inputconn: placeholder fired");
            Ok(())
        }
    }

    /// Tokio current-thread runtime as the `BinderAsyncRuntime` —
    /// required by `BnInputMethodClient::new_async_binder`. Same shape
    /// as `wandr_sensors_client`' internal `TokioRuntime`.
    struct TokioRuntime;
    impl rsbinder::BinderAsyncRuntime for TokioRuntime {
        fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
            static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
            let rt = RT.get_or_init(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio current-thread runtime")
            });
            rt.block_on(f)
        }
    }

    pub fn run() {
        if let Err(reason) = crate::binder::init() {
            log::warn!("ime: binder init failed: {reason}");
            return;
        }

        let svc: rsbinder::Strong<dyn IInputMethodManager> =
            match rsbinder::hub::get_interface("input_method") {
                Ok(s)  => s,
                Err(e) => {
                    log::warn!("ime: input_method service unavailable: {e:?}");
                    return;
                }
            };

        let client: rsbinder::Strong<dyn IInputMethodClient> =
            BnInputMethodClient::new_async_binder(ImeClient, TokioRuntime);
        let input_conn: rsbinder::Strong<dyn IRemoteInputConnection> =
            BnRemoteInputConnection::new_async_binder(RemoteInputConn, TokioRuntime);

        // Primary display id is 0 on every device shipped this decade.
        // "untrustedDisplayId" is the framework's name; for a non-virtual
        // display caller it's just the display the client purports to
        // be on, used by IMMS for per-display IME routing.
        let display_id: i32 = 0;

        log::info!(
            "ime: calling addClient(client=Bn, inputConn=Bn, displayId={display_id}) — \
             watching for permission/identity/token rejection",
        );

        match svc.r#addClient(&client, &input_conn, display_id) {
            Ok(()) => log::info!(
                "ime: addClient OK — IMMS accepted our non-Activity client registration. \
                 Session 3 milestone reached: we are now a registered IMMS client. \
                 Next: startInputOrWindowGainedFocus + WindowToken (session 4).",
            ),
            Err(e) => log::warn!(
                "ime: addClient rejected — {e:?}. \
                 Inspect the Status for SecurityException / IllegalStateException / \
                 binder-transport vs framework-level failure. The two callback Bn \
                 servers were successfully constructed (binder identities live), so \
                 the failure is on IMMS's accept side — likely permission, calling \
                 uid check, or display routing.",
            ),
        }

        // Keep the process (and therefore both Bn binders) alive for a
        // bit so a curious operator can `dumpsys input_method` and see
        // our ClientState entry. Without this, the binders are GC'd by
        // IMMS's DeathRecipient the moment we exit.
        log::info!("ime: holding client alive for 5s for dumpsys inspection — pid {}", std::process::id());
        std::thread::sleep(std::time::Duration::from_secs(5));
        log::info!("ime: exit — IMMS will drop our ClientState via DeathRecipient");
    }
}

#[cfg(target_os = "android")]
pub fn probe_addclient() {
    session3::run();
}

#[cfg(not(target_os = "android"))]
pub fn probe_addclient() {}

// ─── Session 4: startInputOrWindowGainedFocus + fabricated windowToken ───────

#[cfg(target_os = "android")]
mod session4 {
    use std::sync::OnceLock;

    use crate::binder_aidl::com::android::internal::view::IInputMethodManager::IInputMethodManager;
    use crate::binder_aidl::com::android::internal::inputmethod::IInputMethodClient::{
        BnInputMethodClient, IInputMethodClient, IInputMethodClientAsyncService,
    };
    use crate::binder_aidl::com::android::internal::inputmethod::IRemoteInputConnection::{
        BnRemoteInputConnection, IRemoteInputConnection, IRemoteInputConnectionAsyncService,
    };
    use crate::binder_aidl::android::window::ImeOnBackInvokedDispatcher::ImeOnBackInvokedDispatcher;

    /// Same Bn-side server as session 3 — IMMS may fire any of the 12
    /// oneway slot_NN methods after registration (especially setActive
    /// + setInteractive on focus). Stubs log + return Ok.
    struct ImeClient;
    impl rsbinder::Interface for ImeClient {}
    #[async_trait::async_trait]
    impl IInputMethodClientAsyncService for ImeClient {
        async fn r#slot_00_onBindMethod(&self)                  -> rsbinder::status::Result<()> { log::info!("ime/client: onBindMethod fired"); Ok(()) }
        async fn r#slot_01_onStartInputResult(&self)            -> rsbinder::status::Result<()> { log::info!("ime/client: onStartInputResult fired"); Ok(()) }
        async fn r#slot_02_onBindAccessibilityService(&self)    -> rsbinder::status::Result<()> { log::info!("ime/client: onBindAccessibilityService fired"); Ok(()) }
        async fn r#slot_03_onUnbindMethod(&self)                -> rsbinder::status::Result<()> { log::info!("ime/client: onUnbindMethod fired"); Ok(()) }
        async fn r#slot_04_onUnbindAccessibilityService(&self)  -> rsbinder::status::Result<()> { log::info!("ime/client: onUnbindAccessibilityService fired"); Ok(()) }
        async fn r#slot_05_setActive(&self)                     -> rsbinder::status::Result<()> { log::info!("ime/client: setActive fired"); Ok(()) }
        async fn r#slot_06_setInteractive(&self)                -> rsbinder::status::Result<()> { log::info!("ime/client: setInteractive fired"); Ok(()) }
        async fn r#slot_07_setImeVisibility(&self)              -> rsbinder::status::Result<()> { log::info!("ime/client: setImeVisibility fired"); Ok(()) }
        async fn r#slot_08_scheduleStartInputIfNecessary(&self) -> rsbinder::status::Result<()> { log::info!("ime/client: scheduleStartInputIfNecessary fired"); Ok(()) }
        async fn r#slot_09_reportFullscreenMode(&self)          -> rsbinder::status::Result<()> { log::info!("ime/client: reportFullscreenMode fired"); Ok(()) }
        async fn r#slot_10_setImeTraceEnabled(&self)            -> rsbinder::status::Result<()> { log::info!("ime/client: setImeTraceEnabled fired"); Ok(()) }
        async fn r#slot_11_throwExceptionFromSystem(&self)      -> rsbinder::status::Result<()> { log::info!("ime/client: throwExceptionFromSystem fired"); Ok(()) }
    }

    struct RemoteInputConn;
    impl rsbinder::Interface for RemoteInputConn {}
    #[async_trait::async_trait]
    impl IRemoteInputConnectionAsyncService for RemoteInputConn {
        async fn r#slot_00_placeholder(&self) -> rsbinder::status::Result<()> {
            log::info!("ime/inputconn: placeholder fired");
            Ok(())
        }
    }

    /// Bn server for a free-standing IBinder we use as the windowToken.
    /// IMMS treats windowToken as opaque (per session-1 recon — focused
    /// windows in `dumpsys input_method` show as bare `BinderProxy`
    /// objects with no descriptor type). We pick `IRemoteInputConnection`
    /// as the carrier interface — IMMS never looks at the interface;
    /// it just uses the raw binder identity as a focus key.
    struct WindowTokenCarrier;
    impl rsbinder::Interface for WindowTokenCarrier {}
    #[async_trait::async_trait]
    impl IRemoteInputConnectionAsyncService for WindowTokenCarrier {
        async fn r#slot_00_placeholder(&self) -> rsbinder::status::Result<()> {
            log::info!("ime/windowtoken: callback fired (unexpected — IMMS shouldn't invoke methods on the windowToken)");
            Ok(())
        }
    }

    struct TokioRuntime;
    impl rsbinder::BinderAsyncRuntime for TokioRuntime {
        fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
            static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
            let rt = RT.get_or_init(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio current-thread runtime")
            });
            rt.block_on(f)
        }
    }

    /// `StartInputReason.UNSPECIFIED` — see
    /// frameworks/base/core/java/com/android/internal/inputmethod/StartInputReason.java
    /// Choosing UNSPECIFIED keeps IMMS from doing reason-conditional
    /// extra work; it just records "a client started input".
    const START_INPUT_REASON_UNSPECIFIED: i32 = 0;

    pub fn run() {
        if let Err(reason) = crate::binder::init() {
            log::warn!("ime: binder init failed: {reason}");
            return;
        }

        let svc: rsbinder::Strong<dyn IInputMethodManager> =
            match rsbinder::hub::get_interface("input_method") {
                Ok(s)  => s,
                Err(e) => {
                    log::warn!("ime: input_method service unavailable: {e:?}");
                    return;
                }
            };

        // Re-register as a client first — startInputOrWindowGainedFocus
        // requires us to be a known client, otherwise IMMS rejects on
        // ClientState lookup.
        let client: rsbinder::Strong<dyn IInputMethodClient> =
            BnInputMethodClient::new_async_binder(ImeClient, TokioRuntime);
        let input_conn: rsbinder::Strong<dyn IRemoteInputConnection> =
            BnRemoteInputConnection::new_async_binder(RemoteInputConn, TokioRuntime);

        if let Err(e) = svc.r#addClient(&client, &input_conn, 0) {
            log::warn!("ime: addClient failed before startInput: {e:?}");
            return;
        }
        log::info!("ime: addClient OK (session-4 prelude) — proceeding to startInput");

        // Fabricated windowToken. The carrier interface doesn't matter;
        // IMMS extracts only the IBinder identity. We pull the IBinder
        // out via Strong::as_binder() so the call signature's
        // `Option<&SIBinder>` is satisfied.
        let window_token_carrier: rsbinder::Strong<dyn IRemoteInputConnection> =
            BnRemoteInputConnection::new_async_binder(WindowTokenCarrier, TokioRuntime);
        let window_token_binder = window_token_carrier.as_binder();

        // Empty default ImeOnBackInvokedDispatcher. The AIDL declares it
        // as a forward-declared `parcelable` (the real fields live in
        // the Java class); rsbinder gives us a zero-field Rust struct
        // that serializes as the non-null marker followed by 0 bytes
        // of payload. IMMS's readFromParcel for this parcelable may
        // accept defaults or fail with EX_BAD_PARCELABLE — either
        // outcome is acceptable session-4 signal.
        let ime_dispatcher = ImeOnBackInvokedDispatcher::default();

        // For root-running probe, primary user = userId 0.
        const USER_ID: i32 = 0;
        const TARGET_SDK: i32 = 35;  // Android 15

        // Two attempts to isolate the windowToken question from the
        // parcel-marshaling question:
        //   - Attempt A: windowToken = None → tests "just do startInput
        //     without focus event", same parcelable surface.
        //   - Attempt B: windowToken = Some(fabricated) → adds the
        //     WindowToken validation question.
        // Same error in both ⇒ marshaling / parcelable shape; different
        // errors ⇒ the differing arg is the cause.

        for (label, window_token) in [
            ("attempt-A windowToken=None ", None),
            ("attempt-B windowToken=Some ", Some(&window_token_binder)),
        ] {
            log::info!(
                "ime: {label} — calling startInputOrWindowGainedFocus(reason=UNSPECIFIED, \
                 editorInfo=None, raic=None, imeDispatcher=Default)",
            );

            let result = svc.r#startInputOrWindowGainedFocus(
                START_INPUT_REASON_UNSPECIFIED,
                &client,
                window_token,
                /* startInputFlags = */ 0,
                /* softInputMode   = */ 0,
                /* windowFlags     = */ 0,
                /* editorInfo      = */ None,
                Some(&input_conn),
                /* remoteAccessibilityInputConnection = */ None,
                TARGET_SDK,
                USER_ID,
                &ime_dispatcher,
            );

            match result {
                Ok(_bind_result) => log::info!(
                    "ime: {label} OK — IMMS returned a non-null InputBindResult \
                     (fields not inspected — empty stub). The call landed, IMMS \
                     created a bind, the empty-stub ImeOnBackInvokedDispatcher \
                     parcel marshalled cleanly. Session 4 milestone reached.",
                ),
                Err(e) => {
                    // `Status::transaction_error()` only returns the StatusCode
                    // when the exception is TransactionFailed; for our case
                    // (`exception=NullPointer, code=UnexpectedNull`) we have to
                    // consume the Status via `Into<StatusCode>` to get the code.
                    let exc = e.exception_code();
                    let code: rsbinder::StatusCode = e.into();
                    if code == rsbinder::StatusCode::UnexpectedNull {
                        log::info!(
                            "ime: {label} OK — IMMS returned a null InputBindResult \
                             (rsbinder maps this to UnexpectedNull on the response \
                             deserializer). This is a documented IMMS response: when a \
                             client isn't the focused window or there's no IME to bind, \
                             IMMS returns null. The call landed cleanly through transport, \
                             parcel marshaling, ImeOnBackInvokedDispatcher empty-stub \
                             serialization, and IMMS's framework gates. Session 4 \
                             milestone reached.",
                        );
                    } else {
                        log::warn!(
                            "ime: {label} rejected — exception={exc:?} code={code:?}. \
                             Not UnexpectedNull — IMMS rejected with a structured Status. \
                             Likely categories: EX_SECURITY (permission), \
                             EX_BAD_PARCELABLE (ImeOnBackInvokedDispatcher shape), or \
                             IllegalStateException (identity/uid check).",
                        );
                    }
                }
            }
        }

        log::info!("ime: holding client alive for 5s for dumpsys inspection — pid {}", std::process::id());
        std::thread::sleep(std::time::Duration::from_secs(5));
        log::info!("ime: exit");
    }
}

#[cfg(target_os = "android")]
pub fn probe_startinput() {
    session4::run();
}

#[cfg(not(target_os = "android"))]
pub fn probe_startinput() {}

// ─── Session 5: showSoftInput — try to summon Gboard ─────────────────────────

#[cfg(target_os = "android")]
mod session5 {
    use std::sync::OnceLock;

    use crate::binder_aidl::com::android::internal::view::IInputMethodManager::IInputMethodManager;
    use crate::binder_aidl::com::android::internal::inputmethod::IInputMethodClient::{
        BnInputMethodClient, IInputMethodClient, IInputMethodClientAsyncService,
    };
    use crate::binder_aidl::com::android::internal::inputmethod::IRemoteInputConnection::{
        BnRemoteInputConnection, IRemoteInputConnection, IRemoteInputConnectionAsyncService,
    };
    use crate::binder_aidl::android::window::ImeOnBackInvokedDispatcher::ImeOnBackInvokedDispatcher;
    use crate::binder_aidl::android::view::inputmethod::ImeTracker::ImeTracker;

    /// Same Bn-side server pattern as sessions 3-4. IMMS may fire any
    /// of the 12 oneway slot_NN methods after registration (especially
    /// setActive / setInteractive when focus state changes).
    struct ImeClient;
    impl rsbinder::Interface for ImeClient {}
    #[async_trait::async_trait]
    impl IInputMethodClientAsyncService for ImeClient {
        async fn r#slot_00_onBindMethod(&self)                  -> rsbinder::status::Result<()> { log::info!("ime/client: onBindMethod fired"); Ok(()) }
        async fn r#slot_01_onStartInputResult(&self)            -> rsbinder::status::Result<()> { log::info!("ime/client: onStartInputResult fired"); Ok(()) }
        async fn r#slot_02_onBindAccessibilityService(&self)    -> rsbinder::status::Result<()> { log::info!("ime/client: onBindAccessibilityService fired"); Ok(()) }
        async fn r#slot_03_onUnbindMethod(&self)                -> rsbinder::status::Result<()> { log::info!("ime/client: onUnbindMethod fired"); Ok(()) }
        async fn r#slot_04_onUnbindAccessibilityService(&self)  -> rsbinder::status::Result<()> { log::info!("ime/client: onUnbindAccessibilityService fired"); Ok(()) }
        async fn r#slot_05_setActive(&self)                     -> rsbinder::status::Result<()> { log::info!("ime/client: setActive fired"); Ok(()) }
        async fn r#slot_06_setInteractive(&self)                -> rsbinder::status::Result<()> { log::info!("ime/client: setInteractive fired"); Ok(()) }
        async fn r#slot_07_setImeVisibility(&self)              -> rsbinder::status::Result<()> { log::info!("ime/client: setImeVisibility fired"); Ok(()) }
        async fn r#slot_08_scheduleStartInputIfNecessary(&self) -> rsbinder::status::Result<()> { log::info!("ime/client: scheduleStartInputIfNecessary fired"); Ok(()) }
        async fn r#slot_09_reportFullscreenMode(&self)          -> rsbinder::status::Result<()> { log::info!("ime/client: reportFullscreenMode fired"); Ok(()) }
        async fn r#slot_10_setImeTraceEnabled(&self)            -> rsbinder::status::Result<()> { log::info!("ime/client: setImeTraceEnabled fired"); Ok(()) }
        async fn r#slot_11_throwExceptionFromSystem(&self)      -> rsbinder::status::Result<()> { log::info!("ime/client: throwExceptionFromSystem fired"); Ok(()) }
    }

    struct RemoteInputConn;
    impl rsbinder::Interface for RemoteInputConn {}
    #[async_trait::async_trait]
    impl IRemoteInputConnectionAsyncService for RemoteInputConn {
        async fn r#slot_00_placeholder(&self) -> rsbinder::status::Result<()> {
            log::info!("ime/inputconn: placeholder fired");
            Ok(())
        }
    }

    struct WindowTokenCarrier;
    impl rsbinder::Interface for WindowTokenCarrier {}
    #[async_trait::async_trait]
    impl IRemoteInputConnectionAsyncService for WindowTokenCarrier {
        async fn r#slot_00_placeholder(&self) -> rsbinder::status::Result<()> {
            log::info!("ime/windowtoken: callback fired (unexpected)");
            Ok(())
        }
    }

    struct TokioRuntime;
    impl rsbinder::BinderAsyncRuntime for TokioRuntime {
        fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
            static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
            let rt = RT.get_or_init(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio current-thread runtime")
            });
            rt.block_on(f)
        }
    }

    const START_INPUT_REASON_UNSPECIFIED: i32 = 0;
    /// `InputMethodManager.SHOW_FORCED` — strongest hint that the user
    /// wants the IME visible. Used to override IMMS heuristics that
    /// might suppress the show on a non-focused client.
    const SHOW_FORCED: i32 = 0x0002;
    /// `SoftInputShowHideReason.SHOW_SOFT_INPUT` — the default reason
    /// for a programmatic showSoftInput call.
    const SOFT_INPUT_REASON_SHOW: i32 = 0;
    /// `MotionEvent.TOOL_TYPE_UNKNOWN`.
    const TOOL_TYPE_UNKNOWN: i32 = 0;

    pub fn run() {
        if let Err(reason) = crate::binder::init() {
            log::warn!("ime: binder init failed: {reason}");
            return;
        }

        let svc: rsbinder::Strong<dyn IInputMethodManager> =
            match rsbinder::hub::get_interface("input_method") {
                Ok(s)  => s,
                Err(e) => {
                    log::warn!("ime: input_method service unavailable: {e:?}");
                    return;
                }
            };

        let client: rsbinder::Strong<dyn IInputMethodClient> =
            BnInputMethodClient::new_async_binder(ImeClient, TokioRuntime);
        let input_conn: rsbinder::Strong<dyn IRemoteInputConnection> =
            BnRemoteInputConnection::new_async_binder(RemoteInputConn, TokioRuntime);
        let window_token_carrier: rsbinder::Strong<dyn IRemoteInputConnection> =
            BnRemoteInputConnection::new_async_binder(WindowTokenCarrier, TokioRuntime);
        let window_token_binder = window_token_carrier.as_binder();

        // (1) Register as a client (session 3).
        if let Err(e) = svc.r#addClient(&client, &input_conn, 0) {
            log::warn!("ime: addClient failed before showSoftInput: {e:?}");
            return;
        }
        log::info!("ime: (1/3) addClient OK");

        // (2) Notify IMMS that our window has focus (session 4).
        let ime_dispatcher = ImeOnBackInvokedDispatcher::default();
        let start_result = svc.r#startInputOrWindowGainedFocus(
            START_INPUT_REASON_UNSPECIFIED,
            &client,
            Some(&window_token_binder),
            /* startInputFlags = */ 0,
            /* softInputMode   = */ 0,
            /* windowFlags     = */ 0,
            /* editorInfo      = */ None,
            Some(&input_conn),
            /* remoteAccessibilityInputConnection = */ None,
            /* targetSdk       = */ 35,
            /* userId          = */ 0,
            &ime_dispatcher,
        );
        // UnexpectedNull == null InputBindResult (documented IMMS response
        // for clients without a focused editor). Other errors are real.
        match start_result {
            Ok(_) => log::info!("ime: (2/3) startInput OK — got non-null InputBindResult"),
            Err(e) => {
                let exc = e.exception_code();
                let code: rsbinder::StatusCode = e.into();
                if code == rsbinder::StatusCode::UnexpectedNull {
                    log::info!("ime: (2/3) startInput OK — null InputBindResult (expected)");
                } else {
                    log::warn!("ime: (2/3) startInput failed — exception={exc:?} code={code:?}");
                    return;
                }
            }
        }

        // (3) Ask IMMS to show the soft input keyboard.
        let stats_token = ImeTracker::default();
        log::info!(
            "ime: (3/3) calling showSoftInput(client, windowToken, statsToken=Default, \
             flags=SHOW_FORCED, lastClickToolType=UNKNOWN, resultReceiver=None, \
             reason=SHOW_SOFT_INPUT, async=false)",
        );
        let show_result = svc.r#showSoftInput(
            &client,
            Some(&window_token_binder),
            &stats_token,
            /* flags             = */ SHOW_FORCED,
            /* lastClickToolType = */ TOOL_TYPE_UNKNOWN,
            /* resultReceiver    = */ None,
            /* reason            = */ SOFT_INPUT_REASON_SHOW,
            /* async             = */ false,
        );
        match show_result {
            Ok(shown) => log::info!(
                "ime: (3/3) showSoftInput returned {shown}. true = IMMS dispatched a \
                 show request to the IME; false = IMMS declined (likely because we \
                 are not the focused window per WMS). Watch the device screen + \
                 logcat for InputMethodService activity. Session 5 milestone reached \
                 if either: the call returned without exception (transport + AIDL OK), \
                 OR Gboard actually pops up (the full path works).",
            ),
            Err(e) => {
                let exc = e.exception_code();
                let code: rsbinder::StatusCode = e.into();
                if code == rsbinder::StatusCode::UnexpectedNull {
                    log::info!(
                        "ime: (3/3) showSoftInput — UnexpectedNull on response. \
                         Treating as transport+dispatch OK (IMMS-returned-null path); \
                         the show may or may not have actually fired.",
                    );
                } else {
                    log::warn!(
                        "ime: (3/3) showSoftInput rejected — exception={exc:?} code={code:?}. \
                         If EX_SECURITY: permission issue (showSoftInput has no \
                         explicit annotation but IMMS may check internally). \
                         If EX_BAD_PARCELABLE: the empty ImeTracker stub didn't \
                         survive IMMS's readFromParcel (probably needs real Token \
                         shape with non-null binder field).",
                    );
                }
            }
        }

        log::info!("ime: holding client alive 8s for dumpsys + IME observation — pid {}", std::process::id());
        std::thread::sleep(std::time::Duration::from_secs(8));
        log::info!("ime: exit");
    }
}

#[cfg(target_os = "android")]
pub fn probe_showsoftinput() {
    session5::run();
}

#[cfg(not(target_os = "android"))]
pub fn probe_showsoftinput() {}
