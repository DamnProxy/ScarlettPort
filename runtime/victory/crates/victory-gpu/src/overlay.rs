//! The settings overlay on the window side: samples the keyboard and mouse
//! each present, runs `victory_core::overlay`, and draws what it produces
//! into the present's own copy list, after the game's frame and the splash,
//! with a renderer small enough to read at a glance.
//!
//! The renderer is one draw: the overlay's triangles (position in pixels,
//! atlas coordinates, premultiplied colour) from an upload buffer, the
//! font atlas (an 8-bit texture of 128 by 48 texels, uploaded once with the
//! first frame that shows it) sampled with points, blended over the back
//! buffer. No GPU waits: a vertex buffer is reused only once the submission
//! that read it has finished (`FINISHED`, the same count the present's
//! other lists go by), else another is made.
//!
//! shortcut: any back buffer format is drawn to as it is (an 8-bit one is
//! what the title presents in its Series S mode); the colours are not
//! converted for a float or 10-bit buffer.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use victory_core::overlay::gui::Vertex;
use victory_core::overlay::perf::SysInfo;
use victory_core::overlay::store::WindowMode;
use victory_core::overlay::{self, font};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32G32_FLOAT,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, ScreenToClient,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::*;

const SOURCE: &str = r#"
cbuffer Screen : register(b0) { float2 scale; };
Texture2D<float> atlas : register(t0);
SamplerState point_sampler : register(s0);
struct In { float2 pos : POSITION; float2 uv : TEXCOORD0; float4 color : COLOR0; };
struct Out { float4 sv : SV_Position; float2 uv : TEXCOORD0; float4 color : COLOR0; };
Out vs(In i) {
    Out o;
    o.sv = float4(i.pos.x * scale.x - 1.0, 1.0 - i.pos.y * scale.y, 0.0, 1.0);
    o.uv = i.uv;
    o.color = i.color;
    return o;
}
float4 ps(Out i) : SV_Target { return i.color * atlas.Sample(point_sampler, i.uv); }
"#;

const STRIDE: u32 = size_of::<Vertex>() as u32;
/// Vertex buffers in flight at most; a frame past that draws no overlay.
const MAX_SLOTS: usize = 8;

struct Slot {
    buffer: ID3D12Resource,
    ptr: *mut u8,
    bytes: usize,
    /// The submission that last read it (0: none yet).
    flown: u64,
}

struct Pass {
    format: i32,
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    targets: ID3D12DescriptorHeap,
    atlas: ID3D12Resource,
    /// Holds the atlas texels until the copy has run (kept for good: 12 KB).
    upload: ID3D12Resource,
    /// The atlas copy has been recorded.
    uploaded: bool,
    slots: Vec<Slot>,
}

// SAFETY: D3D12 objects are free-threaded; the mapped pointers belong to
// their slots and are written under the pass's lock.
unsafe impl Send for Pass {}

