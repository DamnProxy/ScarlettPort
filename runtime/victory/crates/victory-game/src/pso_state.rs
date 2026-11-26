//! The two state blocks of a precompiled graphics pipeline
//! ([`crate::pso_cache`]), decoded into what a D3D12 graphics pipeline
//! description holds.
//!
//! - Block 0 (380 to 520 bytes) is the console driver's register image of
//!   the pipeline: GFX10 context registers in an order of the driver's own,
//!   the same for every record from the dword `0x060201bc` on (dword 29 of
//!   a vertex-shader pipeline, dword 50 of a mesh one; dword 1 is 4 or 6).
//!   Blend, depth/stencil and rasteriser state are read from it.
//! - Block 1 (80 to 340 bytes) is the driver's copy of the description's
//!   other fields: render target and depth formats, their count, the
//!   topology type, sample mask and count, the stream output (always
//!   empty) and the input layout with its semantic names.
//!
//! Checked against the title (see the tests and `PIPECACHE.md`): the
//! pipeline journal of a run (56,015 graphics descriptions the title passed
//! to `CreateGraphicsPipelineState`, every field, with its own shaders)
//! names the shaders of 13,398 of the 13,409 vertex-shader records, and
//! 13,185 of them decode to a description the title created field for
//! field (blend of every target it has, sample mask, the whole rasteriser
//! state with depth bias and conservative rasterisation, depth/stencil,
//! input layout, topology, strip cut, formats, sample count). All but one
//! of the other 213 differ from the title's pipelines with the same
//! shaders in their target or depth formats only: other pipelines than
//! the run made.
//! Mesh records are created from streams, which the journal does not have.
//!
//! Fields are taken only where the title's pipelines confirm the
//! decoding. A field holding a value they do not confirm (a logic
//! operation, a fill mode other than solid, a topology type other than
//! triangles or rectangles, a sample count above one, ...) makes the
//! record refused ([`Error::Unverified`]): the title then creates the
//! pipeline from its stream.

/// One render target's blend state, as `D3D12_RENDER_TARGET_BLEND_DESC`
/// holds it (D3D12 enumeration values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetBlend {
    pub blend: bool,
    pub logic_op_enable: bool,
    pub src: u32,
    pub dst: u32,
    pub op: u32,
    pub src_alpha: u32,
    pub dst_alpha: u32,
    pub op_alpha: u32,
    pub logic_op: u32,
    pub write_mask: u8,
}

/// `D3D12_DEPTH_STENCILOP_DESC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StencilFace {
    pub fail: u32,
    pub depth_fail: u32,
    pub pass: u32,
    pub func: u32,
}

/// `D3D12_INPUT_ELEMENT_DESC` (format: the console's DXGI number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputElement {
    pub name: String,
    pub index: u32,
    pub format: u32,
    pub slot: u32,
    pub offset: u32,
    pub class: u32,
    pub step_rate: u32,
}

/// A graphics pipeline description without its shaders and root
/// signature. Enumerations are D3D12's values; formats the console's
/// DXGI numbers (as the title passes them in a stream).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphicsDesc {
    /// The record's shader stages feed a mesh shader (no input layout).
    pub mesh: bool,
    pub alpha_to_coverage: bool,
    pub independent_blend: bool,
    pub targets_blend: [TargetBlend; 8],
    pub sample_mask: u32,
    pub fill_mode: u32,
    pub cull_mode: u32,
    pub front_ccw: bool,
    pub depth_bias: i32,
    pub depth_bias_clamp: f32,
    pub slope_scaled_depth_bias: f32,
    pub depth_clip: bool,
    /// `D3D12_CONSERVATIVE_RASTERIZATION_MODE_ON` (overestimation).
    pub conservative_raster: bool,
    pub depth_enable: bool,
    /// `D3D12_DEPTH_WRITE_MASK`: 0 or 1.
    pub depth_write: u32,
    pub depth_func: u32,
    pub stencil_enable: bool,
    pub stencil_read_mask: u8,
    pub stencil_write_mask: u8,
    pub front: StencilFace,
    pub back: StencilFace,
    pub depth_bounds: bool,
    pub inputs: Vec<InputElement>,
    pub strip_cut: u32,
    pub topology: u32,
    pub num_targets: u32,
    pub target_formats: [u32; 8],
    pub depth_format: u32,
    pub sample_count: u32,
    pub sample_quality: u32,
}

/// Why a pair of state blocks does not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The register block is too short or has no anchor where expected.
    Registers,
    /// A register field holds a value without a D3D12 equivalent.
    Value(&'static str, u32),
    /// The description block does not hold what its sizes say.
    Desc(&'static str),
    /// A field holds a value whose decoding no pipeline of the title
    /// confirms: not guessed, the record is refused.
    Unverified(&'static str, u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Registers => f.write_str("register block of unknown layout"),
            Error::Value(what, v) => write!(f, "{what} {v:#x} has no D3D12 value"),
            Error::Desc(what) => write!(f, "description block: {what}"),
            Error::Unverified(what, v) => {
                write!(f, "{what} {v:#x}: a value the title's pipelines do not confirm")
            }
        }
    }
}

impl std::error::Error for Error {}

/// The dword every register block has where its fixed part starts.
pub const ANCHOR: u32 = 0x0602_01bc;

/// Register block dwords, from [`ANCHOR`].
mod at {
    pub const CB_COLOR_CONTROL: usize = 2;
    pub const PA_CL_CLIP_CNTL: usize = 10;
    pub const PA_SU_SC_MODE_CNTL: usize = 11;
    pub const POLY_OFFSET_CLAMP: usize = 14;
    pub const POLY_OFFSET_SCALE: usize = 15;
    pub const POLY_OFFSET_OFFSET: usize = 16;
    pub const DB_DEPTH_CONTROL: usize = 17;
    pub const DB_STENCIL_CONTROL: usize = 18;
    pub const DB_STENCILREFMASK: usize = 19;
    pub const DB_STENCILREFMASK_BF: usize = 20;
    /// `CB_TARGET_MASK` as the description gives it: 4 bits per target,
    /// all eight.
    pub const CB_TARGET_MASK: usize = 22;
    pub const CB_BLEND0_CONTROL: usize = 23;
    pub const SX_PS_DOWNCONVERT: usize = 32;
    /// `PA_SC_CONSERVATIVE_RASTERIZATION_CNTL`.
    pub const CONSERVATIVE: usize = 43;
    /// Dwords from the anchor on that a block has at least.
    pub const END: usize = 46;
}

