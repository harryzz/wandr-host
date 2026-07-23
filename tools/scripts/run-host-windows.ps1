<#
.SYNOPSIS
  One-command Windows bring-up for the DXVA2/D3D11 video path (task 117 Phase 2).

  Builds the host with the `d3d11` feature, provisions ANGLE (libEGL/libGLESv2)
  next to the exe, and runs `--video-decode-file` to decode a real H.264 MP4
  through the hardware d3d11 backend, writing a PNG of the last presented frame.

  By default it builds H.264-ONLY (drops libvpx/dav1d/libde265), so you need NO
  codec build toolchains (no vcpkg, no meson/nasm) - the d3d11 backend covers
  H.264. Pass -FullCodecs for the full desktop build (needs those toolchains).

  NOTE: `--video-decode-file` is HEADLESS (Skia raster), so it exercises the
  d3d11 DECODE + reorder + present and proves it on Windows via the PNG. It does
  NOT exercise the ANGLE zero-copy GL import - that needs the host running in a
  WINDOW (the further bring-up). This is the reliable first step.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File run-host-windows.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File run-host-windows.ps1 -Video C:\clips\my.mp4 -Gpu
#>
param(
    [string]$Video = "",
    [switch]$FullCodecs,
    [switch]$Gpu,
    [switch]$BuildOnly,
    [switch]$Window   # run WINDOWED: install wandr.video.player + play on screen
                      # (d3d11 decode on ANGLE's device -> zero-copy GL import).
)
$ErrorActionPreference = 'Stop'

# tools/scripts -> host root (runtime/wandr-host) -> repo root (wandr)
$hostRoot = Split-Path (Split-Path $PSScriptRoot)
$repoRoot = Split-Path (Split-Path $hostRoot)
$manifest = Join-Path $hostRoot 'Cargo.toml'
$lock     = Join-Path $hostRoot 'Cargo.lock'
$target   = Join-Path $env:TEMP 'wandr-host-target'
$exe      = Join-Path $target 'release\wasm-android-host.exe'
if (-not $Video) { $Video = Join-Path $repoRoot 'repros\samples\test-25fps.mp4' }

Write-Host "host    : $hostRoot"
Write-Host "video   : $Video"
Write-Host "target  : $target"
if (-not (Test-Path $manifest)) { throw "host Cargo.toml not found at $manifest" }

# --- 1. Build (H.264-only unless -FullCodecs; back up + restore the temp edits) ---
$tomlBak = Join-Path $env:TEMP 'wandr-host-Cargo.toml.bak'
$lockBak = Join-Path $env:TEMP 'wandr-host-Cargo.lock.bak'
Copy-Item $manifest $tomlBak -Force
if (Test-Path $lock) { Copy-Item $lock $lockBak -Force }
$dropRe   = 'wandr-video = \{ path = "crates/wandr-video", features = \[[^\]]*\] \}'
$dropWith = 'wandr-video = { path = "crates/wandr-video", features = [] }'
try {
    if (-not $FullCodecs) {
        Write-Host "build   : H.264-only (dropping libvpx/dav1d/libde265, no codec toolchains needed)"
        (Get-Content -Raw $manifest) -replace $dropRe, $dropWith | Set-Content -NoNewline $manifest
    } else {
        Write-Host "build   : full desktop codecs (needs vcpkg libvpx + meson/nasm dav1d)"
    }
    # cargo over a possibly-UNC manifest: run from a NATIVE cwd + native target
    # dir (cargo reads --manifest-path regardless of cwd; the native cwd avoids
    # the "UNC paths are not supported" cmd trap).
    $env:CARGO_TARGET_DIR = $target
    Push-Location $env:TEMP
    try {
        & cargo build --release --features p3-async,d3d11 --manifest-path $manifest
    } finally {
        Pop-Location
    }
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}
finally {
    # Always restore the tree; the codec drop + the lock churn are build-only.
    Copy-Item $tomlBak $manifest -Force
    if (Test-Path $lockBak) { Copy-Item $lockBak $lock -Force }
    Remove-Item $tomlBak, $lockBak -ErrorAction SilentlyContinue
}
if (-not (Test-Path $exe)) { throw "build produced no exe at $exe" }
Write-Host "built   : $exe"