static PASS: Mutex<Option<Pass>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn make(device: &ID3D12Device, format: DXGI_FORMAT) -> windows::core::Result<Pass> {
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::core::s;
    let vs = crate::host::compile(SOURCE, s!("vs"), s!("vs_5_1"))?;
    let ps = crate::host::compile(SOURCE, s!("ps"), s!("ps_5_1"))?;
    let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
        pShaderBytecode: bytes.as_ptr().cast(),
        BytecodeLength: bytes.len(),
    };
    // Root constants (the scale to clip space) for the vertex stage, one
    // texture for the pixel stage, a point sampler; serialized by the
    // runtime, as `over` does, not by the shader compiler.
    let range = [D3D12_DESCRIPTOR_RANGE {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
        NumDescriptors: 1,
        BaseShaderRegister: 0,
        RegisterSpace: 0,
        OffsetInDescriptorsFromTableStart: 0,
    }];
    let params = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 2,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_VERTEX,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: range.as_ptr(),
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        },
    ];
    let sampler = [D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        MaxLOD: f32::MAX,
        ShaderRegister: 0,
        RegisterSpace: 0,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        ..Default::default()
    }];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: params.len() as u32,
        pParameters: params.as_ptr(),
        NumStaticSamplers: 1,
        pStaticSamplers: sampler.as_ptr(),
        Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
    };
    let mut blob: Option<ID3DBlob> = None;
    // SAFETY: the description points at live data; the blob is read while it lives.
    let root: ID3D12RootSignature = unsafe {
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)?;
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        device.CreateRootSignature(0, bytes)?
    };
    let element =
        |name: windows::core::PCSTR, format: DXGI_FORMAT, offset: u32| D3D12_INPUT_ELEMENT_DESC {
            SemanticName: name,
            SemanticIndex: 0,
            Format: format,
            InputSlot: 0,
            AlignedByteOffset: offset,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        };
    let layout = [
        element(s!("POSITION"), DXGI_FORMAT_R32G32_FLOAT, 0),
        element(s!("TEXCOORD"), DXGI_FORMAT_R32G32_FLOAT, 8),
        element(s!("COLOR"), DXGI_FORMAT_R8G8B8A8_UNORM, 16),
    ];
    // SAFETY: the description points at live data and never releases its pointer.
    let pipeline: ID3D12PipelineState = unsafe {
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            VS: code(&vs[..]),
            PS: code(&ps[..]),
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: layout.as_ptr(),
                NumElements: layout.len() as u32,
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        desc.RTVFormats[0] = format;
        // Premultiplied: the colours already carry their alpha.
        desc.BlendState.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        device.CreateGraphicsPipelineState(&desc)?
    };
    let views: ID3D12DescriptorHeap = unsafe {
        device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: 1,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?
    };
    let targets: ID3D12DescriptorHeap = unsafe {
        device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: 1,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        })?
    };
    // The atlas texture and the buffer that carries its texels up.
    let tex_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(font::ATLAS_W),
        Height: font::ATLAS_H,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        ..Default::default()
    };
    // SAFETY: plain creation; the upload buffer is mapped and written inside its size.
    let (atlas, upload) = unsafe {
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        device.GetCopyableFootprints(
            &tex_desc,
            0,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        );
        let mut atlas: Option<ID3D12Resource> = None;
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &tex_desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut atlas,
        )?;
        let upload = buffer(
            device,
            total,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )?;
        let mut mapped = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut mapped))?;
        let texels = font::atlas();
        let pitch = footprint.Footprint.RowPitch as usize;
        for row in 0..font::ATLAS_H as usize {
            std::ptr::copy_nonoverlapping(
                texels.as_ptr().add(row * font::ATLAS_W as usize),
                mapped
                    .cast::<u8>()
                    .add(footprint.Offset as usize + row * pitch),
                font::ATLAS_W as usize,
            );
        }
        upload.Unmap(0, None);
        (atlas.ok_or_else(windows::core::Error::empty)?, upload)
    };
    // SAFETY: a texture of ours and a slot of our heap.
    unsafe {
        device.CreateShaderResourceView(
            &atlas,
            Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R8_UNORM,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            }),
            views.GetCPUDescriptorHandleForHeapStart(),
        );
    }
    Ok(Pass {
        format: format.0,
        root,
        pipeline,
        views,
        targets,
        atlas,
        upload,
        uploaded: false,
        slots: Vec::new(),
    })
}

/// A buffer of `size` bytes in a heap of `kind`.
fn buffer(
    device: &ID3D12Device,
    size: u64,
    kind: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
) -> windows::core::Result<ID3D12Resource> {
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size.max(256),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            state,
            None,
            &mut out,
        )?;
    }
    out.ok_or_else(windows::core::Error::empty)
}

/// A vertex buffer slot for `bytes`: one the card has finished with (made
/// larger when it is too small), else a new one; `None` at the limit.
fn take_slot(device: &ID3D12Device, pass: &mut Pass, bytes: usize, finished: u64) -> Option<usize> {
    let make_slot = || -> Option<Slot> {
        let want = (bytes.max(64 * 1024)).next_power_of_two();
        let buf = buffer(
            device,
            want as u64,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )
        .ok()?;
        let mut mapped = std::ptr::null_mut();
        // SAFETY: an upload buffer of ours, mapped for good.
        unsafe { buf.Map(0, None, Some(&mut mapped)).ok()? };
        Some(Slot {
            buffer: buf,
            ptr: mapped.cast(),
            bytes: want,
            flown: 0,
        })
    };
    match pass.slots.iter().position(|s| s.flown <= finished) {
        Some(i) => {
            // The card is done with it: made larger if this frame needs more.
            if pass.slots[i].bytes < bytes {
                pass.slots[i] = make_slot()?;
            }
            Some(i)
        }
        None if pass.slots.len() < MAX_SLOTS => {
            pass.slots.push(make_slot()?);
            Some(pass.slots.len() - 1)
        }
        None => None,
    }
}

