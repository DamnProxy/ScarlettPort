//! AMD FSR 3 upscaling and frame generation on the host, through the PC
//! FidelityFX API (`ffx_api.h`), loaded at run time.
//!
//! The title calls the console FidelityFX upscaler with a
//! `ffxDispatchDescUpscale` (`victory_hle::modules::ffx`). The PC API takes
//! the very same description, so the dispatch is answered by the PC
//! upscaler with the title's numbers and our host copies of its textures.
//!
//! The DLL is not part of this build. It is looked for, in order, at
//! `VICTORY_FFX_DLL`, then next to the executable as
//! `amd_fidelityfx_loader_dx12.dll` (FidelityFX SDK 2.x signed binaries,
//! AMD's binary-only licence) and `amd_fidelityfx_dx12.dll` (SDK 1.1.x,
//! MIT, buildable from source). Without one, the upscale falls back to the
//! plain stretch ([`crate::over`]).
//!
//! Frame generation (`VICTORY_FRAMEGEN=1`) replaces the presenter's swap
//! chain with the API's frame-generation swap chain
//! ([`framegen_swap_chain`]) and, at each upscale, configures generation
//! (the upscaled image before the title's UI as the HUD-less colour) and
//! records its prepare pass. A gate keeps it off under a 15 fps base rate
//! ([`victory_core::upscale::FramegenGate`]).

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use victory_core::upscale::{FramegenGate, HostParams, Plumbing, TitleParams};

// ---------------------------------------------------------------------------
// The API's types (ffx_api.h, ffx_api_types.h, ffx_upscale.h,
// ffx_framegeneration.h, dx12/ffx_api_dx12.h, ffx_api_framegeneration_dx12.h)
// ---------------------------------------------------------------------------

#[repr(C)]
struct Header {
    kind: u64,
    next: *const Header,
}

impl Header {
    fn new(kind: u64) -> Header {
        Header { kind, next: std::ptr::null() }
    }
}

/// `FfxApiResource`: a pointer, a description (type, format, width,
/// height, depth, mips, flags, usage) and a state.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Resource {
    pub resource: *mut c_void,
    pub description: [u32; 8],
    pub state: u32,
}

impl Resource {
    pub(crate) const NONE: Resource = Resource { resource: std::ptr::null_mut(), description: [0; 8], state: 0 };
}

const OK: u32 = 0;

const CREATE_UPSCALE: u64 = 0x0001_0000;
const DISPATCH_UPSCALE: u64 = 0x0001_0001;
const CREATE_BACKEND_DX12: u64 = 0x0000_0002;
const CREATE_FRAMEGEN: u64 = 0x0002_0001;
const CONFIGURE_FRAMEGEN: u64 = 0x0002_0002;
const DISPATCH_FRAMEGEN_PREPARE: u64 = 0x0002_0004;
const CREATE_FRAMEGEN_HUDLESS: u64 = 0x0002_0009;
const CREATE_FGSWAPCHAIN_FOR_HWND_DX12: u64 = 0x0003_0006;

/// `FFX_API_RESOURCE_STATE_*`.
const STATE_UNORDERED: u32 = 1 << 1;
const STATE_COMPUTE_READ: u32 = 1 << 2;
/// `FFX_API_RESOURCE_TYPE_TEXTURE2D`.
const TYPE_TEXTURE2D: u32 = 2;
/// `FFX_API_RESOURCE_USAGE_*`.
const USAGE_UAV: u32 = 1 << 1;
const USAGE_DEPTH: u32 = 1 << 2;
const USAGE_STENCIL: u32 = 1 << 5;

/// `FFX_UPSCALE_ENABLE_DEPTH_INVERTED`, `_DEPTH_INFINITE`, `_DYNAMIC_RESOLUTION`.
const UPSCALE_DEPTH_INVERTED: u32 = 1 << 3;
const UPSCALE_DEPTH_INFINITE: u32 = 1 << 4;
const UPSCALE_DYNAMIC_RESOLUTION: u32 = 1 << 6;
/// `FFX_UPSCALE_ENABLE_MOTION_VECTORS_JITTER_CANCELLATION` and the
/// frame-generation flags of the same meaning.
const UPSCALE_JITTER_CANCELLATION: u32 = 1 << 2;
const FG_JITTER_CANCELLATION: u32 = 1 << 2;
const FG_DEPTH_INVERTED: u32 = 1 << 3;
const FG_DEPTH_INFINITE: u32 = 1 << 4;
const FG_HDR: u32 = 1 << 5;
const UPSCALE_HDR: u32 = 1 << 0;

