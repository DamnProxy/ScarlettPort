# Upscaling and frame generation

The title's Series S profile renders below its output size and calls the
console FidelityFX upscaler (`ffxDispatch` with an `ffxDispatchDescUpscale`)
with colour, depth, motion vectors, jitter, motion-vector scale, render and
output sizes and the camera planes. `victory_hle::modules::ffx` takes that
call; `victory_core::upscale` decides what answers it; `victory_gpu::fsr`
answers it with FSR 3.1 through the PC FidelityFX API.

## Switches

Environment variable, or the `[upscale]` key of the config file (the
environment wins; the F1 overlay's Display page edits and saves them).

| variable | key | values | |
|---|---|---|---|
| `VICTORY_UPSCALER` | `upscaler` | `fsr` (default), `dlss`, `title`, `stretch` | `title` is the old `VICTORY_OWN_UPSCALER=1` (restart); `dlss` is not wired yet and is answered by FSR |
| `VICTORY_UPSCALE_QUALITY` | `quality` | `native`, `quality` (default, the title's 2/3), `balanced`, `performance`, `ultra` | restart; other than `quality` it sets the render scale (`VICTORY_RENDER_SCALE` wins) |
| `VICTORY_UPSCALE_SHARPNESS` | `sharpness` | 0..1, default 0.2 | live |
| `VICTORY_FRAMEGEN` | `framegen` | 0 / 1 | FSR 3 frame generation, works on RTX 30. **Only use it at a base rate of 15 fps or more**: below 15 fps it is held off (back on above 17). DLSS frame generation needs an RTX 40 and is not offered. |
| `VICTORY_FFX_DLL` | | a path | the FidelityFX DLL, if not next to `victory.exe` |

With frame generation running, the pacer (`VICTORY_PACE_FPS`, an output
rate) holds each rendered frame two periods.

## The FidelityFX DLL

`amd_fidelityfx_dx12.dll` next to `victory.exe`, built from AMD's
MIT-licensed FidelityFX SDK source:

- tag **v1.1.4**, commit **c6efa6bf7f2027b3ec94f28578bb5965eabb9e55**
  (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK), LICENSE.txt MIT;
- components: the FidelityFX API DLL with FSR 2, FSR 3.1 upscaler, optical
  flow and frame interpolation (frame generation and its swap chain), DX12 x64;
- built on the farm with `xtask/ffx-cross-build.sh` (clang-cl / lld-link from
  cargo-xwin, the SDK's shader compiler under wine); `xtask/build-ffx-dll.ps1`
  is the MSVC route;
- SHA-256 of the build shipped in `fxfork-upscale-target\release`:
  `09dbf0714129e139f8c3490449b8060ec73299b259117d6517d14f46eea2fcef`.

The licence is in `THIRD_PARTY_LICENSES/FidelityFX-MIT.txt` and must travel
with the DLL. The DLL is not in git.

AMD's later SDK 2.x signed binaries (`amd_fidelityfx_loader_dx12.dll` and
friends) are under a binary-only licence that forbids reverse engineering;
they are not shipped. The runtime still loads them if a user puts them next
to `victory.exe`.

DLSS: NVIDIA's `nvngx_dlss.dll` is under NVIDIA's licence and cannot be
shipped with this GPL program; a DLSS backend would load a user-provided copy.
Not wired yet.
