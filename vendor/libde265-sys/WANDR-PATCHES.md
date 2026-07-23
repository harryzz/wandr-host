# wandr patches to libde265-sys 0.1.1

A copy of the crates.io release with **build.rs only** changed — three fixes so
the `static` (from-source) build works on Windows and macOS-arm64, not just
Linux-x86. Consumed via `[patch.crates-io]` in the workspace roots. The `src/`
(ffi bindings) and `Cargo.toml` are untouched. Grep `wandr patch` in build.rs.

The crate downloads libde265 **1.0.15** source at build time and compiles it with
`cc`, globbing every `.cc`. Upstream's own CMake/autotools gate the things below;
this crate's hand-rolled build did not. All three root causes verified against
the 1.0.15 source.

## FIX 1 — x86 SSE sources on non-x86 (macOS-arm64 blocker)
`libde265/x86/sse-*.cc` `#include <emmintrin.h>` and fail on aarch64 with
"only meant to be used on x86". Their dispatch (`init_acceleration_functions_sse`)
is called only `#ifdef HAVE_SSE4_1`, which this build **never defines** — so even
on x86 those files compile but are never called (dead), and excluding them on
non-x86 just leaves the C fallback. We match the `x86` **path component**
(`{SEP}x86{SEP}`), never the substring, because the build path is under the
target triple `x86_64-...` and a substring match would drop every file.

## FIX 2 — HAVE_MALLOC_H on macOS (macOS blocker)
`image.cc` does `#ifdef HAVE_MALLOC_H → #include <malloc.h>`; macOS has no
`<malloc.h>`. Every other `malloc.h` in libde265 is `#if defined(_MSC_VER)`-
guarded, so `image.cc` is the only offender. Define `HAVE_MALLOC_H` where the
header exists (Linux, MSVC), not on macOS.

## FIX 4 — memalign / HAVE_POSIX_MEMALIGN (macOS blocker, exposed by FIX 2)
`image.cc`'s `ALLOC_ALIGNED` macro chooses `_aligned_malloc` on `_WIN32`,
`posix_memalign` when `HAVE_POSIX_MEMALIGN` is set, else glibc `memalign` —
which macOS lacks (undeclared identifier). Dropping `<malloc.h>` on macOS (FIX 2)
left it on the `memalign` branch and it failed. Define `HAVE_POSIX_MEMALIGN` on
unix so Linux and macOS both take the portable POSIX branch; Windows is on
`_aligned_malloc` and needs neither. Only `image.cc` uses this.

## FIX 3 — MSVC dllimport (Windows blocker)
`de265.h`: `#if defined(_MSC_VER) && !defined(LIBDE265_STATIC_BUILD)` makes
`LIBDE265_API` = `__declspec(dllimport)`. Compiling the function DEFINITIONS in
`de265.cc` against a dllimport declaration is `error C2491`. We define
`LIBDE265_STATIC_BUILD` so the macro is empty — the build is static, which is
exactly the case the macro exists for.

Upstreamable: all four match what libde265's CMake already does
(`DISABLE_SSE`, malloc.h feature-detect, `LIBDE265_STATIC_BUILD`). Worth a PR to
libde265-sys rather than carrying forever.
