# wandr patches to cros-codecs 0.0.6

A faithful copy of the crates.io release with **two** changes, both in
`src/backend/vaapi/decoder.rs` and both marked in-source with
`── wandr patch (n/2) ──`. Grep for `wandr patch` before and after any upgrade;
losing either one is a silent regression, not a build error.

Consumed via `[patch.crates-io]` in the workspace root `Cargo.toml`. The same
patched tree also lives in `repros/vaapi-decode-probe/vendor/` (the isolated
probe that proved the approach on hardware); the two must stay in step.

## (1/2) placeholder context must respect the driver minimum

`VaapiBackend::new` builds a throwaway VA config + context at construction and
hardcodes it to **16x16**, then `.expect()`s the result. Mesa's D3D12/VAOn12
driver enforces a decode-heap minimum and rejects that with
`VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED` (19) — so the *process panics* rather
than the backend declining. The patch asks the driver for its reported minimum
instead of assuming a constant.

Note that `crates/wandr-video/src/backends/vaapi.rs` ALSO guards this from the
outside: its capability probe refuses the whole backend when the driver's minimum
exceeds 16, precisely so the host never depends on this patch being present to
avoid a panic. Belt and braces, deliberately — a panic here kills the host.

## (2/2) `VaapiDecodedHandle::surface()` widened from `pub(crate)` to `pub`

The only route to decoded pixels for a **custom VA-allocated `VideoFrame`**.

The alternative public path is `VideoFrame::map()`, and it cannot work here:
`VaapiPicture::new` calls `backing_frame.to_native_handle(display)` and moves the
returned `Surface` into the picture. `Surface` is not cloneable, so by the time
anyone holds the frame it no longer has the surface to map. Upstream's own frame
impls sidestep this by being GBM/DMA-backed (they map the underlying bo), which
is exactly what we cannot use — GBM allocation fails on every machine available
to this project, for unrelated per-driver reasons. See the module header of
`crates/wandr-video/src/backends/vaapi.rs`.

Upstreamable: this is a visibility change with no semantic effect, and the
0.0.6 tree already exposes the equivalent accessor on `VaapiPicture`. Worth
proposing rather than carrying forever.
