//! WMS round-trip probes (task 44 sessions 7+).
//!
//! Task 40 sessions 2-5 walked the IMMS binder protocol end-to-end but
//! Gboard still doesn't appear: IMMS gates `showSoftInput` on
//! `mCurFocusedWindowClient == callingClient`, and that field is fed by
//! WMS — not by InputDispatcher's focus tracking. Our standalone
//! wandr-host (task 33) registers an InputDispatcher window directly via
//! `IInputFlinger.createInputChannel` and shows up in `dumpsys input`,
//! but WMS never learns about it. The only path is to register a real
//! WMS window via `IWindowManager.openSession` →
//! `IWindowSession.addToDisplay`.
//!
//! Session 7 (this file) is the first sub-session: prove
//! `IWindowManager.openSession` returns a valid `IWindowSession` binder
//! from `system_server`'s `"window"` service. No `addToDisplay` yet —
//! session 8 starts there.
//!
//! See `tasks/44-wms-window-registration.md` for the full multi-session
//! plan.

// ─── Session 7: openSession + Bn-side IWindowSessionCallback server ──────────

#[cfg(target_os = "android")]
mod session7 {
    use std::sync::OnceLock;

    use crate::binder_aidl::android::view::IWindowManager::IWindowManager;
    use crate::binder_aidl::android::view::IWindowSessionCallback::{
        BnWindowSessionCallback, IWindowSessionCallback, IWindowSessionCallbackAsyncService,
    };

    /// Bn-side server for `IWindowSessionCallback`. WMS calls this back
    /// when the animator scale changes (after `Settings.Global.ANIMATOR_DURATION_SCALE`
    /// flips). Won't fire during the openSession probe but the binder
    /// identity must be live (WMS stores it in the SessionInfo struct).
    struct WindowSessionCallbackImpl;
    impl rsbinder::Interface for WindowSessionCallbackImpl {}
    #[async_trait::async_trait]
    impl IWindowSessionCallbackAsyncService for WindowSessionCallbackImpl {
        async fn r#onAnimatorScaleChanged(&self, scale: f32) -> rsbinder::status::Result<()> {
            log::info!("wms/sessioncb: onAnimatorScaleChanged({scale}) fired");
            Ok(())
        }
    }

    /// Tokio current-thread runtime as the `BinderAsyncRuntime` —
    /// required by `BnWindowSessionCallback::new_async_binder`. Same
    /// shape as `ime_impl::session3::TokioRuntime`.
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
            log::warn!("wms: binder init failed: {reason}");
            return;
        }

        let svc: rsbinder::Strong<dyn IWindowManager> =
            match rsbinder::hub::get_interface("window") {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("wms: window service unavailable: {e:?}");
                    return;
                }
            };

        let callback: rsbinder::Strong<dyn IWindowSessionCallback> =
            BnWindowSessionCallback::new_async_binder(WindowSessionCallbackImpl, TokioRuntime);

        log::info!(
            "wms: calling openSession(callback=Bn) — watching for transport \
             vs. permission/identity rejection. system_server WMS typically \
             does not gate openSession itself (the gate is on addToDisplay), \
             but we're root-via-su so even gated calls should pass."
        );

        match svc.r#openSession(&callback) {
            Ok(_session) => {
                log::info!(
                    "wms: openSession OK — got IWindowSession binder. \
                     Transport validated against android.view.IWindowManager; \
                     WMS accepted our IWindowSessionCallback Bn server. \
                     Session 7 milestone reached: a real session binder is live. \
                     Next: addToDisplay with empty LayoutParams (session 8) to \
                     find out which fields WMS actually requires."
                );
            }
            Err(e) => {
                // Mirror ime_impl.rs:374-401's pattern for distinguishing
                // UnexpectedNull (a documented-null response, which is
                // success at the transport layer) from a real structured
                // rejection (EX_SECURITY / IllegalStateException / ...).
                let exc = e.exception_code();
                let code: rsbinder::StatusCode = e.into();
                if code == rsbinder::StatusCode::UnexpectedNull {
                    log::info!(
                        "wms: openSession returned null (rsbinder maps to UnexpectedNull). \
                         The call landed at WMS; WMS chose to return null. \
                         Session 7 transport milestone reached, but session 8 will need \
                         to investigate why WMS refused to allocate a session for our \
                         calling identity (uid=0 via su)."
                    );
                } else {
                    log::warn!(
                        "wms: openSession rejected — exception={exc:?} code={code:?}. \
                         Not UnexpectedNull — WMS rejected with a structured Status. \
                         Likely categories: EX_SECURITY (permission), \
                         IllegalStateException (calling-uid check), or \
                         transport failure (decoder mismatch on IWindowSession \
                         return parcel)."
                    );
                }
            }
        }

        // Keep the process (and the IWindowSessionCallback Bn binder)
        // alive for a beat so a curious operator can `dumpsys window`
        // and see whether our session shows up.
        log::info!(
            "wms: holding callback alive for 5s for dumpsys inspection — pid {}",
            std::process::id()
        );
        std::thread::sleep(std::time::Duration::from_secs(5));
        log::info!("wms: exit");
    }
}

#[cfg(target_os = "android")]
pub fn probe_wms_opensession() {
    session7::run();
}

#[cfg(not(target_os = "android"))]
pub fn probe_wms_opensession() {}
