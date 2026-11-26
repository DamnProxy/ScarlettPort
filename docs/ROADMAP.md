# Roadmap: 30 fps on a mid-range NVIDIA GPU

Current state is in [STATUS.md](STATUS.md): the lit world at about 5 fps in the cell scene and about 4 fps while moving on an RTX 3060 (Series S profile), with stable frame pacing, shadowed areas too dark, the intro cutscene very slow unless skipped, not yet verified playable. The target is 30 fps in the open world on a card such as an RTX 3060 with a correct image. Items are ordered roughly by what blocks the next one; none has a date.

## 1. Light in shadow

The title's ray-traced bounce light and shadows do not run yet, so anything the sun does not reach directly is too dark. The base has started this: its structure-builder programs translate, title memory can be read by address from shaders, and the ray/box and ray/triangle tests are emulated. See `runtime/victory/docs/fork-ray-tracing.md`.

- **Off** (current default): ray queries return no hit.
- **On**: map the console's ray-tracing structures to DXR or keep the emulated traversal, and make it fast enough to use.

## 2. Frame time

About 4-5 fps means roughly 200-250 ms per frame. Done in v0.4.0: the yard stutter (lazy remap plus the copy fix) and memory copies cut from 286 to 84 MB per frame. Remaining candidates: title-memory copies, submission waits (the opt-in relay), shader efficiency, and the 64-lane programs that run as two 32-lane halves on NVIDIA.

## 3. Intro cutscene

The intro is very slow unless it is skipped. Find out why before judging overall speed.

## 4. Correctness of the world

Some cards lose the first room's walls and roof with certain switches; shader clean-up passes are off by default for that reason. Find the cause so they can be turned on.

## 5. Ahead-of-time shader and pipeline build

First-run shader compiles and pipeline creation stall the world (pipeline compile freezes when moving on first runs). v0.4.0 adds a shader prewarm. XCL has a pipeline journal and an offline build; the VIctory-based runtime caches translated shaders on disk. The aim is a warm-up that finishes before the first frame.

## 6. Series S profile and memory

Run the title the way the smaller console does (its memory budget, resolution and feature set), which also lowers memory use, and keep streaming within the VRAM of a 12 GB card.

## 7. Verify playable

Play a session end to end, with saves, and record what breaks.

## Changelog

### v0.4.0

Runtime synced to fx-cand4 (efcc7b9); launcher 0.4.0. Measured on an RTX 3060 (Series S profile): about 5 fps in the cell scene and about 4 fps while moving, with stable frame pacing.

- **Fixed:** the yard stutter (lazy remap plus the copy fix).
- **Faster:** memory copies cut from 286 to 84 MB per frame.
- **Fixed:** fast boot (still optional, off in the presets).
- **New:** in-session A/B tooling (live switches cycled within one session).
- **New:** shader prewarm.
- **Launcher:** the Stable preset uses the fx-cand4 settings (precise syncs and settles, precompiled graphics, read fix, read-back-only copies, narrow root/views/BVH, prepared barriers/tables/reuse, lazy remap, sub-page memory, no-query copies, 4 GB card buffers).
- **Known issues:** lighting and hair look wrong (ray tracing is off); pipeline compile freezes when moving on first runs.

### Still in progress

- **In testing:** further streaming work (narrower declared write ranges, fills and copies served on the card).
- **In testing:** missing-geometry fixes: character skin shaders, mesh shaders linked under the 32 KiB output limit, view heap size on NVIDIA.
- **In testing:** CPU recording cuts, with a per-part preparing line for measuring them.
- **In testing:** save-safety fix.
- **In testing:** one-command benchmark queue for A/B runs.
- **Planned:** FSR upscaler (AMD PC FidelityFX) and frame generation.

### v0.2.1

- Runtime synced to the verified default build; launcher with Discord status, updates from Releases and hardware report.