#[repr(C)]
struct CreateBackendDx12 {
    header: Header,
    device: *mut c_void,
}

#[repr(C)]
struct CreateUpscale {
    header: Header,
    flags: u32,
    max_render: [u32; 2],
    max_upscale: [u32; 2],
    message: Option<extern "C" fn(u32, *const u16)>,
}

/// `ffxDispatchDescUpscale`, as the title lays it out too.
#[repr(C)]
struct DispatchUpscale {
    header: Header,
    list: *mut c_void,
    colour: Resource,
    depth: Resource,
    motion: Resource,
    exposure: Resource,
    reactive: Resource,
    composition: Resource,
    output: Resource,
    jitter: [f32; 2],
    motion_scale: [f32; 2],
    render: [u32; 2],
    upscale: [u32; 2],
    sharpening: bool,
    sharpness: f32,
    frame_ms: f32,
    pre_exposure: f32,
    reset: bool,
    near: f32,
    far: f32,
    fov_y: f32,
    meters: f32,
    flags: u32,
}

#[repr(C)]
struct CreateFramegen {
    header: Header,
    flags: u32,
    display: [u32; 2],
    max_render: [u32; 2],
    back_buffer_format: u32,
}

#[repr(C)]
struct CreateFramegenHudless {
    header: Header,
    hudless_format: u32,
}

#[repr(C)]
struct FgSwapChainForHwnd {
    header: Header,
    swap_chain: *mut *mut c_void,
    hwnd: *mut c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    fullscreen: *const c_void,
    factory: *mut c_void,
    queue: *mut c_void,
}

type FgDispatchCallback = extern "C" fn(*mut c_void, *mut c_void) -> u32;

#[repr(C)]
struct ConfigureFramegen {
    header: Header,
    swap_chain: *mut c_void,
    present_callback: *const c_void,
    present_context: *mut c_void,
    framegen_callback: Option<FgDispatchCallback>,
    framegen_context: *mut c_void,
    enabled: bool,
    allow_async: bool,
    hudless: Resource,
    flags: u32,
    only_generated: bool,
    rect: [i32; 4],
    frame_id: u64,
}

#[repr(C)]
struct DispatchFramegenPrepare {
    header: Header,
    frame_id: u64,
    flags: u32,
    list: *mut c_void,
    render: [u32; 2],
    jitter: [f32; 2],
    motion_scale: [f32; 2],
    frame_ms: f32,
    unused_reset: bool,
    near: f32,
    far: f32,
    fov_y: f32,
    meters: f32,
    depth: Resource,
    motion: Resource,
}

const _: () = {
    use std::mem::offset_of;
    assert!(size_of::<Resource>() == 48);
    assert!(offset_of!(DispatchUpscale, output) == 0x138);
    assert!(offset_of!(DispatchUpscale, jitter) == 0x168);
    assert!(offset_of!(DispatchUpscale, sharpening) == 0x188);
    assert!(offset_of!(DispatchUpscale, flags) == 0x1ac);
    assert!(offset_of!(CreateUpscale, message) == 40);
    assert!(offset_of!(ConfigureFramegen, hudless) == 64);
    assert!(offset_of!(ConfigureFramegen, frame_id) == 136);
    assert!(offset_of!(DispatchFramegenPrepare, depth) == 88);
    assert!(size_of::<DispatchFramegenPrepare>() == 184);
};

type Create = unsafe extern "C" fn(*mut *mut c_void, *const Header, *const c_void) -> u32;
type Destroy = unsafe extern "C" fn(*mut *mut c_void, *const c_void) -> u32;
type Configure = unsafe extern "C" fn(*mut *mut c_void, *const Header) -> u32;
type Dispatch = unsafe extern "C" fn(*mut *mut c_void, *const Header) -> u32;

struct Api {
    create: Create,
    destroy: Destroy,
    configure: Configure,
    dispatch: Dispatch,
}