/// Where the anchor is: dword 29 of a vertex-shader pipeline's block
/// (dword 1 = 4), 50 of a mesh one's (dword 1 = 6).
fn anchor(w: &[u32]) -> Option<(usize, bool)> {
    let (a, mesh) = match w.get(1)? {
        4 => (29, false),
        6 => (50, true),
        _ => return None,
    };
    (w.get(a) == Some(&ANCHOR) && w.len() >= a + at::END).then_some((a, mesh))
}

fn dwords(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes"))).collect()
}

fn bits(v: u32, lo: u32, n: u32) -> u32 {
    v >> lo & ((1 << n) - 1)
}

/// `V_028780_BLEND_*` to `D3D12_BLEND` (for the alpha factors, the colour
/// ones read as their alpha ones, which is what they are there).
fn blend_factor(hw: u32, alpha: bool) -> Result<u32, Error> {
    Ok(match (hw, alpha) {
        (0..=10, false) => hw + 1,
        (2, true) => 5,
        (3, true) => 6,
        (8, true) => 7,
        (9, true) => 8,
        (0 | 1 | 4..=7 | 10, true) => hw + 1,
        (13, _) => 14,
        (14, _) => 15,
        (15, false) => 16,
        (16, false) => 17,
        (15, true) | (17, _) => 18,
        (16, true) | (18, _) => 19,
        (19, _) => 20,
        (20, _) => 21,
        _ => return Err(Error::Value("blend factor", hw)),
    })
}

/// `COMB_FCN` to `D3D12_BLEND_OP`.
fn blend_op(hw: u32) -> Result<u32, Error> {
    Ok(match hw {
        0 => 1, // add
        1 => 2, // subtract
        4 => 3, // reverse subtract
        2 => 4, // min
        3 => 5, // max
        _ => return Err(Error::Value("blend function", hw)),
    })
}

/// `CB_COLOR_CONTROL.ROP3`: copy (0xcc) is no logic operation, the only
/// value of the title's file; any other is refused (no pipeline of the
/// title confirms how the driver writes D3D12's logic operations).
const ROP3_COPY: u32 = 0xcc;

/// `PA_SC_CONSERVATIVE_RASTERIZATION_CNTL` as the driver writes it: off
/// (`NULL_SQUAD_AA_MASK_ENABLE` alone) or overestimation
/// (`OVER_RAST_ENABLE`, `UNDER_RAST_SAMPLE_SELECT` 1,
/// `PBB_UNCERTAINTY_REGION_ENABLE`, the coverage, pre-Z and post-Z AA
/// masks: what Mesa's radv writes for it). 94 records have the second;
/// all those the title's journal names it created with
/// `ConservativeRaster` on, all others off.
const CONSERVATIVE_OFF: u32 = 0x0010_0000;
const CONSERVATIVE_ON: u32 = 0x00e0_0441;

/// `STENCIL_*` to `D3D12_STENCIL_OP` (both replace forms are replace; the
/// clamped and wrapped add and subtract of the operation value 1 are
/// D3D12's increments and decrements).
fn stencil_op(hw: u32) -> Result<u32, Error> {
    Ok(match hw {
        0 => 1,
        1 => 2,
        3 | 4 => 3,
        5 => 4,
        6 => 5,
        7 => 6,
        8 => 7,
        9 => 8,
        _ => return Err(Error::Value("stencil operation", hw)),
    })
}

fn f32_of(v: u32) -> f32 {
    f32::from_bits(v)
}

