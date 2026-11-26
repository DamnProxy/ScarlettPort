//! A mesh shader linked to the pixel shader it draws for. D3D12 caps a mesh
//! shader's outputs at 32 KiB for its declared vertex and primitive maxima
//! (the validator: 4 bytes per declared component, the maxima rounded up to
//! 32). Some of the title's 128-lane mesh programs declare more than fits even
//! at one vertex per lane; the console has no such cap. Their pixel shaders
//! read fewer: the pair is translated with the mesh outputs the pixel shader
//! reads (and every system value), and the pixel shader with only the inputs
//! it reads, so the pixel shader declares nothing the mesh shader leaves out.
//! A dropped output is exported by the console code and stored nowhere: the
//! values the pixel shader sees are the same.

use std::cell::RefCell;

use super::{HostShader, translate};
use crate::{Container, FourCC, ShaderError};

/// A semantic of an arbitrary (not system-value) signature element. Names
/// compare without case, as D3D12 links them; kept upper-case.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Semantic {
    pub name: String,
    pub index: u32,
}

impl Semantic {
    pub fn new(name: &str, index: u32) -> Semantic {
        Semantic {
            name: name.to_ascii_uppercase(),
            index,
        }
    }

    pub(super) fn names(&self, name: &str, index: u32) -> bool {
        self.index == index && self.name.eq_ignore_ascii_case(name)
    }
}

thread_local! {
    static KEEP: RefCell<Option<Vec<Semantic>>> = const { RefCell::new(None) };
    static MASKS: RefCell<Option<Vec<(Semantic, u8)>>> = const { RefCell::new(None) };
}

/// Experiment (`VICTORY_MESH_LINK=1`, off by default; Marvisak/VIctory
/// #2398): a linked pair also leaves out the leading and trailing channels
/// of a kept element the pixel shader does not read, so a mesh shader still
/// past 32 KiB with whole elements kept can fit.
pub fn channels_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_MESH_LINK").is_ok_and(|v| v == "1"))
}

/// The channels (absolute mask) of each kept element a translation on this
/// thread keeps, if limited by channel ([`with_masks`]).
pub(super) fn kept_masks() -> Option<Vec<(Semantic, u8)>> {
    MASKS.with(|k| k.borrow().clone())
}