# --- 2. Provision ANGLE next to the exe (from Edge) ---
$edge = Get-ChildItem 'C:\Program Files (x86)\Microsoft\Edge\Application' -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName 'libEGL.dll') } | Select-Object -Last 1
if ($edge) {
    foreach ($d in 'libEGL.dll', 'libGLESv2.dll', 'd3dcompiler_47.dll') {
        $src = Join-Path $edge.FullName $d
        if (Test-Path $src) { Copy-Item $src (Split-Path $exe) -Force }
    }
    Write-Host "angle   : $($edge.FullName) -> $(Split-Path $exe)"
} else {
    Write-Warning "ANGLE (libEGL.dll) not found in Edge - the windowed/GL path won't init, but --video-decode-file (headless) still works."
}

if ($BuildOnly) { Write-Host "`n-BuildOnly: done."; return }

# --- 3w. WINDOWED: install wandr.video.player + play it on screen ---
if ($Window) {
    # The player mounts ~/wandr/repros/oxideav-spike/samples -> /samples and reads
    # bbb-h264.mp4 itself (guest-side demux). ~ expands to %USERPROFILE% on Windows
    # and the host SKIPS a mount whose host dir is absent, so put the sample there.
    $sampSrc = Join-Path $repoRoot 'repros\oxideav-spike\samples'
    $sampDst = Join-Path $env:USERPROFILE 'wandr\repros\oxideav-spike\samples'
    New-Item -ItemType Directory -Force $sampDst | Out-Null
    foreach ($f in 'bbb-h264.mp4', 'bbb.h264', 'bbb.srt') {
        $s = Join-Path $sampSrc $f
        if (Test-Path $s) { Copy-Item $s $sampDst -Force }
    }
    Write-Host "sample  : $sampDst"

    # Install the (prebuilt) video player into a self-contained apps root, then run.
    $env:WANDR_APPS_ROOT = Join-Path $target 'apps-root'
    New-Item -ItemType Directory -Force $env:WANDR_APPS_ROOT | Out-Null
    $appDir = Join-Path $repoRoot 'apps\user\wandr.video.player'
    Write-Host "install : $appDir"
    & $exe --install $appDir
    if ($LASTEXITCODE -ne 0) { throw "app install failed ($LASTEXITCODE)" }

    # Force GPU-texture output (belt + braces; it also auto-enables once the host
    # points the decoder at ANGLE's device).
    $env:WANDR_VIDEO_D3D11_GPU = '1'
    $env:RUST_LOG = 'wasm_android_host=info,wandr_video=info'
    Write-Host "`n=== running WINDOWED: wasm-android-host --app wandr.video.player ==="
    Write-Host "    (a window should open and play Big Buck Bunny; close it to exit)`n"
    & $exe --app wandr.video.player
    Write-Host "`n=== window closed (exit $LASTEXITCODE) ==="
    return
}

# --- 3. Run --video-decode-file (headless decode -> PNG) ---
if (-not (Test-Path $Video)) { throw "video not found: $Video (pass -Video <path.mp4>)" }
if ($Gpu) { $env:WANDR_VIDEO_D3D11_GPU = '1'; Write-Host "gpu     : WANDR_VIDEO_D3D11_GPU=1" }
$env:RUST_LOG = 'wasm_android_host=info,wandr_video=info'
# The host writes its PNG to a hardcoded UNIX path "/tmp/decode-file.png", which
# on Windows resolves to <current drive>\tmp - create it and run from C:\ so the
# write lands at a known place (the write is best-effort in the host, so without
# this the PNG is silently skipped).
New-Item -ItemType Directory -Force 'C:\tmp' | Out-Null
$png = 'C:\tmp\decode-file.png'
Remove-Item $png -ErrorAction SilentlyContinue
Write-Host "`n=== running: wasm-android-host --video-decode-file ===`n"
Push-Location 'C:\'
try { & $exe --video-decode-file $Video; $code = $LASTEXITCODE }
finally { Pop-Location }
Write-Host "`n=== done (exit $code) ==="
if (Test-Path $png) {
    Write-Host "decoded frame PNG: $png  (open it - a correct decode shows the video, not noise)"
} else {
    Write-Host "no PNG at $png - check the RESULT log line above for decoded/presented counts."
}
