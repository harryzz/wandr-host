extern crate wasm_android_host;

#[cfg(not(target_os = "android"))]
fn main() {
    // Desktop `--install <wandrpkg-dir>`: same task-35 installer as on
    // device, into WANDR_APPS_ROOT — pairs with the desktop `--app <id>`
    // run mode so cross-app deps resolve in the dev loop.
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--install") {
        let Some(wandrpkg) = args.get(i + 1) else {
            eprintln!("wandr-host --install: requires a <wandrpkg-dir> path");
            std::process::exit(2);
        };
        match wasm_android_host::install_wandrpkg(std::path::Path::new(wandrpkg)) {
            Ok(installed) => {
                println!(
                    "installed: {} v{} → {}",
                    installed.app_id, installed.version, installed.install_dir.display(),
                );
                return;
            }
            Err(e) => {
                eprintln!("wandr-host --install: {e:#}");
                std::process::exit(1);
            }
        }
    }
    // Desktop `--camera-shot <outfile.png>`: verify PiP self-view compositing.
    if let Some(i) = args.iter().position(|a| a == "--camera-shot") {
        let _ = env_logger::builder().try_init();
        let out = args.get(i + 1).map(String::as_str).unwrap_or("camera-shot.png");
        if let Err(e) = wasm_android_host::camera_preview_shot(out) {
            eprintln!("wandr-host --camera-shot: {e:#}");
            std::process::exit(1);
        }
        println!("camera-shot: wrote {out}");
        return;
    }
    // Desktop `--video-selfview-test`: reproduce/measure the Signal self-view
    // freeze — a 60fps render loop pumping the blocking camera encoder.
    if args.iter().any(|a| a == "--video-selfview-test") {
        let _ = env_logger::builder().try_init();
        if let Err(e) = wasm_android_host::video_selfview_test() {
            eprintln!("wandr-host --video-selfview-test: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // Desktop `--media-test`: exercise camera + audio-out + audio-in (mic level
    // bar) to isolate per-platform media bugs without a call partner.
    if args.iter().any(|a| a == "--media-test") {
        // Default to `info` so the diagnostic prints without RUST_LOG (its own
        // lines are at warn, but the backend camera/audio lines are at info).
        let _ = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("info"),
        ).try_init();
        if let Err(e) = wasm_android_host::media_test() {
            eprintln!("wandr-host --media-test: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // `--font-probe`: does Skia's SYSTEM FontMgr resolve OS-installed fonts by NAME
    // with real (non-zero) metrics on this desktop? (The zero-metrics ban in CLAUDE.md
    // is Android-specific; verify before adding a desktop resolve-by-name path.)
    if args.iter().any(|a| a == "--font-probe") {
        let _ = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("info"),
        ).try_init();
        wasm_android_host::font_probe();
        return;
    }
    // Desktop `--run-once <app-id>`: one-shot a wasi:cli/command guest from
    // WANDR_APPS_ROOT (e.g. wandr.video.test), same as the device path. Headless
    // — no winit window. Mirrors the android main's --run-once branch.
    if let Some(i) = args.iter().position(|a| a == "--run-once") {
        let Some(app_id) = args.get(i + 1) else {
            eprintln!("wandr-host --run-once: requires <app-id>");
            std::process::exit(2);
        };
        if let Err(e) = wasm_android_host::run_once::run(app_id) {
            eprintln!("wandr-host --run-once: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    wasm_android_host::run();
}

// Android entry — four modes selected by argv:
//
//   `wandr-host`                                  → NativeActivity stub (this
//                                                  bin is never executed by
//                                                  the APK; android_main in
//                                                  the cdylib is the entry).
//   `wandr-host --install <wandrpkg-dir>`           → task-35 installer: read
//                                                  bundle, AOT-precompile,
//                                                  write `cache-key.toml`.
//   `wandr-host --standalone [--app <app-id>]`    → task-33 boot-model:
//                                                  privileged process that
//                                                  owns the display. Loads
//                                                  the dev cwasm at
//                                                  /data/local/tmp by default;
//                                                  with `--app`, loads via
//                                                  AppRef::Installed.
//   `wandr-host --run-once <app-id>`              → task-36 step-7 one-shot:
//                                                  load an installed
//                                                  wasi:cli/command app,
//                                                  call `wasi:cli/run.run()`
//                                                  once, exit with its
//                                                  status. Used for
//                                                  CLI/smoke consumers.
//   `wandr-host --probe-ime`                      → task-40 session-2 probe:
//                                                  one-shot read-only call
//                                                  to IMMS
//                                                  (isImeTraceEnabled) to
//                                                  verify rsbinder reaches
//                                                  the input method service.
//   `wandr-host --probe-ime-addclient`            → task-40 session-3 probe:
//                                                  stand up Bn-side servers
//                                                  for IInputMethodClient +
//                                                  IRemoteInputConnection,
//                                                  call addClient on IMMS,
//                                                  log the outcome (accept
//                                                  vs permission/identity
//                                                  rejection).
#[cfg(target_os = "android")]
fn main() {
    let args: Vec<String> = std::env::args().collect();

    // [spike] `--font-probe` on device: does skia-safe 0.99 (Skia m150) resolve system fonts by
    // name with real metrics? Logs go to logcat via android_logger (the working Android path).
    if args.iter().any(|a| a == "--font-probe") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Info),
        );
        wasm_android_host::font_probe();
        return;
    }

    if args.iter().any(|a| a == "--probe-ime") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::ime_impl::probe();
        return;
    }

    if args.iter().any(|a| a == "--probe-ime-addclient") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::ime_impl::probe_addclient();
        return;
    }

    if args.iter().any(|a| a == "--probe-ime-startinput") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::ime_impl::probe_startinput();
        return;
    }

    // Audio mic-capture de-risk (does openStream(INPUT) succeed for our caller?).
    if args.iter().any(|a| a == "--probe-audio-capture") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_impl::probe_capture();
        return;
    }

    // Task 76 P1 — call-order full-duplex capture probe: --probe-audio-duplex <preset>.
    if let Some(i) = args.iter().position(|a| a == "--probe-audio-duplex") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        let preset = args.get(i + 1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(6);
        wasm_android_host::audio_impl::probe_duplex(preset);
        return;
    }

    // Audio mic→speaker loopback (full capture path: hear yourself).
    if args.iter().any(|a| a == "--probe-audio-loopback") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_impl::probe_loopback();
        return;
    }

    // Task-76 audio capability probe (read-only): dump the device's real audio
    // picture (ports/routing/volumes via dumpsys + binder reachability) and a
    // typed device model. See audio_caps.rs.
    if args.iter().any(|a| a == "--probe-audio-caps") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_caps::probe();
        return;
    }

    // Task-93 video spike: open the camera + HW VP8-encode its frames under
    // --no-art, report fps / first-frame latency. `wandr-host --probe-video`.
    if args.iter().any(|a| a == "--probe-video") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::video_probe::probe_video();
        return;
    }

    // Task 93 Phase A — `wandr:crypto` symmetric core: correctness vectors + per-
    // algorithm throughput (HW AES/SHA via the ARMv8 extensions). Pure CPU, no
    // device services needed → runs anywhere. `wandr-host --probe-crypto`.
    if args.iter().any(|a| a == "--probe-crypto") {
        wasm_android_host::crypto::probe();
        return;
    }

    // Task-76 P8 volume probe: read media volume range + speaker/earpiece index,
    // set speaker to max, read back, restore. Proves the write path.
    if args.iter().any(|a| a == "--probe-audio-volume") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_policy_impl::probe_volume();
        return;
    }

    // Standalone tone player (same media.aaudio MMAP path the host uses) — for
    // A/B testing audio routing with vs without system_server. `--play-tone [ms]
    // [hz] [vol]`. Defaults 8000ms, 440Hz, 0.6.
    if let Some(i) = args.iter().position(|a| a == "--play-tone") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        let ms  = args.get(i + 1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(8000);
        let hz  = args.get(i + 2).and_then(|s| s.parse::<f32>().ok()).unwrap_or(440.0);
        let vol = args.get(i + 3).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.6);
        wasm_android_host::audio_impl::play_tone(ms, hz, vol);
        return;
    }

    // Task 97 bug #1 — reproduce the SHARED-output suspend stall on the call path
    // (and A/B the silence-pump fix). `--probe-call-stall [secs] [speaker0|1]
    // [drain0|1] [pump0|1]`. Defaults: 8s resume window, earpiece (speaker=0),
    // drain-during-underflow OFF, pump OFF (the permanent-stall baseline). pump=1
    // opens via the guest create_track path (spawns the silence-pump fix) → the
    // same underflow should NOT stall. Drive logcat for "Suspending stream".
    if let Some(i) = args.iter().position(|a| a == "--probe-call-stall") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        let secs    = args.get(i + 1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(8);
        let speaker = args.get(i + 2).map(|s| s == "1").unwrap_or(false);
        let drain   = args.get(i + 3).map(|s| s == "1").unwrap_or(false);
        let pump    = args.get(i + 4).map(|s| s == "1").unwrap_or(false);
        wasm_android_host::audio_impl::probe_call_stall(secs, speaker, drain, pump);
        return;
    }

    // Task 97 bug #5 — verify the earpiece/speaker toggle re-routes the shared
    // MEDIA output (setDevicesRoleForStrategy) instead of pinning a per-stream
    // deviceId (which -889s). `--probe-route-toggle`.
    if args.iter().any(|a| a == "--probe-route-toggle") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_impl::probe_route_toggle();
        return;
    }

    // Task 98 — native AudioFlinger backend smoke test, run from *inside* wandr-host
    // (which already hosts a binder thread-pool, unlike the standalone af_probe).
    // Opens an AudioTrack directly via the `audioclient` crate (IAudioFlingerService
    // .createTrack → cblk ring) and writes a 440 Hz tone. `--probe-audioclient [secs]
    // [hz] [vol]`. This is the on-device validation that the AudioFlinger-direct path
    // makes sound without AAudioService.
    if let Some(i) = args.iter().position(|a| a == "--probe-audioclient") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        let secs = args.get(i + 1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(3);
        let hz   = args.get(i + 2).and_then(|s| s.parse::<f32>().ok()).unwrap_or(440.0);
        let vol  = args.get(i + 3).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.3);
        probe_audioclient(secs, hz, vol);
        return;
    }

    // Task 98 (Tier 3) — blocking I/O via the cblk futex: blocking-write a tone and
    // confirm it paces to the server drain (writing N s of audio takes ~N s wall-clock,
    // NOT instant) instead of busy-polling. `--probe-audioclient-blocking [secs] [hz]`.
    if let Some(i) = args.iter().position(|a| a == "--probe-audioclient-blocking") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        let secs = args.get(i + 1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(4);
        let hz   = args.get(i + 2).and_then(|s| s.parse::<f32>().ok()).unwrap_or(440.0);
        probe_audioclient_blocking(secs, hz);
        return;
    }

    // Task 98 — createTrack request-variant matrix: isolate which CreateTrackRequest
    // field audioserver silently rejects with BAD_VALUE (it logs nothing server-side,
    // even at VERBOSE, in both ART and --no-art). `--probe-audioclient-matrix`.
    if args.iter().any(|a| a == "--probe-audioclient-matrix") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        audioclient::probe_matrix();
        return;
    }

    // Task 98 — WIT audio-path integration test: drive create_track/write/start (the
    // backend-dispatch the guest's WIT Host calls) → the selected backend (audioclient
    // by default; WANDR_AUDIO_BACKEND=aaudio for legacy). `--probe-audio-backend [secs]`.
    if let Some(i) = args.iter().position(|a| a == "--probe-audio-backend") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        let secs = args.get(i + 1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(3);
        wasm_android_host::audio_impl::probe_backend(secs, 440.0, 0.4);
        return;
    }

    // Task 98 — AudioFlinger-direct CAPTURE smoke test: open a mic record via the
    // `audioclient` crate (IAudioFlingerService.createRecord → IAudioRecord → cblk
    // ring), read PCM for `secs`, and report frame count + peak level (so it's clear
    // real mic audio is flowing). `--probe-audioclient-capture [secs]`.
    if let Some(i) = args.iter().position(|a| a == "--probe-audioclient-capture") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        let secs = args.get(i + 1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(3);
        // optional 2nd arg = AUDIO_SOURCE_* (1=MIC default, 7=VOICE_COMMUNICATION/AEC).
        let source = args.get(i + 2).and_then(|s| s.parse::<i32>().ok()).unwrap_or(1);
        probe_audioclient_capture(secs, source);
        return;
    }

    // --no-art audio bring-up: replicate AudioService's boot volume/device init
    // (initStreamVolume + setStreamVolumeIndex + mode/force-use) so audio is
    // audible without system_server. Run by run-hybrid-stack after audioserver.
    if args.iter().any(|a| a == "--init-audio-policy") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_policy_impl::init_audio_policy();
        return;
    }

    // Task-76 #6 — enumerate audio ports over binder (listAudioPorts) instead
    // of dumpsys; tests AudioPortFw decode at runtime.
    if args.iter().any(|a| a == "--probe-audio-ports") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_policy_impl::probe_list_audio_ports();
        return;
    }

    // Task-76 routing core (step 4): build the live device model and log the
    // resolved stream plan for every intent (read-only). See audio_routing.rs.
    if args.iter().any(|a| a == "--probe-audio-route") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_routing::probe_routes();
        return;
    }

    // Task-76 audio state matrix (step 3): targeted self-restoring on-device
    // opens filling the (usage × mode × device × sharing × format × channels)
    // matrix. Restores phone state to NORMAL after the comms-mode cells.
    if args.iter().any(|a| a == "--probe-audio-matrix") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_caps::probe_matrix();
        return;
    }

    // Call-audio reachability (read-only): does a root caller reach
    // media.audio_policy? Logs phone state + COMMUNICATION routing.
    if args.iter().any(|a| a == "--probe-audio-policy") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::audio_policy_impl::probe();
        return;
    }

    // Call-audio routing WRITE probe: setForceUse(COMMUNICATION, speaker|earpiece)
    // then restore. `--probe-audio-policy-route speaker` | `... earpiece`.
    if let Some(i) = args.iter().position(|a| a == "--probe-audio-policy-route") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        let speaker = args.get(i + 1).map(|s| s == "speaker").unwrap_or(false);
        wasm_android_host::audio_policy_impl::probe_route(speaker);
        return;
    }

    if args.iter().any(|a| a == "--probe-ime-showsoft") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::ime_impl::probe_showsoftinput();
        return;
    }

    if args.iter().any(|a| a == "--probe-wms-opensession") {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        wasm_android_host::wms_impl::probe_wms_opensession();
        return;
    }

    // Task 45 step 1 — zygote server mode. Long-lived parent process that
    // preloads wasmtime::Engine and fork()s on each LAUNCH command from
    // /data/local/tmp/wandr-zygote.sock. See tasks/45-wandr-zygote-spike.md.
    if args.iter().any(|a| a == "--zygote") {
        // Optional preload-hint app-id; documentary at MVP (only engine
        // is preloaded today). `--zygote-preload <app-id>` keeps the CLI
        // shape forward-compatible with per-app Component preload later.
        let preload_hint = args.iter()
            .position(|a| a == "--zygote-preload")
            .and_then(|i| args.get(i + 1))
            .map(|s| s.as_str());
        if let Err(e) = wasm_android_host::zygote::serve(preload_hint) {
            eprintln!("wandr-host --zygote: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    // Task 45 step 1 — zygote client mode. Connect to the zygote, write
    // LAUNCH <app-id> (headless / wasi:cli/command), print the child pid
    // (or the structured ERR).
    if let Some(i) = args.iter().position(|a| a == "--zygote-launch") {
        let Some(app_id) = args.get(i + 1) else {
            eprintln!("wandr-host --zygote-launch: requires <app-id>");
            std::process::exit(2);
        };
        match wasm_android_host::zygote::launch_client(app_id, /*gui=*/ false, /*overlay=*/ false) {
            Ok(_pid) => return,
            Err(e) => {
                eprintln!("wandr-host --zygote-launch: {e:#}");
                std::process::exit(1);
            }
        }
    }

    // Task 45 step 2 — same as above but for full Compose render loop.
    // Forks via the zygote, child runs standalone::run_with_engine
    // against the preloaded engine.
    // Accepts an optional <app-id>; if omitted, the child falls back
    // to the dev cwasm at /data/local/tmp/skiko-component.cwasm.
    // `--overlay` (task 47 step 3c) acquires a bottom-strip overlay
    // surface in the child instead of a fullscreen one — used for
    // IME apps such as `wandr.ime.keyboard`.
    if let Some(i) = args.iter().position(|a| a == "--zygote-launch-gui") {
        let app_id = args.get(i + 1).map(|s| s.as_str()).unwrap_or("");
        let overlay = args.iter().any(|a| a == "--overlay");
        match wasm_android_host::zygote::launch_client(app_id, /*gui=*/ true, overlay) {
            Ok(_pid) => return,
            Err(e) => {
                eprintln!("wandr-host --zygote-launch-gui: {e:#}");
                std::process::exit(1);
            }
        }
    }

    // Task 46 step 1 — graceful + forceful KILL of a child via the
    // zygote socket. Validates server-side that the pid is one of the
    // zygote's own children before signaling.
    for (flag, force) in [
        ("--zygote-kill", false),
        ("--zygote-kill-force", true),
    ] {
        if let Some(i) = args.iter().position(|a| a == flag) {
            let Some(pid_s) = args.get(i + 1) else {
                eprintln!("wandr-host {flag}: requires <pid>");
                std::process::exit(2);
            };
            let Ok(pid) = pid_s.parse::<i32>() else {
                eprintln!("wandr-host {flag}: <pid> must be an integer");
                std::process::exit(2);
            };
            match wasm_android_host::zygote::kill_client(pid, force) {
                Ok(()) => return,
                Err(e) => {
                    eprintln!("wandr-host {flag}: {e:#}");
                    std::process::exit(1);
                }
            }
        }
    }

    // Task 46 step 2 — PRELOAD socket command client. Used by the
    // installer (after upgrades) and by the future wandr-arbiter
    // (predictive warm-up before launches). System bundles are
    // auto-preloaded at zygote startup; this command handles user
    // apps and post-upgrade refreshes.
    if let Some(i) = args.iter().position(|a| a == "--zygote-preload") {
        let Some(app_id) = args.get(i + 1) else {
            eprintln!("wandr-host --zygote-preload: requires <app-id>");
            std::process::exit(2);
        };
        match wasm_android_host::zygote::preload_client(app_id) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("wandr-host --zygote-preload: {e:#}");
                std::process::exit(1);
            }
        }
    }

    if let Some(i) = args.iter().position(|a| a == "--install") {
        let Some(wandrpkg) = args.get(i + 1) else {
            eprintln!("wandr-host --install: requires a <wandrpkg-dir> path");
            std::process::exit(2);
        };
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        match wasm_android_host::install_wandrpkg(std::path::Path::new(wandrpkg)) {
            Ok(installed) => {
                println!(
                    "installed: {} v{} → {}",
                    installed.app_id, installed.version, installed.install_dir.display(),
                );
            }
            Err(e) => {
                eprintln!("wandr-host --install: {e:#}");
                std::process::exit(1);
            }
        }
        return;
    }

    if let Some(i) = args.iter().position(|a| a == "--run-once") {
        let Some(app_id) = args.get(i + 1) else {
            eprintln!("wandr-host --run-once: requires <app-id>");
            std::process::exit(2);
        };
        if let Err(e) = wasm_android_host::run_once::run(app_id) {
            eprintln!("wandr-host --run-once: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    if args.iter().any(|a| a.starts_with("--standalone")) {
        let app_id = args.iter()
            .position(|a| a == "--app")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        // Overlay mode: `--standalone-overlay` = bottom strip (IME,
        // task 47); `--standalone-overlay-bottom-bar` = thin bottom nav
        // strip (taskbar, task 56); `--standalone-overlay-top` = top
        // strip (status bar, task 55); none = fullscreen.
        use wasm_android_host::standalone::OverlayMode;
        let mode = if args.iter().any(|a| a == "--standalone-overlay-top") {
            OverlayMode::Top
        } else if args.iter().any(|a| a == "--standalone-overlay-bottom-bar") {
            OverlayMode::BottomBar
        } else if args.iter().any(|a| a == "--standalone-overlay-lock") {
            OverlayMode::Lock
        } else if args.iter().any(|a| a == "--standalone-overlay") {
            OverlayMode::Bottom
        } else {
            OverlayMode::None
        };
        if let Err(e) = wasm_android_host::standalone::run(app_id, mode) {
            eprintln!("wandr-host --standalone: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    let _keep: usize = wasm_android_host::android_main as usize;
    std::hint::black_box(_keep);
}

// Task 98 — AudioFlinger-direct smoke test (see the `--probe-audioclient` flag).
// Runs inside wandr-host so the local IAudioTrackCallback Bn marshals against a real
// binder thread-pool (the standalone af_probe couldn't — kernel rejected the
// oneway-spam ioctl with EINVAL). Opens an output AudioTrack via the `audioclient`
// crate, writes a `hz` sine at `vol` for `secs`, starting after the first accepted
// write, then closes.
#[cfg(target_os = "android")]
fn probe_audioclient(secs: u64, hz: f32, vol: f32) {
    eprintln!("probe-audioclient: open_output(MEDIA/MUSIC, 48k stereo)…");
    let h = audioclient::open_output(audioclient::OutputConfig {
        sample_rate: 48_000,
        channels: 2,
        usage: 1,        // AUDIO_USAGE_MEDIA
        content_type: 2, // AUDIO_CONTENT_TYPE_MUSIC
        flags: 0,
        frame_count: 0,
    });
    if h == 0 {
        eprintln!("probe-audioclient: open_output FAILED (see logcat tag 'audioclient')");
        std::process::exit(1);
    }
    eprintln!("probe-audioclient: handle={h} — writing {hz} Hz tone for {secs}s…");

    let sr = 48_000.0_f32;
    let mut phase = 0.0_f32;
    let mut started = false;
    let mut total = 0usize;
    let mut zero_ticks = 0u32;
    let mut tick = 0u32;
    let mut paused_test_done = false;
    let mut pending: Vec<f32> = Vec::new();
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs() < secs {
        tick += 1;
        // ~200 ticks/s (5 ms sleep). get_timestamp every ~1s (position should advance).
        if started && tick % 200 == 0 {
            match audioclient::get_timestamp(h) {
                Some((pos, nt)) => eprintln!("probe-audioclient: t≈{}s getTimestamp pos={pos} nanoTime={nt}", tick / 200),
                None => eprintln!("probe-audioclient: t≈{}s getTimestamp (none yet)", tick / 200),
            }
        }
        // one pause→resume cycle at ~1.5s (proves pause keeps position, start resumes).
        if started && !paused_test_done && tick == 300 {
            paused_test_done = true;
            let p = audioclient::pause(h);
            eprintln!("probe-audioclient: pause ok={p} (200ms gap)…");
            std::thread::sleep(std::time::Duration::from_millis(200));
            let r = audioclient::start(h);
            eprintln!("probe-audioclient: resumed (start ok={r})");
        }
        // drop the gain to 0.1 at ~2.2s (audible: the tone should get quieter).
        if started && tick == 440 {
            audioclient::set_volume(h, 0.1);
            eprintln!("probe-audioclient: set_volume(0.1) — tone should drop");
        }
        // Smooth pacing: keep a `pending` generator buffer topped up to ~one ring
        // (4096 frames) and write whatever the ring will accept, advancing the sine
        // phase ONLY by frames actually consumed (drain). This (a) keeps the ring full
        // so the HAL never underruns, and (b) preserves sample continuity on a partial
        // write (no phase jump → no click). The 5 ms sleep stays ahead of the drain.
        let target = 4096usize; // ≈ one ring of headroom (server ring ~3844 frames)
        while pending.len() < target * 2 {
            let s = phase.sin() * vol;
            phase += 2.0 * std::f32::consts::PI * hz / sr;
            if phase > 2.0 * std::f32::consts::PI {
                phase -= 2.0 * std::f32::consts::PI;
            }
            pending.push(s);
            pending.push(s);
        }
        let n = audioclient::write(h, &pending);
        if n == 0 {
            zero_ticks += 1;
        } else {
            total += n;
            pending.drain(0..n * 2); // keep the unwritten tail (continuity)
        }
        if !started && n > 0 {
            let ok = audioclient::start(h);
            started = true;
            eprintln!("probe-audioclient: started (IAudioTrack.start ok={ok})");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let f = audioclient::flush(h);
    eprintln!("probe-audioclient: wrote {total} frames (zero-ticks={zero_ticks}); flush ok={f}; closing");
    audioclient::stop(h);
    audioclient::close(h);
    eprintln!("probe-audioclient: done");
}

// Task 98 (Tier 3) — blocking-write smoke test (see `--probe-audioclient-blocking`).
// Opens an output track, pre-fills + starts it, then drives a tone with the BLOCKING
// write (futex-paced). The proof: emitting `secs` seconds of audio takes ~`secs`
// seconds of wall-clock — the writer sleeps on the cblk futex while the ring is full
// and the server wakes it on drain, rather than busy-spinning on a short write.
#[cfg(target_os = "android")]
fn probe_audioclient_blocking(secs: u64, hz: f32) {
    use std::time::{Duration, Instant};
    eprintln!("probe-blocking: open_output(MEDIA/MUSIC, 48k stereo)…");
    let h = audioclient::open_output(audioclient::OutputConfig {
        sample_rate: 48_000,
        channels: 2,
        usage: 1,        // AUDIO_USAGE_MEDIA
        content_type: 2, // AUDIO_CONTENT_TYPE_MUSIC
        flags: 0,
        frame_count: 0,
    });
    if h == 0 {
        eprintln!("probe-blocking: open_output FAILED (see logcat 'audioclient')");
        std::process::exit(1);
    }
    // Generate `secs` seconds of a stereo tone (interleaved f32).
    let total_frames = secs as usize * 48_000;
    let mut tone = Vec::with_capacity(total_frames * 2);
    let mut phase = 0.0f32;
    for _ in 0..total_frames {
        let s = (phase).sin() * 0.3;
        phase += 2.0 * std::f32::consts::PI * hz / 48_000.0;
        if phase > 2.0 * std::f32::consts::PI { phase -= 2.0 * std::f32::consts::PI; }
        tone.push(s); tone.push(s);
    }
    // Pre-fill a little (non-blocking) + start so the server is actively draining
    // before we block on it (a blocking write to an unstarted ring would just time out).
    let pre = 1920 * 2; // ~40 ms
    let wrote0 = audioclient::write(h, &tone[..pre.min(tone.len())]);
    let started = audioclient::start(h);
    eprintln!("probe-blocking: pre-fill={wrote0} frames, start ok={started}; blocking-writing {secs}s of {hz} Hz…");
    let t0 = Instant::now();
    let mut written = wrote0;
    let mut pos = pre.min(tone.len());
    while pos < tone.len() {
        // Block until the ring frees (futex), up to 1 s per call.
        let n = audioclient::write_blocking(h, &tone[pos..], Duration::from_secs(1));
        if n == 0 { eprintln!("probe-blocking: write_blocking returned 0 (timeout/error) — stopping"); break; }
        written += n;
        pos += n * 2;
    }
    let elapsed = t0.elapsed().as_secs_f32();
    let audio_secs = written as f32 / 48_000.0;
    eprintln!(
        "probe-blocking: wrote {written} frames ({audio_secs:.2}s of audio) in {elapsed:.2}s wall-clock — \
         futex-paced if wall≈audio (busy-poll would finish near-instantly)"
    );
    audioclient::stop(h);
    audioclient::close(h);
    eprintln!("probe-blocking: done");
}

// Task 98 — AudioFlinger-direct capture smoke test (see `--probe-audioclient-capture`).
// Opens a mic record via the `audioclient` crate, reads PCM for `secs`, reports the
// frame count + peak level (proves real mic audio is flowing through the cblk ring).
#[cfg(target_os = "android")]
fn probe_audioclient_capture(secs: u64, source: i32) {
    // source: AUDIO_SOURCE_* — MIC=1 (default), VOICE_COMMUNICATION=7 (engages the
    // device's voice pre-processing / AEC via the audio_effects.xml <preprocess> map).
    eprintln!("probe-capture: open_input(source={source}, 48k mono)…");
    let h = audioclient::open_input(audioclient::InputConfig {
        sample_rate: 48_000,
        channels: 1,
        source,
    });
    if h == 0 {
        eprintln!("probe-capture: open_input FAILED (see logcat tag 'audioclient')");
        std::process::exit(1);
    }
    let ok = audioclient::start(h);
    eprintln!("probe-capture: handle={h} started (IAudioRecord.start ok={ok}) — reading {secs}s…");

    let mut total = 0usize;
    let mut peak = 0.0_f32;
    let mut zero_reads = 0u32;
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs() < secs {
        let buf = audioclient::read(h, 480);
        if buf.is_empty() {
            zero_reads += 1;
        } else {
            total += buf.len();
            for &s in &buf {
                peak = peak.max(s.abs());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    eprintln!("probe-capture: read {total} samples, peak={peak:.4}, zero-reads={zero_reads}; closing");
    audioclient::stop(h);
    audioclient::close(h);
    eprintln!("probe-capture: done");
}