/// `f` with [`kept_masks`] answering `masks` on this thread.
pub fn with_masks<T>(masks: &[(Semantic, u8)], f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Vec<(Semantic, u8)>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            MASKS.with(|k| *k.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(MASKS.with(|k| k.borrow_mut().replace(masks.to_vec())));
    f()
}

/// Kept arbitrary 32-bit elements cut to the channels `masks` reads
/// (leading and trailing unread channels left out, as #2398 does); an
/// element with nothing read, a 16-bit one or a system value stays whole.
pub(super) fn trim_channels(elements: &mut [super::container::SigElement], masks: &[(Semantic, u8)]) {
    for e in elements.iter_mut() {
        if e.kind != 0 || e.half || e.row.is_none() || e.cols == 0 {
            continue;
        }
        let used = masks
            .iter()
            .filter(|(s, _)| s.names(&e.name, e.index))
            .fold(0u8, |a, (_, m)| a | m);
        let rel = (used >> e.col) & ((1u8 << e.cols) - 1);
        if rel == 0 {
            continue;
        }
        let first = rel.trailing_zeros() as u8;
        let last = 7 - rel.leading_zeros() as u8;
        e.col += first;
        e.cols = last - first + 1;
    }
}

/// The arbitrary elements a translation on this thread keeps, if limited
/// ([`translate_keeping`]): mesh outputs, pixel inputs.
pub(super) fn kept() -> Option<Vec<Semantic>> {
    KEEP.with(|k| k.borrow().clone())
}

/// `container` translated with only the arbitrary mesh outputs or pixel
/// inputs `keep` names (system values always stay); other stages as
/// [`translate`] makes them. Exports to a dropped output are not stored.
/// A pixel shader that reads an input `keep` leaves out does not translate.
pub fn translate_keeping(container: &[u8], keep: &[Semantic]) -> Result<HostShader, ShaderError> {
    with_keep(keep, || translate(container))
}

/// `f` with [`kept`] answering `keep` on this thread.
pub(crate) fn with_keep<T>(keep: &[Semantic], f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Vec<Semantic>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            KEEP.with(|k| *k.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(KEEP.with(|k| k.borrow_mut().replace(keep.to_vec())));
    f()
}

/// The rows of a signature part as (semantic, system value, mask, rw mask).
fn rows(part: &[u8]) -> Result<Vec<(Semantic, u32, u8, u8)>, ShaderError> {
    use crate::dxbc::u32_at;
    let count = u32_at(part, 0)? as usize;
    let base = u32_at(part, 4)? as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = base + 32 * i;
        let name_at = u32_at(part, at + 4)? as usize;
        let name = part
            .get(name_at..)
            .and_then(|s| s.split(|&b| b == 0).next())
            .ok_or(ShaderError::Malformed("signature name out of bounds"))?;
        let masks = part
            .get(at + 24..at + 26)
            .ok_or(ShaderError::Malformed("truncated"))?;
        out.push((
            Semantic::new(&String::from_utf8_lossy(name), u32_at(part, at + 8)?),
            u32_at(part, at + 12)?,
            masks[0],
            masks[1],
        ));
    }
    Ok(out)
}

/// The arbitrary inputs a translated pixel shader reads (a component of
/// which its code loads), sorted.
pub fn pixel_reads(ps: &HostShader) -> Result<Vec<Semantic>, ShaderError> {
    let c = Container::parse(&ps.dxil)?;
    let Some(part) = c.part(FourCC::ISG1) else {
        return Ok(Vec::new());
    };
    let mut reads: Vec<Semantic> = rows(part.data)?
        .into_iter()
        .filter(|(_, sv, _, rw)| *sv == 0 && *rw != 0)
        .map(|(s, ..)| s)
        .collect();
    reads.sort();
    reads.dedup();
    Ok(reads)
}

/// The arbitrary inputs a translated pixel shader reads with the channels
/// (absolute mask) read of each, sorted.
pub fn pixel_read_masks(ps: &HostShader) -> Result<Vec<(Semantic, u8)>, ShaderError> {
    let c = Container::parse(&ps.dxil)?;
    let Some(part) = c.part(FourCC::ISG1) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<(Semantic, u8)> = Vec::new();
    for (s, sv, _, rw) in rows(part.data)? {
        if sv != 0 || rw == 0 {
            continue;
        }
        match out.iter_mut().find(|(t, _)| *t == s) {
            Some((_, m)) => *m |= rw,
            None => out.push((s, rw)),
        }
    }
    out.sort();
    Ok(out)
}

/// A translated mesh shader's outputs in bytes as the validator measures
/// them against D3D12's 32 KiB; 0 for another stage.
pub fn mesh_output_bytes(ms: &HostShader) -> Result<u64, ShaderError> {
    let c = Container::parse(&ms.dxil)?;
    let Some(psv) = c.part(FourCC::PSV0) else {
        return Ok(0);
    };
    let psv = psv.data;
    // PSV0 runtime info: mesh maxima at 12 and 14, the stage at 24 (13 mesh).
    let info = |at: usize| psv.get(4 + at..6 + at).map(|b| u32::from(u16::from_le_bytes([b[0], b[1]])));
    if psv.get(4 + 24) != Some(&13) {
        return Ok(0);
    }
    let (Some(vertices), Some(primitives)) = (info(12), info(14)) else {
        return Err(ShaderError::Malformed("truncated"));
    };
    let scalars = |cc| -> Result<u64, ShaderError> {
        Ok(match c.part(cc) {
            Some(p) => rows(p.data)?.iter().map(|r| u64::from(r.2.count_ones())).sum(),
            None => 0,
        })
    };
    let align = |n: u32| u64::from(n).next_multiple_of(32);
    Ok(4 * (scalars(FourCC::OSG1)? * align(vertices) + scalars(FourCC::PSG1)? * align(primitives)))
}

/// Whether a translated mesh shader's outputs pass D3D12's 32 KiB (it does
/// not validate alone; [`link`] it with its pixel shader).
pub fn mesh_outputs_over(ms: &HostShader) -> bool {
    mesh_output_bytes(ms).is_ok_and(|n| n > super::container::MESH_OUTPUT_LIMIT)
}

/// A mesh shader and the pixel shader it draws for, linked: the mesh
/// shader with only the outputs the pixel shader reads, the pixel shader
/// with only those inputs. `ps` is the pixel shader's own translation.
/// Both keep every resource binding of their own translations (an output
/// dropped leaves the reads behind it in place), checked here.
pub fn link(
    ms_container: &[u8],
    ms: &HostShader,
    ps_container: &[u8],
    ps: &HostShader,
) -> Result<(HostShader, HostShader), ShaderError> {
    let reads = pixel_reads(ps)?;
    let ms2 = translate_keeping(ms_container, &reads)?;
    let ps2 = translate_keeping(ps_container, &reads)?;
    if !same_bindings(ms, &ms2) || !same_bindings(ps, &ps2) {
        return Err(ShaderError::Unsupported {
            offset: 0,
            what: "a linked mesh or pixel shader with other bindings than its own",
        });
    }
    if mesh_outputs_over(&ms2) {
        return Err(ShaderError::Unsupported {
            offset: 0,
            what: "mesh outputs a pixel shader reads past the host's 32 KiB",
        });
    }
    Ok((ms2, ps2))
}

/// Whether two translations of one program bind the same (a linked one
/// must, to run under the root signature its own translation defines).
pub fn same_bindings(a: &HostShader, b: &HostShader) -> bool {
    a.user_data == b.user_data
        && a.samplers == b.samplers
        && a.root_cbvs == b.root_cbvs
        && a.root_srvs == b.root_srvs
        && a.root_uavs == b.root_uavs
        && a.root_indexed_views == b.root_indexed_views
        && a.half_views == b.half_views
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::container::{self, SigElement};

    fn element(name: &str, index: u32, kind: u8, row: u32, cols: u8, usage: u8) -> SigElement {
        SigElement {
            name: name.into(),
            index,
            kind,
            comp: 9,
            interp: 2,
            row: Some(row),
            col: 0,
            cols,
            usage,
            half: false,
        }
    }

    fn shader(parts: &[(FourCC, &[u8])]) -> HostShader {
        HostShader {
            stage: crate::HwStage::Ms,
            dxil: crate::dxbc::build(parts),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            root_indexed_views: Vec::new(),
            half_views: Vec::new(),
        }
    }

    /// The inputs a pixel shader's code reads, by their `ISG1` read masks:
    /// not the position, not an input it declares and never reads.
    #[test]
    fn pixel_reads_are_the_read_arbitrary_inputs() {
        let ins = [
            element("SV_Position", 0, 3, 0, 4, 0b11),
            element("TEXCOORD", 3, 0, 1, 4, 0b1),
            element("texcoord", 1, 0, 2, 2, 0),
            element("COLOR", 0, 0, 3, 3, 0b100),
        ];
        let isg1 = container::signature(&ins, true);
        let ps = shader(&[(FourCC::ISG1, &isg1)]);
        assert_eq!(pixel_reads(&ps).unwrap(), [Semantic::new("COLOR", 0), Semantic::new("TEXCOORD", 3)]);
    }

    /// The validator's measure: 66 components at 128 vertices is past 32 KiB,
    /// 64 is not.
    #[test]
    fn mesh_output_bytes_as_the_validator_counts() {
        let mesh = |scalars: u8| {
            let mut outs = vec![element("SV_Position", 0, 3, 0, 4, 15)];
            let mut row = 1;
            let mut left = scalars - 4;
            while left > 0 {
                let n = left.min(4);
                outs.push(element("TEXCOORD", row, 0, row, n, 0));
                left -= n;
                row += 1;
            }
            let osg1 = container::signature(&outs, false);
            let psv0 = container::psv(&container::Psv {
                stage: 13,
                flag: false,
                inputs: &[],
                outputs: &outs,
                prims: &[],
                mesh: Some((super::super::shader::Mesh { max_vertices: 128, max_primitives: 128, topology: 2 }, 0)),
                geometry: None,
                bindings: &[],
                threads: [128, 1, 1],
                wave_size: None,
            });
            shader(&[(FourCC::OSG1, &osg1), (FourCC::PSV0, &psv0)])
        };
        assert_eq!(mesh_output_bytes(&mesh(66)).unwrap(), 66 * 128 * 4);
        assert!(mesh_outputs_over(&mesh(66)));
        assert!(!mesh_outputs_over(&mesh(64)));
    }

    #[test]
    fn channels_trimmed_to_the_reads() {
        let mut els = vec![
            element("SV_Position", 0, 3, 0, 4, 0xF),
            element("TEXCOORD", 0, 0, 1, 4, 0xF),
            element("TEXCOORD", 15, 0, 2, 4, 0xF),
            element("TEXCOORD", 3, 0, 3, 4, 0xF),
        ];
        let masks = [
            (Semantic::new("texcoord", 0), 0b0011),
            (Semantic::new("TEXCOORD", 15), 0b1100),
        ];
        trim_channels(&mut els, &masks);
        let cut: Vec<(u8, u8)> = els.iter().map(|e| (e.col, e.cols)).collect();
        // system value whole; xy; zw; nothing read stays whole
        assert_eq!(cut, [(0, 4), (0, 2), (2, 2), (0, 4)]);
        let bytes = container::mesh_output_bytes(&els[..3], &[], 128, 128);
        assert_eq!(bytes, 4 * 8 * 128);
    }
}