/// The DLL names tried next to the executable, in order.
pub(crate) const DLLS: [&str; 2] = ["amd_fidelityfx_loader_dx12.dll", "amd_fidelityfx_dx12.dll"];

fn api() -> Option<&'static Api> {
    static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();
    API.get_or_init(|| {
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        let mut tried = Vec::new();
        if let Some(p) = std::env::var_os("VICTORY_FFX_DLL") {
            tried.push(std::path::PathBuf::from(p));
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf())) {
            tried.extend(DLLS.iter().map(|n| dir.join(n)));
        }
        for path in &tried {
            if !path.exists() {
                continue;
            }
            let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
            // SAFETY: loading a DLL the user put there; its exports are the
            // FidelityFX API's C entry points.
            unsafe {
                let Ok(module) = LoadLibraryW(windows::core::PCWSTR(wide.as_ptr())) else {
                    tracing::warn!(path = %path.display(), "the FidelityFX DLL did not load");
                    continue;
                };
                let get = |name: windows::core::PCSTR| GetProcAddress(module, name);
                let (Some(c), Some(d), Some(f), Some(x)) = (
                    get(windows::core::s!("ffxCreateContext")),
                    get(windows::core::s!("ffxDestroyContext")),
                    get(windows::core::s!("ffxConfigure")),
                    get(windows::core::s!("ffxDispatch")),
                ) else {
                    tracing::warn!(path = %path.display(), "the FidelityFX DLL lacks the API's entry points");
                    continue;
                };
                tracing::info!(path = %path.display(), "FidelityFX API loaded (FSR 3 upscaling, frame generation)");
                return Some(Api {
                    create: std::mem::transmute::<_, Create>(c),
                    destroy: std::mem::transmute::<_, Destroy>(d),
                    configure: std::mem::transmute::<_, Configure>(f),
                    dispatch: std::mem::transmute::<_, Dispatch>(x),
                });
            }
        }
        tracing::warn!(
            "no FidelityFX DLL ({} or {} next to the executable, or VICTORY_FFX_DLL): the upscale is a plain stretch",
            DLLS[0],
            DLLS[1]
        );
        None
    })
    .as_ref()
}

/// Whether the API can be had (loads it on first call).
pub(crate) fn available() -> bool {
    api().is_some()
}

extern "C" fn message(kind: u32, text: *const u16) {
    if text.is_null() {
        return;
    }
    // SAFETY: a nul-terminated wide string from the runtime.
    let s = unsafe {
        let mut n = 0;
        while *text.add(n) != 0 && n < 4096 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, n))
    };
    if kind == 0 {
        tracing::error!(message = s, "FidelityFX");
    } else {
        tracing::warn!(message = s, "FidelityFX");
    }
}

// ---------------------------------------------------------------------------
// Formats and resource descriptions
// ---------------------------------------------------------------------------

/// `FfxApiSurfaceFormat` for a DXGI format, as `ffxApiGetSurfaceFormatDX12`
/// maps it (depth-stencil formats read as their depth or stencil plane).
pub(crate) fn ffx_format(dxgi: i32) -> u32 {
    match dxgi {
        1 => 1,                 // R32G32B32A32_TYPELESS
        2 => 3,                 // R32G32B32A32_FLOAT
        3 => 2,                 // R32G32B32A32_UINT
        9 => 30,                // R16G16B16A16_TYPELESS
        10 => 4,                // R16G16B16A16_FLOAT
        6 => 5,                 // R32G32B32_FLOAT
        15 => 31,               // R32G32_TYPELESS
        16 => 6,                // R32G32_FLOAT
        17 => 38,               // R32G32_UINT
        19..=21 => 28,          // R32G8X24 / D32S8 / R32_X8X24: R32_FLOAT
        22 | 47 => 7,           // the stencil planes: R8_UINT
        23 => 32,               // R10G10B10A2_TYPELESS
        24 => 17,               // R10G10B10A2_UNORM
        26 => 16,               // R11G11B10_FLOAT
        27 => 9,                // R8G8B8A8_TYPELESS
        28 => 10,               // R8G8B8A8_UNORM
        29 => 12,               // R8G8B8A8_UNORM_SRGB
        31 => 11,               // R8G8B8A8_SNORM
        33 => 33,               // R16G16_TYPELESS
        34 => 18,               // R16G16_FLOAT
        36 => 19,               // R16G16_UINT
        38 => 20,               // R16G16_SINT
        39 => 37,               // R32_TYPELESS
        40 | 41 => 28,          // D32_FLOAT, R32_FLOAT
        42 => 8,                // R32_UINT
        44..=46 => 8,           // R24G8 / D24S8 / R24_X8: R32_UINT
        48 => 36,               // R8G8_TYPELESS
        49 => 26,               // R8G8_UNORM
        50 => 27,               // R8G8_UINT
        53 => 34,               // R16_TYPELESS
        54 => 21,               // R16_FLOAT
        55 | 56 => 23,          // D16_UNORM, R16_UNORM
        57 => 22,               // R16_UINT
        58 => 24,               // R16_SNORM
        60 => 35,               // R8_TYPELESS
        61 => 25,               // R8_UNORM
        62 => 7,                // R8_UINT
        63 => 39,               // R8_SNORM
        67 => 29,               // R9G9B9E5_SHAREDEXP
        87 => 14,               // B8G8R8A8_UNORM
        90 => 13,               // B8G8R8A8_TYPELESS
        91 => 15,               // B8G8R8A8_UNORM_SRGB
        _ => 0,
    }
}