/// The register image's part of the description.
fn registers(block: &[u8], d: &mut GraphicsDesc) -> Result<(), Error> {
    let w = dwords(block);
    let (a, mesh) = anchor(&w).ok_or(Error::Registers)?;
    let r = |i: usize| w[a + i];
    d.mesh = mesh;

    // Blend.
    let rop3 = bits(r(at::CB_COLOR_CONTROL), 16, 8);
    if rop3 != ROP3_COPY {
        return Err(Error::Unverified("logic operation (ROP3)", rop3));
    }
    let masks = r(at::CB_TARGET_MASK);
    for (t, out) in d.targets_blend.iter_mut().enumerate() {
        let b = r(at::CB_BLEND0_CONTROL + t);
        let write_mask = bits(masks, 4 * t as u32, 4) as u8;
        *out = if b >> 30 & 1 == 1 {
            TargetBlend {
                blend: true,
                src: blend_factor(bits(b, 0, 5), false)?,
                op: blend_op(bits(b, 5, 3))?,
                dst: blend_factor(bits(b, 8, 5), false)?,
                src_alpha: blend_factor(bits(b, 16, 5), true)?,
                op_alpha: blend_op(bits(b, 21, 3))?,
                dst_alpha: blend_factor(bits(b, 24, 5), true)?,
                write_mask,
                ..DEFAULT_BLEND
            }
        } else {
            TargetBlend {
                write_mask,
                ..DEFAULT_BLEND
            }
        };
    }
    d.independent_blend = d.targets_blend.iter().any(|t| *t != d.targets_blend[0]);

    // Rasteriser.
    let mode = r(at::PA_SU_SC_MODE_CNTL);
    d.cull_mode = match bits(mode, 0, 2) {
        0 => 1, // none
        1 => 2, // front
        2 => 3, // back
        // Both culled: D3D12 has no such mode.
        _ => return Err(Error::Value("cull mode", bits(mode, 0, 2))),
    };
    // FACE 0 (front is counter-clockwise) in all but 2 records; both
    // values are the title's (records 14996, 15000 have FACE 1).
    d.front_ccw = bits(mode, 2, 1) == 0;
    // POLY_MODE is 0 (solid; the type fields then do not apply) in every
    // record and the title never asks for wireframe: refused otherwise.
    if bits(mode, 3, 2) != 0 {
        return Err(Error::Unverified("polygon mode", bits(mode, 3, 2)));
    }
    d.fill_mode = 3;
    // PA_SU_POLY_OFFSET_CLAMP, _FRONT_SCALE (1/16 pixel units, so the
    // slope factor times 16), _FRONT_OFFSET (the constant, as a float):
    // the title's pipelines (bias 1 slope 1; clamp 0.003 slope 1.8125)
    // confirm order and scale.
    d.depth_bias_clamp = f32_of(r(at::POLY_OFFSET_CLAMP));
    d.slope_scaled_depth_bias = f32_of(r(at::POLY_OFFSET_SCALE)) / 16.0;
    let offset = f32_of(r(at::POLY_OFFSET_OFFSET));
    if offset.fract() != 0.0 || offset.abs() > i32::MAX as f32 {
        return Err(Error::Value("depth bias", r(at::POLY_OFFSET_OFFSET)));
    }
    d.depth_bias = offset as i32;
    let clip = r(at::PA_CL_CLIP_CNTL);
    // ZCLIP_NEAR_DISABLE and ZCLIP_FAR_DISABLE: both or neither.
    d.depth_clip = match bits(clip, 26, 2) {
        0 => true,
        3 => false,
        v => return Err(Error::Value("depth clip (one plane only)", v)),
    };
    d.conservative_raster = match r(at::CONSERVATIVE) {
        CONSERVATIVE_OFF => false,
        CONSERVATIVE_ON => true,
        v => return Err(Error::Unverified("conservative rasterisation", v)),
    };

    // Depth and stencil.
    let dc = r(at::DB_DEPTH_CONTROL);
    let sc = r(at::DB_STENCIL_CONTROL);
    let refmask = r(at::DB_STENCILREFMASK);
    d.stencil_enable = bits(dc, 0, 1) == 1;
    d.depth_enable = bits(dc, 1, 1) == 1;
    d.depth_write = bits(dc, 2, 1);
    // DEPTH_BOUNDS_ENABLE: never set in the title's file.
    if bits(dc, 3, 1) == 1 {
        return Err(Error::Unverified("depth bounds test", dc));
    }
    d.depth_bounds = false;
    d.depth_func = bits(dc, 4, 3) + 1;
    let back_func = if bits(dc, 7, 1) == 1 { bits(dc, 20, 3) } else { bits(dc, 8, 3) };
    let face = |at: u32, func: u32| -> Result<StencilFace, Error> {
        Ok(StencilFace {
            fail: stencil_op(bits(sc, at, 4))?,
            pass: stencil_op(bits(sc, at + 4, 4))?,
            depth_fail: stencil_op(bits(sc, at + 8, 4))?,
            func: func + 1,
        })
    };
    d.front = face(0, bits(dc, 8, 3))?;
    d.back = face(12, back_func)?;
    d.stencil_read_mask = bits(refmask, 8, 8) as u8;
    d.stencil_write_mask = bits(refmask, 16, 8) as u8;
    // D3D12 has one pair of masks for both faces.
    let back = r(at::DB_STENCILREFMASK_BF);
    if bits(back, 8, 16) != bits(refmask, 8, 16) {
        return Err(Error::Value("back-face stencil masks", back));
    }
    Ok(())
}

const DEFAULT_BLEND: TargetBlend = TargetBlend {
    blend: false,
    logic_op_enable: false,
    src: 2,
    dst: 1,
    op: 1,
    src_alpha: 2,
    dst_alpha: 1,
    op_alpha: 1,
    logic_op: 4,
    write_mask: 0xf,
};

/// The description block's part.
///
/// ```text
/// +0x00 u32 hash, u32 0x1000, u32 0, u32 input slots used (a bit each)
/// +0x10 u8[8] render target formats
/// +0x18 u32 0, u32 sample mask, u32 0
/// +0x24 u8 depth format, u8 render targets, u8 topology type, u8 0
/// +0x28 u32 sample count (low byte), u32 x3 (zero but in 21 records)
/// +0x38 u32 input layout bytes (from +0x4c), u32 stream output bytes (12)
/// +0x40 stream output: u32 entries, strides, rasterized stream (all 0)
/// +0x4c u32 input elements, then each: u32 name bytes (with the NUL),
///       the name, u32 index, format, slot, offset, class, step rate
/// ```
/// The console's rectangle-list topology type, as the title's pipeline
/// streams name it.
pub const RECTANGLES: u32 = 19;
const TRIANGLES: u32 = 3;

/// The topology byte at +0x26: the title's `PrimitiveTopologyType`
/// itself, all of it. 0x13 (408 records) is the console's rectangle list,
/// not a triangle type with a strip cut: taken apart as such, its
/// pipelines lost the geometry stage that completes each rectangle and
/// drew half of each one. Triangles (25,062 records) and rectangles are
/// what the title's pipelines confirm; the one record of type 2 (lines,
/// record 25459) is refused with any other value.
fn topology(byte: u32) -> Result<u32, Error> {
    match byte {
        TRIANGLES | RECTANGLES => Ok(byte),
        _ => Err(Error::Unverified("topology type", byte)),
    }
}

/// +0x28..+0x38 of 21 records (25450 to 25470, each alone with its
/// shaders): the same four dwords in all, with a sample count of 1. The
/// title creates 15 of them with descriptions equal to the rest of the
/// record's (sample count 1, quality 0, no other field it has differs):
/// something of the console's own, not part of a D3D12 description.
const SAMPLE_DWORDS: [[u32; 4]; 2] = [[1, 0, 0, 0], [0x0080_0001, 0x0404_0400, 0x0404_0404, 4]];