/// That the submission `flown` carries the list that drew with `slot`: the
/// buffer is not reused before it has finished.
pub(crate) fn sent(slot: usize, flown: u64) {
    if let Some(p) = lock(&PASS).as_mut()
        && let Some(s) = p.slots.get_mut(slot)
    {
        s.flown = flown;
    }
}

// ---------------------------------------------------------------------------
// Input and system figures
// ---------------------------------------------------------------------------

/// Keys the overlay watches while it is open: everything but the generic
/// modifiers (their left and right keys are reported) and the first codes
/// that are mouse buttons, which are read separately.
fn open_keys() -> impl Iterator<Item = u8> {
    [0x01u8, 0x02, 0x04, 0x05, 0x06]
        .into_iter()
        .chain((0x08..=0xFE_u8).filter(|vk| !(0x10..=0x12).contains(vk)))
}

fn down(vk: u8) -> bool {
    // SAFETY: a plain query.
    unsafe { GetAsyncKeyState(i32::from(vk)) < 0 }
}

/// Whether a window of this process is the one in front.
fn ours_in_front() -> bool {
    // SAFETY: plain queries.
    unsafe {
        let front = GetForegroundWindow();
        if front.0.is_null() {
            return false;
        }
        let mut process = 0;
        GetWindowThreadProcessId(front, Some(&mut process));
        process == windows::Win32::System::Threading::GetCurrentProcessId()
    }
}

fn pointer(hwnd: HWND, size: (u32, u32)) -> (f32, f32) {
    let mut p = POINT::default();
    let mut client = RECT::default();
    // SAFETY: plain queries on our window.
    unsafe {
        if GetCursorPos(&mut p).is_err()
            || !ScreenToClient(hwnd, &mut p).as_bool()
            || GetClientRect(hwnd, &mut client).is_err()
        {
            return (-1.0, -1.0);
        }
    }
    // The swap chain is stretched over the client area.
    let (cw, ch) = (
        (client.right - client.left).max(1) as f32,
        (client.bottom - client.top).max(1) as f32,
    );
    (
        p.x as f32 * size.0 as f32 / cw,
        p.y as f32 * size.1 as f32 / ch,
    )
}

/// Memory figures, read at most twice a second and only while the overlay shows them.
fn sys_info(video: &dyn Fn() -> Option<[u64; 4]>) -> Option<SysInfo> {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = lock(&LAST);
        if last.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return None;
        }
        *last = Some(Instant::now());
    }
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut s = SysInfo::default();
    // SAFETY: plain data the calls fill in, with their sizes set.
    unsafe {
        let mut c = PROCESS_MEMORY_COUNTERS {
            cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
            s.ram_working_set = c.WorkingSetSize as u64;
            s.ram_private = c.PagefileUsage as u64;
        }
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) != 0 {
            s.ram_available = m.ullAvailPhys;
            s.ram_total = m.ullTotalPhys;
        }
    }
    if let Some([budget, used, shared_budget, shared_used]) = video() {
        (
            s.vram_budget,
            s.vram_used,
            s.shared_budget,
            s.shared_used,
            s.vram_known,
        ) = (budget, used, shared_budget, shared_used, true);
    }
    Some(s)
}

// ---------------------------------------------------------------------------
// The window's mode
// ---------------------------------------------------------------------------

struct Framed {
    style: isize,
    placement: WINDOWPLACEMENT,
}

static FRAMED: Mutex<Option<Framed>> = Mutex::new(None);