/// The description `ffxApiGetResourceDX12` makes for a 2D texture.
pub(crate) fn describe(format: i32, width: u64, height: u32, depth_or_array: u16, mips: u16, uav: bool) -> [u32; 8] {
    let mut usage = match format {
        40 | 55 => USAGE_DEPTH,
        20 | 45 => USAGE_DEPTH | USAGE_STENCIL,
        _ => 0,
    };
    if uav {
        usage |= USAGE_UAV;
    }
    [
        TYPE_TEXTURE2D,
        ffx_format(format),
        width as u32,
        height,
        u32::from(depth_or_array),
        u32::from(mips),
        0,
        usage,
    ]
}

fn resource(r: &ID3D12Resource, state: u32) -> Resource {
    // SAFETY: a live resource.
    let d = unsafe { r.GetDesc() };
    Resource {
        resource: r.as_raw(),
        description: describe(
            d.Format.0,
            d.Width,
            d.Height,
            d.DepthOrArraySize,
            d.MipLevels,
            d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0,
        ),
        state,
    }
}

// ---------------------------------------------------------------------------
// The upscaler
// ---------------------------------------------------------------------------

struct Context {
    ctx: *mut c_void,
    max_render: [u32; 2],
    max_upscale: [u32; 2],
    flags: u32,
}

struct Framegen {
    ctx: *mut c_void,
    display: [u32; 2],
    gate: FramegenGate,
    frame_id: u64,
}

#[derive(Default)]
struct State {
    upscale: Option<Context>,
    framegen: Option<Framegen>,
    framegen_failed: bool,
    plumbing: Plumbing,
    last: Option<std::time::Instant>,
}

// SAFETY: the contexts are the API's, usable from any thread one at a time
// (the mutex).
unsafe impl Send for State {}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// The title's upscale context flags, from its `ffxCreateContext`
/// (`victory_hle::modules::ffx`); `u32::MAX` until it made one.
static TITLE_FLAGS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// Records the flags the title made its upscale context with.
pub fn set_title_flags(flags: u32) {
    TITLE_FLAGS.store(flags, Ordering::Relaxed);
}

/// The flags our context is made with: the title's (it knows its depth and
/// colour), or reversed depth when it made none we saw; always dynamic
/// resolution (the render size can change under a fixed maximum).
pub(crate) fn context_flags(title: u32) -> u32 {
    let base = if title == u32::MAX { UPSCALE_DEPTH_INVERTED } else { title & 0x1ff };
    base | UPSCALE_DYNAMIC_RESOLUTION
}

/// The textures the title hands its upscale, as host resources.
pub(crate) struct Inputs<'a> {
    pub colour: &'a ID3D12Resource,
    pub depth: Option<&'a ID3D12Resource>,
    pub motion: Option<&'a ID3D12Resource>,
    pub exposure: Option<&'a ID3D12Resource>,
    pub reactive: Option<&'a ID3D12Resource>,
    pub composition: Option<&'a ID3D12Resource>,
    pub output: &'a ID3D12Resource,
}