fn description(block: &[u8], d: &mut GraphicsDesc) -> Result<(), Error> {
    let u = |at: usize| -> Result<u32, Error> {
        block
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
            .ok_or(Error::Desc("too short"))
    };
    if block.len() < 0x50 {
        return Err(Error::Desc("too short"));
    }
    for (t, f) in d.target_formats.iter_mut().enumerate() {
        *f = u32::from(block[0x10 + t]);
    }
    d.sample_mask = u(0x1c)?;
    let w = u(0x24)?;
    d.depth_format = bits(w, 0, 8);
    d.num_targets = bits(w, 8, 8);
    if d.num_targets > 8 {
        return Err(Error::Desc("more than eight render targets"));
    }
    d.topology = topology(bits(w, 16, 8))?;
    if bits(w, 24, 8) != 0 {
        return Err(Error::Unverified("byte after the topology type", bits(w, 24, 8)));
    }
    // No byte holds a strip cut value: every pipeline of the title has
    // none (D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED).
    d.strip_cut = 0;
    let samples = [u(0x28)?, u(0x2c)?, u(0x30)?, u(0x34)?];
    if !SAMPLE_DWORDS.contains(&samples) {
        // A sample count above 1 among them: no record has one, and the
        // title's pipelines are all single-sampled.
        return Err(Error::Unverified("sample count", samples[0]));
    }
    d.sample_count = 1;
    d.sample_quality = 0;
    if u(0x3c)? != 12 || u(0x40)? != 0 || u(0x44)? != 0 {
        return Err(Error::Desc("a stream output"));
    }
    let layout = u(0x38)? as usize;
    let n = u(0x4c)? as usize;
    let mut at = 0x50;
    for _ in 0..n {
        let len = u(at)? as usize;
        let name = block.get(at + 4..at + 4 + len).ok_or(Error::Desc("input element name"))?;
        let name = name.strip_suffix(&[0]).ok_or(Error::Desc("input element name without its end"))?;
        let name = std::str::from_utf8(name).map_err(|_| Error::Desc("input element name"))?;
        at += 4 + len;
        d.inputs.push(InputElement {
            name: name.to_owned(),
            index: u(at)?,
            format: u(at + 4)?,
            slot: u(at + 8)?,
            offset: u(at + 12)?,
            class: u(at + 16)?,
            step_rate: u(at + 20)?,
        });
        at += 24;
    }
    if at - 0x4c != layout {
        return Err(Error::Desc("input layout size"));
    }
    // +0x0c: the input slots the layout reads, a bit each (all records).
    let slots = d.inputs.iter().try_fold(0u32, |m, e| 1u32.checked_shl(e.slot).map(|b| m | b));
    if slots != Some(u(0x0c)?) {
        return Err(Error::Desc("input slots"));
    }
    Ok(())
}

/// A record's two state blocks, decoded.
pub fn decode(registers_block: &[u8], description_block: &[u8]) -> Result<GraphicsDesc, Error> {
    let mut d = GraphicsDesc {
        mesh: false,
        alpha_to_coverage: false,
        independent_blend: false,
        targets_blend: [DEFAULT_BLEND; 8],
        sample_mask: u32::MAX,
        fill_mode: 3,
        cull_mode: 3,
        front_ccw: false,
        depth_bias: 0,
        depth_bias_clamp: 0.0,
        slope_scaled_depth_bias: 0.0,
        depth_clip: true,
        conservative_raster: false,
        depth_enable: false,
        depth_write: 0,
        depth_func: 8,
        stencil_enable: false,
        stencil_read_mask: 0xff,
        stencil_write_mask: 0xff,
        front: StencilFace { fail: 1, depth_fail: 1, pass: 1, func: 8 },
        back: StencilFace { fail: 1, depth_fail: 1, pass: 1, func: 8 },
        depth_bounds: false,
        inputs: Vec::new(),
        strip_cut: 0,
        topology: 3,
        num_targets: 0,
        target_formats: [0; 8],
        depth_format: 0,
        sample_count: 1,
        sample_quality: 0,
    };
    registers(registers_block, &mut d)?;
    description(description_block, &mut d)?;
    if d.mesh && !d.inputs.is_empty() {
        return Err(Error::Desc("an input layout for a mesh pipeline"));
    }
    Ok(d)
}

/// `SX_PS_DOWNCONVERT`'s format for a render target format of the console
/// (`None`: not one seen in the title's records). For [`consistent`].
fn downconvert(format: u32) -> Option<&'static [u32]> {
    Some(match format {
        28 | 29 | 49 | 61 | 62 | 87 | 51 => &[0, 5],
        24 => &[4],
        26 => &[0, 3],
        34 | 35 => &[0, 9],
        41 | 42 => &[1],
        54 | 56 | 57 => &[0, 10],
        2 | 3 | 10 | 17 | 65 | 86 | 115 => &[0, 5],
        67 => &[11],
        _ => return None,
    })
}