/// Puts the window in `mode` on a thread of ours: the window belongs to the
/// title's thread, and changing it waits for that thread, which presenting
/// must not.
fn set_window_mode(hwnd: HWND, mode: WindowMode) {
    let hwnd = hwnd.0 as usize;
    let _ = std::thread::Builder::new()
        .name("victory-window-mode".into())
        .spawn(move || {
            let hwnd = HWND(hwnd as *mut _);
            // SAFETY: calls on a window handle; a window that is gone just fails.
            unsafe {
                match mode {
                    WindowMode::Borderless => {
                        let mut framed = lock(&FRAMED);
                        if framed.is_none() {
                            let mut placement = WINDOWPLACEMENT {
                                length: size_of::<WINDOWPLACEMENT>() as u32,
                                ..Default::default()
                            };
                            if GetWindowPlacement(hwnd, &mut placement).is_err() {
                                return;
                            }
                            *framed = Some(Framed {
                                style: GetWindowLongPtrW(hwnd, GWL_STYLE),
                                placement,
                            });
                        }
                        let style = framed.as_ref().map_or(0, |f| f.style);
                        let mut info = MONITORINFO {
                            cbSize: size_of::<MONITORINFO>() as u32,
                            ..Default::default()
                        };
                        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                            return;
                        }
                        let r = info.rcMonitor;
                        let frameless =
                            (style & !(WS_OVERLAPPEDWINDOW.0 as isize)) | WS_POPUP.0 as isize;
                        SetWindowLongPtrW(hwnd, GWL_STYLE, frameless);
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOP),
                            r.left,
                            r.top,
                            r.right - r.left,
                            r.bottom - r.top,
                            SWP_FRAMECHANGED | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                        );
                        tracing::info!(
                            width = r.right - r.left,
                            height = r.bottom - r.top,
                            "window mode: borderless"
                        );
                    }
                    WindowMode::Windowed => {
                        if let Some(f) = lock(&FRAMED).take() {
                            SetWindowLongPtrW(hwnd, GWL_STYLE, f.style);
                            let _ = SetWindowPlacement(hwnd, &f.placement);
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                0,
                                0,
                                0,
                                0,
                                SWP_FRAMECHANGED
                                    | SWP_NOMOVE
                                    | SWP_NOSIZE
                                    | SWP_NOZORDER
                                    | SWP_NOACTIVATE,
                            );
                            tracing::info!("window mode: windowed");
                        }
                    }
                }
            }
        });
}

static APPLIED: Mutex<u64> = Mutex::new(0);

fn apply_display(hwnd: HWND) {
    let (version, display) = overlay::display();
    let mut applied = lock(&APPLIED);
    if *applied != version {
        *applied = version;
        set_window_mode(hwnd, display.window_mode);
    }
}

// ---------------------------------------------------------------------------
// The frame
// ---------------------------------------------------------------------------

/// The overlay for one present: records its draw into `list` (the back
/// buffer `back` is in the present state before and after) when there is
/// anything to show. Returns the vertex buffer the list reads, for [`sent`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    back: &ID3D12Resource,
    format: DXGI_FORMAT,
    size: (u32, u32),
    hwnd: HWND,
    finished: u64,
    video: &dyn Fn() -> Option<[u64; 4]>,
) -> Option<usize> {
    apply_display(hwnd);
    // Closed, the overlay watches F1 alone: one key query a present.
    let focused = ours_in_front();
    let open = overlay::is_open();
    let mut keys: Vec<u8> = Vec::new();
    if focused {
        if open {
            keys.extend(open_keys().filter(|&vk| down(vk)));
        } else if down(overlay::TOGGLE_KEY) {
            keys.push(overlay::TOGGLE_KEY);
        }
    }
    // The pointer is only needed while the menu is open.
    let mouse = if open {
        pointer(hwnd, size)
    } else {
        (-1.0, -1.0)
    };
    let raw = overlay::Raw {
        now_ns: victory_core::pace::now_ns(),
        size,
        mouse,
        down: &keys,
    };
    let sys = if overlay::wants_sys() {
        sys_info(video)
    } else {
        None
    };
    let mut verts: Vec<Vertex> = Vec::new();
    overlay::frame(&raw, sys, |v| verts.extend_from_slice(v));
    if verts.is_empty() {
        return None;
    }
    draw(device, list, back, format, size, finished, &verts)
}