/// Records FSR's upscale of `i` into `list`. The inputs must be in the
/// non-pixel shader resource state and the output in unordered access.
/// False if nothing was recorded (no DLL, no context, a refused dispatch).
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    i: &Inputs<'_>,
    t: &TitleParams,
) -> bool {
    let Some(api) = api() else { return false };
    if i.depth.is_none() || i.motion.is_none() {
        static TOLD: std::sync::Once = std::sync::Once::new();
        TOLD.call_once(|| tracing::warn!("the title's upscale came without depth or motion: stretched instead"));
        return false;
    }
    // SAFETY: live resources.
    let (cd, od) = unsafe { (i.colour.GetDesc(), i.output.GetDesc()) };
    let colour_size = [cd.Width as u32, cd.Height];
    let output_size = [od.Width as u32, od.Height];
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let st = guard.get_or_insert_with(State::default);
    let switched = victory_core::upscale::take_reset();
    let h = st.plumbing.host(t, colour_size, output_size, victory_core::upscale::live_sharpness(), switched);
    match st.plumbing.watch.see(h.jitter) {
        victory_core::upscale::JitterVerdict::Fine => {}
        v => {
            static TOLD: std::sync::Once = std::sync::Once::new();
            TOLD.call_once(|| tracing::warn!(verdict = ?v, jitter = ?h.jitter, "the title's camera jitter looks wrong: FSR will blur"));
        }
    }
    let flags = context_flags(TITLE_FLAGS.load(Ordering::Relaxed));
    let fits = st.upscale.as_ref().is_some_and(|c| {
        c.flags == flags
            && c.max_upscale == output_size
            && c.max_render[0] >= colour_size[0]
            && c.max_render[1] >= colour_size[1]
    });
    if !fits {
        if let Some(mut old) = st.upscale.take() {
            // SAFETY: our context; the frames using it were recorded, and
            // the API waits for the GPU on destruction.
            unsafe { (api.destroy)(&mut old.ctx, std::ptr::null()) };
        }
        let backend = CreateBackendDx12 { header: Header::new(CREATE_BACKEND_DX12), device: device.as_raw() };
        let mut desc = CreateUpscale {
            header: Header::new(CREATE_UPSCALE),
            flags,
            max_render: colour_size,
            max_upscale: output_size,
            message: Some(message),
        };
        desc.header.next = std::ptr::from_ref(&backend).cast();
        let mut ctx = std::ptr::null_mut();
        // SAFETY: descriptions that live through the call.
        let rc = unsafe { (api.create)(&mut ctx, std::ptr::from_ref(&desc).cast(), std::ptr::null()) };
        if rc != OK || ctx.is_null() {
            static TOLD: std::sync::Once = std::sync::Once::new();
            TOLD.call_once(|| tracing::error!(rc, ?colour_size, ?output_size, "FSR context not made: stretched instead"));
            return false;
        }
        tracing::info!(?colour_size, ?output_size, flags = format_args!("{flags:#x}"), "FSR upscale context made");
        st.upscale = Some(Context { ctx, max_render: colour_size, max_upscale: output_size, flags });
    }
    let opt = |r: Option<&ID3D12Resource>| r.map_or(Resource::NONE, |r| resource(r, STATE_COMPUTE_READ));
    let desc = DispatchUpscale {
        header: Header::new(DISPATCH_UPSCALE),
        list: list.as_raw(),
        colour: resource(i.colour, STATE_COMPUTE_READ),
        depth: opt(i.depth),
        motion: opt(i.motion),
        exposure: opt(i.exposure),
        reactive: opt(i.reactive),
        composition: opt(i.composition),
        output: resource(i.output, STATE_UNORDERED),
        jitter: h.jitter,
        motion_scale: h.motion_scale,
        render: h.render,
        upscale: h.upscale,
        sharpening: h.sharpening,
        sharpness: h.sharpness,
        frame_ms: h.frame_ms,
        pre_exposure: h.pre_exposure,
        reset: h.reset,
        near: h.near,
        far: h.far,
        fov_y: h.fov_y,
        meters: h.meters,
        flags: h.flags & 0x7,
    };
    let Some(c) = st.upscale.as_mut() else { return false };
    // SAFETY: our context and a description that lives through the call.
    let rc = unsafe { (api.dispatch)(&mut c.ctx, std::ptr::from_ref(&desc).cast()) };
    if rc != OK {
        static TOLD: std::sync::Once = std::sync::Once::new();
        TOLD.call_once(|| tracing::error!(rc, "FSR dispatch refused: stretched instead"));
        return false;
    }
    framegen_frame(api, device, list, st, i, &h, flags);
    victory_core::upscale::status::set_answered(1, h.render, h.upscale);
    true
}