/// Whether the register image agrees with the description block on the
/// render target formats (`SX_PS_DOWNCONVERT`, derived by the driver from
/// them). A check of the decoding, not needed to use it.
pub fn consistent(registers_block: &[u8], d: &GraphicsDesc) -> bool {
    let w = dwords(registers_block);
    let Some((a, _)) = anchor(&w) else { return false };
    let down = w[a + at::SX_PS_DOWNCONVERT];
    (0..d.num_targets as usize).all(|t| {
        downconvert(d.target_formats[t]).is_none_or(|ok| ok.contains(&bits(down, 4 * t as u32, 4)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Record 31 of the title's file, created by the title at run time
    /// (capture frame listings) as "depth false; stencil false; cull 1
    /// front-ccw true; blend true src 5 dst 6 op 1 mask 0xf", one target
    /// of format 87.
    const R31_A: &str = include_str!("pso_state_fixtures/r31.a");
    const R31_B: &str = include_str!("pso_state_fixtures/r31.b");
    /// Record 9285: "depth true write 0 func 7; stencil true read 0x18
    /// write 0x0 front/back [fail 1 depth-fail 1 pass 1 func 3]; cull 3
    /// front-ccw true; blend true src 2 dst 2 op 1 mask 0xf", a target of
    /// format 10 and a depth buffer.
    const R9285_A: &str = include_str!("pso_state_fixtures/r9285.a");
    const R9285_B: &str = include_str!("pso_state_fixtures/r9285.b");
    /// Record 18317: "depth true write 1 func 7; stencil true read 0xff
    /// write 0xff front/back [fail 1 depth-fail 1 pass 3 func 8]; cull 1
    /// front-ccw true; blend false mask 0xf", three targets (29, 28, 34).
    const R18317_A: &str = include_str!("pso_state_fixtures/r18317.a");
    const R18317_B: &str = include_str!("pso_state_fixtures/r18317.b");
    /// Record 14973: a mesh pipeline blending with the second source
    /// (one target, format 49), depth bias 5 and slope 40/16.
    const R14973_A: &str = include_str!("pso_state_fixtures/r14973.a");
    const R14973_B: &str = include_str!("pso_state_fixtures/r14973.b");

    // The records below are each equal, field for field, to a description
    // the title passed to `CreateGraphicsPipelineState` with the same
    // shaders (the pipeline journal of a run, see the module's notes).
    /// Record 8894: a rectangle list (topology type 19, no strip cut),
    /// one target of format 28, no depth.
    const R8894_A: &str = include_str!("pso_state_fixtures/r8894.a");
    const R8894_B: &str = include_str!("pso_state_fixtures/r8894.b");
    /// Record 12151: conservative rasterisation on, depth clip off, front
    /// faces culled, depth test always, no targets.
    const R12151_A: &str = include_str!("pso_state_fixtures/r12151.a");
    const R12151_B: &str = include_str!("pso_state_fixtures/r12151.b");
    /// Record 14996: clockwise front faces (`FrontCounterClockwise` false),
    /// back faces culled, six targets.
    const R14996_A: &str = include_str!("pso_state_fixtures/r14996.a");
    const R14996_B: &str = include_str!("pso_state_fixtures/r14996.b");
    /// Record 19373: a shadow pass, D16 depth, bias clamp 0.003, slope
    /// 1.8125 (29.0 in the register), constant 0, depth clip off.
    const R19373_A: &str = include_str!("pso_state_fixtures/r19373.a");
    const R19373_B: &str = include_str!("pso_state_fixtures/r19373.b");
    /// Record 25450: one of the 21 with the console's own dwords at
    /// +0x28..+0x38 of the description.
    const R25450_A: &str = include_str!("pso_state_fixtures/r25450.a");
    const R25450_B: &str = include_str!("pso_state_fixtures/r25450.b");
    /// Record 10547: a vertex shader writing a clip and a cull distance
    /// (register dwords 7 and 9 from the anchor, derived from the shader).
    const R10547_A: &str = include_str!("pso_state_fixtures/r10547.a");
    const R10547_B: &str = include_str!("pso_state_fixtures/r10547.b");
    /// Record 16343: five input elements from slots 0, 1, 2, 3 and 8
    /// (+0x0c of the description: 0x10f).
    const R16343_A: &str = include_str!("pso_state_fixtures/r16343.a");
    const R16343_B: &str = include_str!("pso_state_fixtures/r16343.b");

    fn dec(a: &str, b: &str) -> GraphicsDesc {
        let (a, b) = (hex(a.trim()), hex(b.trim()));
        let d = decode(&a, &b).unwrap();
        assert!(consistent(&a, &d));
        d
    }

    #[test]
    fn the_topology_byte_is_the_whole_topology_type() {
        assert_eq!(topology(0x13), Ok(RECTANGLES));
        assert_eq!(topology(3), Ok(3));
        // Not a strip cut (bits 4-5) on a triangle type, nor any type the
        // title's pipelines do not confirm.
        assert_eq!(topology(0x23), Err(Error::Unverified("topology type", 0x23)));
        assert_eq!(topology(2), Err(Error::Unverified("topology type", 2)));
    }

    #[test]
    fn a_ui_pipeline_blends_with_source_alpha() {
        let d = dec(R31_A, R31_B);
        assert!(!d.mesh);
        assert!(!d.depth_enable && !d.stencil_enable);
        assert_eq!((d.cull_mode, d.front_ccw, d.fill_mode, d.depth_clip), (1, true, 3, true));
        let t = d.targets_blend[0];
        assert!(t.blend && !t.logic_op_enable);
        assert_eq!((t.src, t.dst, t.op, t.write_mask), (5, 6, 1, 0xf));
        assert!(!d.independent_blend);
        assert_eq!((d.num_targets, d.target_formats[0], d.depth_format), (1, 87, 0));
        assert_eq!((d.topology, d.strip_cut, d.sample_count, d.sample_mask), (3, 0, 1, u32::MAX));
        let names: Vec<(&str, u32, u32)> = d.inputs.iter().map(|e| (e.name.as_str(), e.index, e.format)).collect();
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n.0 == "POSITION"));
    }

    #[test]
    fn a_stencil_tested_additive_pipeline() {
        let d = dec(R9285_A, R9285_B);
        assert!(d.depth_enable && d.stencil_enable);
        assert_eq!((d.depth_write, d.depth_func), (0, 7));
        assert_eq!((d.stencil_read_mask, d.stencil_write_mask), (0x18, 0));
        let face = StencilFace { fail: 1, depth_fail: 1, pass: 1, func: 3 };
        assert_eq!((d.front, d.back), (face, face));
        assert_eq!((d.cull_mode, d.front_ccw), (3, true));
        let t = d.targets_blend[0];
        assert_eq!((t.blend, t.src, t.dst, t.op, t.write_mask), (true, 2, 2, 1, 0xf));
        assert_eq!((d.num_targets, d.target_formats[0], d.depth_format), (1, 10, 20));
    }

    #[test]
    fn a_three_target_pass_writing_stencil() {
        let d = dec(R18317_A, R18317_B);
        assert_eq!((d.depth_write, d.depth_func), (1, 7));
        let face = StencilFace { fail: 1, depth_fail: 1, pass: 3, func: 8 };
        assert_eq!((d.front, d.back, d.stencil_read_mask, d.stencil_write_mask), (face, face, 0xff, 0xff));
        assert_eq!(d.cull_mode, 1);
        assert_eq!((d.num_targets, &d.target_formats[..3], d.depth_format), (3, &[29, 28, 34][..], 20));
        assert!(d.targets_blend.iter().all(|t| !t.blend));
        // Masks per target, as the title gave them: 0xf, 0xf, 0x3.
        let masks: Vec<u8> = d.targets_blend.iter().map(|t| t.write_mask).collect();
        assert_eq!(&masks[..3], &[0xf, 0xf, 0x3]);
        assert!(d.independent_blend);
    }

    #[test]
    fn a_mesh_pipeline_with_dual_source_blending_and_depth_bias() {
        let d = dec(R14973_A, R14973_B);
        assert!(d.mesh && d.inputs.is_empty());
        let t = d.targets_blend[0];
        // dst * (1 - src1) + src: the second source's colour and alpha.
        assert_eq!((t.src, t.dst, t.op, t.src_alpha, t.dst_alpha), (2, 17, 1, 2, 19));
        assert_eq!((d.depth_bias, d.slope_scaled_depth_bias, d.depth_bias_clamp), (5, 2.5, 0.0));
        assert_eq!((d.num_targets, d.target_formats[0]), (1, 49));
    }

    #[test]
    fn a_rectangle_list() {
        let d = dec(R8894_A, R8894_B);
        assert_eq!((d.topology, d.strip_cut), (RECTANGLES, 0));
        assert_eq!((d.num_targets, d.target_formats[0], d.depth_format), (1, 28, 0));
        assert!(!d.conservative_raster);
    }

    #[test]
    fn conservative_rasterisation_without_depth_clip() {
        let d = dec(R12151_A, R12151_B);
        assert!(d.conservative_raster);
        assert!(!d.depth_clip);
        assert_eq!((d.cull_mode, d.front_ccw, d.fill_mode), (2, true, 3));
        assert_eq!((d.depth_enable, d.depth_func, d.num_targets), (true, 8, 0));
    }

    #[test]
    fn clockwise_front_faces() {
        let d = dec(R14996_A, R14996_B);
        assert_eq!((d.cull_mode, d.front_ccw), (3, false));
        assert_eq!((d.depth_func, d.depth_format, d.num_targets), (7, 20, 6));
        assert_eq!(&d.target_formats[..6], &[29, 28, 28, 28, 28, 28]);
    }

    #[test]
    fn depth_bias_clamp_and_slope() {
        let d = dec(R19373_A, R19373_B);
        assert_eq!(d.depth_bias, 0);
        assert_eq!(d.depth_bias_clamp, 0.003);
        assert_eq!(d.slope_scaled_depth_bias, 1.8125);
        assert!(!d.depth_clip);
        assert_eq!((d.depth_func, d.depth_format, d.num_targets), (2, 55, 0));
    }

    #[test]
    fn the_console_s_own_description_dwords() {
        let d = dec(R25450_A, R25450_B);
        assert_eq!((d.sample_count, d.sample_quality), (1, 0));
        assert_eq!((d.num_targets, d.target_formats[0], d.depth_format), (1, 29, 20));
    }

    #[test]
    fn clip_and_cull_distances_are_the_shader_s() {
        let d = dec(R10547_A, R10547_B);
        assert_eq!((d.depth_func, d.cull_mode, d.num_targets, d.target_formats[0]), (2, 1, 1, 28));
        assert!(d.inputs.is_empty());
    }

    #[test]
    fn input_slots() {
        let d = dec(R16343_A, R16343_B);
        let mut used: Vec<u32> = d.inputs.iter().map(|e| e.slot).collect();
        assert_eq!(used.len(), 5);
        used.sort_unstable();
        used.dedup();
        assert_eq!(used, [0, 1, 2, 3, 8]);
    }

    /// Puts `v` at register dword `i` from the anchor of a vertex-shader
    /// pipeline's block.
    fn set(a: &mut [u8], i: usize, v: u32) {
        let at = 4 * (29 + i);
        a[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn reg(a: &[u8], i: usize) -> u32 {
        let at = 4 * (29 + i);
        u32::from_le_bytes(a[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn values_without_a_d3d12_meaning_are_refused() {
        let (mut a, b) = (hex(R31_A.trim()), hex(R31_B.trim()));
        // Blend factor 11 (BOTH_SRC_ALPHA) in target 0's colour source.
        set(&mut a, at::CB_BLEND0_CONTROL, 0xe501_050b);
        assert_eq!(decode(&a, &b), Err(Error::Value("blend factor", 11)));
        // No anchor.
        assert_eq!(decode(&a[..0x60], &b), Err(Error::Registers));
        // A description whose layout size disagrees.
        let mut b2 = b.clone();
        b2[0x38] ^= 1;
        assert_eq!(decode(&hex(R31_A.trim()), &b2), Err(Error::Desc("input layout size")));
        // Or whose input slots do.
        let mut b2 = b.clone();
        b2[0x0c] = 3;
        assert_eq!(decode(&hex(R31_A.trim()), &b2), Err(Error::Desc("input slots")));
    }

    /// Values no pipeline of the title confirms the decoding of: the
    /// record is refused, and the title creates the pipeline from its
    /// stream.
    #[test]
    fn unconfirmed_values_are_refused() {
        let (a, b) = (hex(R31_A.trim()), hex(R31_B.trim()));
        let with = |i: usize, f: &dyn Fn(u32) -> u32| {
            let mut a = a.clone();
            let v = f(reg(&a, i));
            set(&mut a, i, v);
            decode(&a, &b)
        };
        // A logic operation (ROP3 xor).
        assert_eq!(
            with(at::CB_COLOR_CONTROL, &|v| v & !0xff_0000 | 0x66_0000),
            Err(Error::Unverified("logic operation (ROP3)", 0x66))
        );
        // Wireframe (POLY_MODE 1).
        assert_eq!(with(at::PA_SU_SC_MODE_CNTL, &|v| v | 1 << 3), Err(Error::Unverified("polygon mode", 1)));
        // Both faces culled.
        assert_eq!(with(at::PA_SU_SC_MODE_CNTL, &|v| v | 3), Err(Error::Value("cull mode", 3)));
        // The depth bounds test.
        assert!(matches!(with(at::DB_DEPTH_CONTROL, &|v| v | 8), Err(Error::Unverified("depth bounds test", _))));
        // Another conservative rasterisation setting (underestimation).
        assert_eq!(
            with(at::CONSERVATIVE, &|_| 0x0010_0021),
            Err(Error::Unverified("conservative rasterisation", 0x0010_0021))
        );
        // One depth clip plane.
        assert_eq!(with(at::PA_CL_CLIP_CNTL, &|v| v | 1 << 26), Err(Error::Value("depth clip (one plane only)", 1)));
        // Other stencil masks for back faces.
        assert!(matches!(
            with(at::DB_STENCILREFMASK_BF, &|v| v ^ 0x100),
            Err(Error::Value("back-face stencil masks", _))
        ));
        // A constant depth bias that is not an integer.
        assert!(matches!(with(at::POLY_OFFSET_OFFSET, &|_| 0x3fc0_0000), Err(Error::Value("depth bias", _))));
        // Lines, and a strip cut value in the topology byte.
        for byte in [2u8, 0x23] {
            let mut b2 = b.clone();
            b2[0x26] = byte;
            assert_eq!(decode(&a, &b2), Err(Error::Unverified("topology type", u32::from(byte))));
        }
        let mut b2 = b.clone();
        b2[0x27] = 1;
        assert_eq!(decode(&a, &b2), Err(Error::Unverified("byte after the topology type", 1)));
        // Four samples.
        let mut b2 = b.clone();
        b2[0x28] = 4;
        assert_eq!(decode(&a, &b2), Err(Error::Unverified("sample count", 4)));
    }

    #[test]
    fn enumerations() {
        assert_eq!(blend_factor(15, false), Ok(16));
        assert_eq!(blend_factor(17, true), Ok(18));
        assert_eq!(blend_factor(2, true), Ok(5));
        assert_eq!(blend_op(4), Ok(3));
        assert_eq!(stencil_op(3), Ok(3));
    }

    fn le(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    /// A `D3D12_GRAPHICS_PIPELINE_STATE_DESC` (x64 layout, 0x290 bytes) as
    /// the journal holds it.
    fn journal_desc(p: &[u8], inputs: Vec<InputElement>) -> GraphicsDesc {
        let f = |at| f32::from_bits(le(p, at));
        let target = |t: usize| {
            let o = 0x80 + 40 * t;
            TargetBlend {
                blend: le(p, o) != 0,
                logic_op_enable: le(p, o + 4) != 0,
                src: le(p, o + 8),
                dst: le(p, o + 12),
                op: le(p, o + 16),
                src_alpha: le(p, o + 20),
                dst_alpha: le(p, o + 24),
                op_alpha: le(p, o + 28),
                logic_op: le(p, o + 32),
                write_mask: p[o + 36],
            }
        };
        let face = |o| StencilFace { fail: le(p, o), depth_fail: le(p, o + 4), pass: le(p, o + 8), func: le(p, o + 12) };
        let (r, z) = (0x1c4, 0x1f0);
        assert!(le(p, r + 28) == 0 && le(p, r + 32) == 0 && le(p, r + 36) == 0, "multisample, lines, forced count");
        GraphicsDesc {
            mesh: false,
            alpha_to_coverage: le(p, 0x78) != 0,
            independent_blend: le(p, 0x7c) != 0,
            targets_blend: std::array::from_fn(target),
            sample_mask: le(p, 0x1c0),
            fill_mode: le(p, r),
            cull_mode: le(p, r + 4),
            front_ccw: le(p, r + 8) != 0,
            depth_bias: le(p, r + 12) as i32,
            depth_bias_clamp: f(r + 16),
            slope_scaled_depth_bias: f(r + 20),
            depth_clip: le(p, r + 24) != 0,
            conservative_raster: le(p, r + 40) != 0,
            depth_enable: le(p, z) != 0,
            depth_write: le(p, z + 4),
            depth_func: le(p, z + 8),
            stencil_enable: le(p, z + 12) != 0,
            stencil_read_mask: p[z + 16],
            stencil_write_mask: p[z + 17],
            front: face(z + 20),
            back: face(z + 36),
            depth_bounds: false,
            inputs,
            strip_cut: le(p, 0x238),
            topology: le(p, 0x23c),
            num_targets: le(p, 0x240),
            target_formats: std::array::from_fn(|t| le(p, 0x244 + 4 * t)),
            depth_format: le(p, 0x264),
            sample_count: le(p, 0x268),
            sample_quality: le(p, 0x26c),
        }
    }

    /// The fields in which a record's description gives another pipeline
    /// than the title's `t`: the targets it has (all eight alike without
    /// independent blending), blend factors only where blending is on,
    /// stencil state only with the test on, depth state only with depth on.
    fn differs(r: &GraphicsDesc, t: &GraphicsDesc) -> Vec<&'static str> {
        let n = t.num_targets as usize;
        let blend = |d: &GraphicsDesc| -> Vec<TargetBlend> {
            (0..n)
                .map(|i| {
                    let mut b = d.targets_blend[if t.independent_blend { i } else { 0 }];
                    if !b.blend {
                        b = TargetBlend { write_mask: b.write_mask, logic_op_enable: b.logic_op_enable, ..DEFAULT_BLEND };
                    }
                    if !b.logic_op_enable {
                        b.logic_op = DEFAULT_BLEND.logic_op;
                    }
                    b
                })
                .collect()
        };
        let mut out = Vec::new();
        let mut check = |what, same: bool| {
            if !same {
                out.push(what);
            }
        };
        check("alpha to coverage", r.alpha_to_coverage == t.alpha_to_coverage);
        check("blend", blend(r) == blend(t));
        check("sample mask", r.sample_mask == t.sample_mask);
        check("fill", r.fill_mode == t.fill_mode);
        check("cull", r.cull_mode == t.cull_mode && r.front_ccw == t.front_ccw);
        check(
            "depth bias",
            (r.depth_bias, r.depth_bias_clamp, r.slope_scaled_depth_bias)
                == (t.depth_bias, t.depth_bias_clamp, t.slope_scaled_depth_bias),
        );
        check("depth clip", r.depth_clip == t.depth_clip);
        check("conservative", r.conservative_raster == t.conservative_raster);
        check("depth", r.depth_enable == t.depth_enable);
        if t.depth_enable {
            check("depth", (r.depth_write, r.depth_func) == (t.depth_write, t.depth_func));
        }
        check("stencil", r.stencil_enable == t.stencil_enable);
        if t.stencil_enable {
            check(
                "stencil",
                (r.stencil_read_mask, r.stencil_write_mask, r.front, r.back)
                    == (t.stencil_read_mask, t.stencil_write_mask, t.front, t.back),
            );
        }
        check("inputs", r.inputs == t.inputs);
        check("topology", (r.topology, r.strip_cut) == (t.topology, t.strip_cut));
        check("target count", r.num_targets == t.num_targets);
        check("formats", r.target_formats[..n] == t.target_formats[..n]);
        check("depth format", r.depth_format == t.depth_format);
        check("samples", (r.sample_count, r.sample_quality) == (t.sample_count, t.sample_quality));
        out
    }

    /// Every record against the descriptions the title created (a pipeline
    /// journal, `XCLPJ001`: frames of tag, u8, u16, u32 payload size, u64
    /// hash, payload; `S` a shader container, `G` five shader hashes
    /// (vertex, pixel, domain, hull, geometry), the description and the
    /// input layout). `VICTORY_PSO_CACHE` and `VICTORY_PSO_JOURNAL`:
    /// `cargo test -p victory-game pso_state -- --ignored`.
    ///
    /// The title's run of 24 November 2026: 13,398 records have shaders it
    /// created pipelines with; 13,185 decode to one of those pipelines, the
    /// others (213) differ from all of them in formats only, but record
    /// 8892 (blend, cull and formats).
    #[test]
    #[ignore]
    fn the_title_s_own_descriptions() {
        use std::collections::{HashMap, HashSet};
        let (Some(cache), Some(journal)) =
            (std::env::var_os("VICTORY_PSO_CACHE"), std::env::var_os("VICTORY_PSO_JOURNAL"))
        else {
            return;
        };
        let file = std::fs::read(cache).unwrap();
        let cache = crate::pso_cache::Cache::parse(&file).unwrap();
        let journal = std::fs::read(journal).unwrap();
        assert_eq!(&journal[..8], b"XCLPJ001");

        let digest = |slot: Option<(u32, u32)>| {
            slot.and_then(|(c, m)| crate::pso_cache::stage_shader_hash(cache.code(c)?, cache.block(m)?))
        };
        let mut records: HashMap<(Option<[u8; 16]>, Option<[u8; 16]>), Vec<usize>> = HashMap::new();
        for i in 0..cache.graphics_count() {
            let g = cache.graphics(i).unwrap();
            let stage = digest(g.stages[1]).or(digest(g.stages[2]));
            records.entry((stage, digest(g.stages[0]))).or_default().push(i);
        }
        let decoded: Vec<Result<GraphicsDesc, Error>> = (0..cache.graphics_count())
            .map(|i| {
                let g = cache.graphics(i).unwrap();
                decode(cache.block(g.state[0]).unwrap(), cache.block(g.state[1]).unwrap())
            })
            .collect();

        let mut shaders: HashMap<u64, [u8; 16]> = HashMap::new();
        let (mut named, mut matched) = (HashSet::new(), HashSet::new());
        let mut nearest: HashMap<usize, Vec<&'static str>> = HashMap::new();
        let mut at = 8;
        while at + 16 <= journal.len() {
            let size = le(&journal, at + 4) as usize;
            let hash = u64::from_le_bytes(journal[at + 8..at + 16].try_into().unwrap());
            let p = &journal[at + 16..at + 16 + size];
            at += 16 + size;
            match journal[at - 16 - size] {
                b'S' => {
                    if let Some(h) = crate::sgd5::shader_hash(p) {
                        shaders.insert(hash, h);
                    }
                }
                b'G' => {
                    let h = |k: usize| u64::from_le_bytes(p[8 * k..8 * k + 8].try_into().unwrap());
                    if (2..5).any(|k| h(k) != 0) {
                        continue;
                    }
                    let n = le(p, 40) as usize;
                    assert_eq!(n, 0x290);
                    let desc = &p[44..44 + n];
                    let mut q = 44 + n + 4;
                    let inputs = (0..le(p, 44 + n))
                        .map(|_| {
                            let len = le(p, q) as usize;
                            let name = String::from_utf8(p[q + 4..q + 4 + len].to_vec()).unwrap();
                            q += 4 + len;
                            let w = |k: usize| le(p, q + 4 * k);
                            let e = InputElement {
                                name,
                                index: w(0),
                                format: w(1),
                                slot: w(2),
                                offset: w(3),
                                class: w(4),
                                step_rate: w(5),
                            };
                            q += 24;
                            e
                        })
                        .collect();
                    let t = journal_desc(desc, inputs);
                    let key = (shaders.get(&h(0)).copied(), shaders.get(&h(1)).copied());
                    for &i in records.get(&key).into_iter().flatten() {
                        named.insert(i);
                        let Ok(r) = &decoded[i] else { continue };
                        let d = differs(r, &t);
                        if d.is_empty() {
                            matched.insert(i);
                        }
                        let best = nearest.entry(i).or_insert_with(|| d.clone());
                        if d.len() < best.len() {
                            *best = d;
                        }
                    }
                }
                _ => {}
            }
        }
        let refused: Vec<usize> = named.iter().copied().filter(|&i| decoded[i].is_err()).collect();
        assert_eq!(refused, Vec::<usize>::new(), "records the title made refused");
        let others: Vec<(usize, &Vec<&str>)> =
            named.difference(&matched).map(|i| (*i, &nearest[i])).collect();
        // Record 8892 alone differs in more: blend, cull and formats.
        let mut far: Vec<usize> = others
            .iter()
            .filter(|(_, d)| !d.iter().all(|f| *f == "formats" || *f == "depth format"))
            .map(|(i, _)| *i)
            .collect();
        far.sort_unstable();
        assert_eq!(far, [8892]);
        assert!(matched.len() >= 13_172, "{} records the title made", matched.len());
        eprintln!("{} records named, {} the title's, {} in formats only", named.len(), matched.len(), others.len());
    }

    /// Every record of the title's file (path in `VICTORY_PSO_CACHE`, the
    /// unpacked `v2.d3d12PipelineCache`): all decode and agree with
    /// themselves, but record 25459, the one line list (refused).
    /// `cargo test -p victory-game pso_state -- --ignored`.
    #[test]
    #[ignore]
    fn every_record_of_the_title_s_file() {
        let Some(path) = std::env::var_os("VICTORY_PSO_CACHE") else { return };
        let file = std::fs::read(path).unwrap();
        let cache = crate::pso_cache::Cache::parse(&file).unwrap();
        let (mut ok, mut agree, mut refused) = (0, 0, Vec::new());
        for i in 0..cache.graphics_count() {
            let g = cache.graphics(i).unwrap();
            let (a, b) = (cache.block(g.state[0]).unwrap(), cache.block(g.state[1]).unwrap());
            match decode(a, b) {
                Ok(d) => {
                    ok += 1;
                    agree += usize::from(consistent(a, &d));
                }
                Err(e @ Error::Unverified(..)) => refused.push((i, e)),
                Err(e) => panic!("record {i}: {e}"),
            }
        }
        assert_eq!(refused, [(25459, Error::Unverified("topology type", 2))]);
        assert_eq!(ok, cache.graphics_count() - 1);
        assert_eq!(agree, ok);
    }
}