fn draw(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    back: &ID3D12Resource,
    format: DXGI_FORMAT,
    (width, height): (u32, u32),
    finished: u64,
    verts: &[Vertex],
) -> Option<usize> {
    let mut guard = lock(&PASS);
    if guard.as_ref().is_none_or(|p| p.format != format.0) {
        *guard = match make(device, format) {
            Ok(p) => Some(p),
            Err(e) => {
                static TOLD: std::sync::Once = std::sync::Once::new();
                TOLD.call_once(|| tracing::warn!(error = %e, "no renderer for the settings overlay: it is not shown"));
                return None;
            }
        };
        tracing::info!(format = format.0, "settings overlay renderer made");
    }
    let pass = guard.as_mut()?;
    let bytes = std::mem::size_of_val(verts);
    let slot = take_slot(device, pass, bytes, finished)?;
    // SAFETY: an open list; our resources, live as long as the process or
    // until remade (a list holds what it was recorded with); the mapped
    // buffer has room for `bytes`.
    unsafe {
        std::ptr::copy_nonoverlapping(verts.as_ptr().cast::<u8>(), pass.slots[slot].ptr, bytes);
        if !pass.uploaded {
            let desc = pass.atlas.GetDesc();
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            device.GetCopyableFootprints(&desc, 0, 1, 0, Some(&mut footprint), None, None, None);
            list.CopyTextureRegion(
                &D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&pass.atlas),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: 0,
                    },
                },
                0,
                0,
                0,
                &D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&pass.upload),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: footprint,
                    },
                },
                None,
            );
            list.ResourceBarrier(&[crate::host::transition(
                &pass.atlas,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )]);
            pass.uploaded = true;
        }
        let target = pass.targets.GetCPUDescriptorHandleForHeapStart();
        device.CreateRenderTargetView(
            back,
            Some(&D3D12_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
                ..Default::default()
            }),
            target,
        );
        list.ResourceBarrier(&[crate::host::transition(
            back,
            D3D12_RESOURCE_STATE_PRESENT,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        )]);
        list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
        list.SetGraphicsRootSignature(&pass.root);
        list.SetPipelineState(&pass.pipeline);
        let scale = [
            (2.0 / width as f32).to_bits(),
            (2.0 / height as f32).to_bits(),
        ];
        list.SetGraphicsRoot32BitConstants(0, 2, scale.as_ptr().cast(), 0);
        list.SetGraphicsRootDescriptorTable(1, pass.views.GetGPUDescriptorHandleForHeapStart());
        list.OMSetRenderTargets(1, Some(&target), false, None);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        }]);
        list.IASetPrimitiveTopology(
            windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        );
        list.IASetVertexBuffers(
            0,
            Some(&[D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: pass.slots[slot].buffer.GetGPUVirtualAddress(),
                SizeInBytes: bytes as u32,
                StrideInBytes: STRIDE,
            }]),
        );
        list.DrawInstanced(verts.len() as u32, 1, 0, 0);
        list.ResourceBarrier(&[crate::host::transition(
            back,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_PRESENT,
        )]);
    }
    // Until the caller says which submission carries this list, the buffer
    // is taken as in flight.
    pass.slots[slot].flown = u64::MAX;
    Some(slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vertex_the_shader_reads_is_what_the_overlay_writes() {
        assert_eq!(STRIDE, 20);
        assert_eq!(std::mem::offset_of!(Vertex, uv), 8);
        assert_eq!(std::mem::offset_of!(Vertex, color), 16);
    }

    /// The compiler is the CPU's (no device, no GPU): the source builds, both stages.
    #[test]
    fn the_shader_source_compiles() {
        use windows::core::s;
        let vs = crate::host::compile(SOURCE, s!("vs"), s!("vs_5_1")).expect("vertex stage");
        let ps = crate::host::compile(SOURCE, s!("ps"), s!("ps_5_1")).expect("pixel stage");
        assert!(vs.len() > 64 && ps.len() > 64);
    }

    #[test]
    fn the_sources_carry_no_root_signature() {
        assert!(
            !SOURCE.contains("RootSignature"),
            "the runtime serializes it (#188)"
        );
    }

    #[test]
    fn the_watched_keys_leave_out_the_generic_modifiers_and_list_the_mouse_buttons_once() {
        let keys: Vec<u8> = open_keys().collect();
        assert!(keys.contains(&0x01) && keys.contains(&0x70) && keys.contains(&0xA0));
        assert!(!keys.contains(&0x10) && !keys.contains(&0x11) && !keys.contains(&0x12));
        assert_eq!(keys.iter().filter(|&&k| k == 0x01).count(), 1);
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len());
    }
}