// ---------------------------------------------------------------------------
// Frame generation
// ---------------------------------------------------------------------------

/// The frame-generation swap chain, once made: its raw pointer (the
/// presenter holds a reference).
static FG_SWAP_CHAIN: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FG_SWAP_FORMAT: AtomicU64 = AtomicU64::new(0);
/// Our frame-generation context, for the dispatch callback.
static FG_CONTEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// Generated frames are being presented now.
static FG_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Presents per rendered frame now: 2 while frame generation runs, else 1.
/// The pacer (`VICTORY_PACE_FPS`, an output rate) holds base frames this
/// many periods.
pub(crate) fn framegen_factor() -> u64 {
    if FG_ACTIVE.load(Ordering::Relaxed) { 2 } else { 1 }
}

/// With `VICTORY_FRAMEGEN=1` and the API at hand: the API's
/// frame-generation swap chain for `hwnd`, made in place of a plain one.
/// None otherwise (the caller makes its own).
pub(crate) fn framegen_swap_chain(
    factory: &IDXGIFactory2,
    queue: &ID3D12CommandQueue,
    hwnd: windows::Win32::Foundation::HWND,
    desc: &DXGI_SWAP_CHAIN_DESC1,
) -> Option<IDXGISwapChain3> {
    if !victory_core::upscale::switches().framegen {
        return None;
    }
    let Some(api) = api() else {
        victory_core::upscale::status::set_framegen(3);
        tracing::warn!("VICTORY_FRAMEGEN=1 but no FidelityFX DLL: no frame generation");
        return None;
    };
    let mut sc: *mut c_void = std::ptr::null_mut();
    let d = FgSwapChainForHwnd {
        header: Header::new(CREATE_FGSWAPCHAIN_FOR_HWND_DX12),
        swap_chain: &mut sc,
        hwnd: hwnd.0,
        desc,
        fullscreen: std::ptr::null(),
        factory: factory.as_raw(),
        queue: queue.as_raw(),
    };
    let mut ctx = std::ptr::null_mut();
    // SAFETY: descriptions that live through the call; the API returns a
    // swap chain with one reference we take.
    let rc = unsafe { (api.create)(&mut ctx, std::ptr::from_ref(&d).cast(), std::ptr::null()) };
    if rc != OK || sc.is_null() {
        victory_core::upscale::status::set_framegen(3);
        tracing::error!(rc, "frame-generation swap chain not made: presenting without frame generation");
        return None;
    }
    // SAFETY: an IDXGISwapChain4 whose reference is ours.
    let chain = unsafe { IDXGISwapChain4::from_raw(sc) };
    FG_SWAP_CHAIN.store(sc, Ordering::Relaxed);
    FG_SWAP_FORMAT.store(desc.Format.0 as u64, Ordering::Relaxed);
    tracing::info!("frame-generation swap chain made (FSR 3); generation starts with the first upscale at >= 15 fps");
    chain.cast::<IDXGISwapChain3>().ok()
}

extern "C" fn framegen_dispatch(params: *mut c_void, _user: *mut c_void) -> u32 {
    let Some(api) = api() else { return 1 };
    let mut ctx = FG_CONTEXT.load(Ordering::Relaxed);
    if ctx.is_null() {
        return 1;
    }
    // SAFETY: the API hands its own dispatch description back.
    unsafe { (api.dispatch)(&mut ctx, params.cast()) }
}

fn framegen_frame(
    api: &Api,
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    st: &mut State,
    i: &Inputs<'_>,
    h: &HostParams,
    upscale_flags: u32,
) {
    let wanted = victory_core::upscale::live_framegen();
    let swap_chain = FG_SWAP_CHAIN.load(Ordering::Relaxed);
    let now = std::time::Instant::now();
    let base_ms = st.last.map_or(0.0, |l| (now - l).as_secs_f32() * 1000.0);
    st.last = Some(now);
    if swap_chain.is_null() || st.framegen_failed {
        FG_ACTIVE.store(false, Ordering::Relaxed);
        if wanted {
            victory_core::upscale::status::set_framegen(3);
        }
        return;
    }
    if st.framegen.as_ref().is_some_and(|f| f.display != h.upscale) {
        if let Some(mut old) = st.framegen.take() {
            FG_CONTEXT.store(std::ptr::null_mut(), Ordering::Relaxed);
            // SAFETY: our context.
            unsafe { (api.destroy)(&mut old.ctx, std::ptr::null()) };
        }
    }
    if st.framegen.is_none() {
        let backbuffer = ffx_format(FG_SWAP_FORMAT.load(Ordering::Relaxed) as i32);
        // SAFETY: a live resource.
        let hudless_format = ffx_format(unsafe { i.output.GetDesc() }.Format.0);
        let mut flags = 0;
        if upscale_flags & UPSCALE_DEPTH_INVERTED != 0 {
            flags |= FG_DEPTH_INVERTED;
        }
        if upscale_flags & UPSCALE_DEPTH_INFINITE != 0 {
            flags |= FG_DEPTH_INFINITE;
        }
        if upscale_flags & UPSCALE_JITTER_CANCELLATION != 0 {
            flags |= FG_JITTER_CANCELLATION;
        }
        if upscale_flags & UPSCALE_HDR != 0 {
            flags |= FG_HDR;
        }
        let backend = CreateBackendDx12 { header: Header::new(CREATE_BACKEND_DX12), device: device.as_raw() };
        let mut hudless = CreateFramegenHudless { header: Header::new(CREATE_FRAMEGEN_HUDLESS), hudless_format };
        hudless.header.next = std::ptr::from_ref(&backend).cast();
        let mut desc = CreateFramegen {
            header: Header::new(CREATE_FRAMEGEN),
            flags,
            display: h.upscale,
            max_render: st.upscale.as_ref().map_or(h.render, |c| c.max_render),
            back_buffer_format: backbuffer,
        };
        desc.header.next = if hudless_format != backbuffer {
            std::ptr::from_ref(&hudless).cast()
        } else {
            std::ptr::from_ref(&backend).cast()
        };
        let mut ctx = std::ptr::null_mut();
        // SAFETY: descriptions that live through the call.
        let rc = unsafe { (api.create)(&mut ctx, std::ptr::from_ref(&desc).cast(), std::ptr::null()) };
        if rc != OK || ctx.is_null() {
            tracing::error!(rc, "frame-generation context not made: no frame generation");
            st.framegen_failed = true;
            victory_core::upscale::status::set_framegen(3);
            return;
        }
        FG_CONTEXT.store(ctx, Ordering::Relaxed);
        st.framegen = Some(Framegen { ctx, display: h.upscale, gate: FramegenGate::default(), frame_id: 0 });
    }
    let Some(fg) = st.framegen.as_mut() else { return };
    // The base rate is measured with generation running too: these are
    // the title's frames, one per upscale.
    let on = fg.gate.frame(wanted, base_ms);
    FG_ACTIVE.store(on, Ordering::Relaxed);
    victory_core::upscale::status::set_framegen(match (wanted, on) {
        (_, true) => 1,
        (true, false) => 2,
        _ => 0,
    });
    fg.frame_id += 1;
    // SAFETY: a live resource.
    let od = unsafe { i.output.GetDesc() };
    let cfg = ConfigureFramegen {
        header: Header::new(CONFIGURE_FRAMEGEN),
        swap_chain,
        present_callback: std::ptr::null(),
        present_context: std::ptr::null_mut(),
        framegen_callback: Some(framegen_dispatch),
        framegen_context: std::ptr::null_mut(),
        enabled: on,
        allow_async: false,
        // The upscaled image is the frame before the title's UI is drawn.
        hudless: resource(i.output, STATE_UNORDERED),
        flags: 0,
        only_generated: false,
        rect: [0, 0, od.Width as i32, od.Height as i32],
        frame_id: fg.frame_id,
    };
    // SAFETY: our context and a description that lives through the call.
    let rc = unsafe { (api.configure)(&mut fg.ctx, std::ptr::from_ref(&cfg).cast()) };
    if rc != OK {
        static TOLD: std::sync::Once = std::sync::Once::new();
        TOLD.call_once(|| tracing::error!(rc, "frame-generation configure refused"));
        return;
    }
    if !on {
        return;
    }
    let (Some(depth), Some(motion)) = (i.depth, i.motion) else { return };
    let prep = DispatchFramegenPrepare {
        header: Header::new(DISPATCH_FRAMEGEN_PREPARE),
        frame_id: fg.frame_id,
        flags: 0,
        list: list.as_raw(),
        render: h.render,
        jitter: h.jitter,
        motion_scale: h.motion_scale,
        frame_ms: h.frame_ms,
        unused_reset: h.reset,
        near: h.near,
        far: h.far,
        fov_y: h.fov_y,
        meters: h.meters,
        depth: resource(depth, STATE_COMPUTE_READ),
        motion: resource(motion, STATE_COMPUTE_READ),
    };
    // SAFETY: as above.
    let rc = unsafe { (api.dispatch)(&mut fg.ctx, std::ptr::from_ref(&prep).cast()) };
    if rc != OK {
        static TOLD: std::sync::Once = std::sync::Once::new();
        TOLD.call_once(|| tracing::error!(rc, "frame-generation prepare refused"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_formats_the_title_feeds_map_as_the_api_maps_them() {
        assert_eq!(ffx_format(10), 4); // HDR colour, R16G16B16A16_FLOAT
        assert_eq!(ffx_format(28), 10); // LDR colour
        assert_eq!(ffx_format(24), 17); // R10G10B10A2_UNORM
        assert_eq!(ffx_format(34), 18); // velocity, R16G16_FLOAT
        assert_eq!(ffx_format(19), 28); // main depth R32G8X24 reads as R32_FLOAT
        assert_eq!(ffx_format(20), 28);
        assert_eq!(ffx_format(40), 28);
        assert_eq!(ffx_format(41), 28); // linear depth
        assert_eq!(ffx_format(87), 14);
        assert_eq!(ffx_format(999), 0);
    }

    #[test]
    fn descriptions_are_the_helper_s() {
        let d = describe(20, 1280, 720, 1, 1, false);
        assert_eq!(d, [TYPE_TEXTURE2D, 28, 1280, 720, 1, 1, 0, USAGE_DEPTH | USAGE_STENCIL]);
        let d = describe(10, 1920, 1080, 1, 1, true);
        assert_eq!(d[1], 4);
        assert_eq!(d[7], USAGE_UAV);
    }

    #[test]
    fn context_flags_follow_the_title_and_allow_resizing() {
        assert_eq!(context_flags(u32::MAX), UPSCALE_DEPTH_INVERTED | UPSCALE_DYNAMIC_RESOLUTION);
        assert_eq!(context_flags(0x1), UPSCALE_HDR | UPSCALE_DYNAMIC_RESOLUTION);
        // Debug bits past the known ones are dropped.
        assert_eq!(context_flags(0xffff_0000), UPSCALE_DYNAMIC_RESOLUTION);
    }

    #[test]
    fn the_dispatch_types_are_the_api_s() {
        // FFX_API_MAKE_EFFECT_SUB_ID(effect, sub).
        let make = |effect: u64, sub: u64| (effect & 0x00ff_0000) | (sub & !0x00ff_0000);
        assert_eq!(CREATE_UPSCALE, make(0x0001_0000, 0));
        assert_eq!(DISPATCH_UPSCALE, make(0x0001_0000, 1));
        assert_eq!(CREATE_FRAMEGEN, make(0x0002_0000, 1));
        assert_eq!(CONFIGURE_FRAMEGEN, make(0x0002_0000, 2));
        assert_eq!(DISPATCH_FRAMEGEN_PREPARE, make(0x0002_0000, 4));
        assert_eq!(CREATE_FRAMEGEN_HUDLESS, make(0x0002_0000, 9));
        assert_eq!(CREATE_FGSWAPCHAIN_FOR_HWND_DX12, make(0x0003_0000, 6));
    }

    #[test]
    fn framegen_doubles_the_pacing_period_only_while_it_runs() {
        assert_eq!(framegen_factor(), 1);
        FG_ACTIVE.store(true, Ordering::Relaxed);
        assert_eq!(framegen_factor(), 2);
        FG_ACTIVE.store(false, Ordering::Relaxed);
    }
}
