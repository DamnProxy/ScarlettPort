//! The lowering's `VICTORY_SHADER_OPT` bits that are not passes over the module
//! (`opt::MUL24`, `opt::LDSSYNC`, `opt::UNIFORM`): what each leaves out, on synthetic
//! GFX10 compute programs, lowered with the bit and without. Host-independent (no
//! validator, no device); `valu_tests` runs the 24-bit forms on WARP.

use std::collections::HashSet;

use super::bitcode::opt;
use super::lower;
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// A compute program of `threads` threads (thread index in v0) with 4 root constants in
/// s0..s3, a raw UAV's descriptor in s[8:11] and `lds_bytes` of LDS, then `body`.
pub(super) fn native(body: &[u32], wave64: bool, threads: u32, lds_bytes: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: (5 << 1) | ((lds_bytes / 512) << 15), // 5 user SGPRs: s0..s4
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `native` lowered for a host of 32-lane waves with the branches closing `wide` taken
/// wave-wide, with the lowering's opt bits `bits`.
pub(super) fn lowered(native: &NativeProgram, wide: &HashSet<usize>, bits: u32) -> lower::Lowered {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure_wide(&p, wide).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let was_wave = lower::TEST_HOST_WAVE.replace(Some(32));
    let was_opt = super::TEST_SHADER_OPT.replace(Some(bits));
    let lowered = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        dropped: Vec::new(),
    });
    lower::TEST_HOST_WAVE.set(was_wave);
    super::TEST_SHADER_OPT.set(was_opt);
    lowered.expect("lower")
}

/// DS `op vdst, addr, data0` (offset 0).
const fn ds(op: u32, vdst: u32, addr: u32, data0: u32) -> [u32; 2] {
    [0x36 << 26 | op << 18, vdst << 24 | data0 << 8 | addr]
}
const fn ds_write_b32(addr: u32, data: u32) -> [u32; 2] {
    ds(13, 0, addr, data)
}
const fn ds_read_b32(vdst: u32, addr: u32) -> [u32; 2] {
    ds(54, vdst, addr, 0)
}
/// `buffer_store_dword v<vdata>, off, s[8:11], 0`.
const fn store(vdata: u32) -> [u32; 2] {
    [0x38 << 26 | 0x1C << 18, 0x80 << 24 | 2 << 16 | vdata << 8]
}
/// VOP3 `op vdst, src0, src1, src2` (`src`: 0..105 SGPR, 128.. inline, 256.. VGPR).
pub(super) const fn vop3(op: u32, vdst: u32, src0: u32, src1: u32, src2: u32) -> [u32; 2] {
    [0x35 << 26 | op << 16 | vdst, src0 | src1 << 9 | src2 << 18]
}
const V: u32 = 256;

/// A console wave64 program of one wave sharing LDS without barriers, run as two host waves
/// of 32: a write, three reads, a write, a read. Without `LDSSYNC` the halves meet before
/// each of the six; with it only where an access can see or overwrite what the other half
/// did since they met: before the first access, the first read, the second write and the
/// read after it.
#[test]
fn lds_meetings_are_left_out_between_reads() {
    let mut words = vec![0x3402_0082]; // v_lshlrev_b32 v1, 2, v0
    words.extend(ds_write_b32(1, 0));
    words.extend(ds_read_b32(2, 1));
    words.extend(ds_read_b32(3, 1));
    words.extend(ds_read_b32(4, 1));
    words.extend(ds_write_b32(1, 2));
    words.extend(ds_read_b32(5, 1));
    words.extend(vop3(0x11D, 5, V + 5, V + 3, 0)); // v_xor_b32 v5, v5, v3
    words.extend(vop3(0x11D, 5, V + 5, V + 4, 0)); // v_xor_b32 v5, v5, v4
    words.extend(store(5));
    let native = native(&words, true, 64, 512);
    let off = lowered(&native, &HashSet::new(), 0);
    let on = lowered(&native, &HashSet::new(), opt::LDSSYNC);
    assert_eq!(off.barriers, 6, "one meeting per LDS access");
    assert_eq!(on.barriers, 4, "no meeting before the second and third read");
    // the other lowering bits leave this program as it is
    let rest = lowered(&native, &HashSet::new(), opt::MUL24 | opt::UNIFORM);
    assert_eq!(rest.dxil, off.dxil);
}

/// A store to other memory between two LDS reads keeps the meeting before the second.
#[test]
fn an_lds_meeting_after_another_store_is_kept() {
    let mut words = vec![0x3402_0082]; // v_lshlrev_b32 v1, 2, v0
    words.extend(ds_read_b32(2, 1));
    words.extend(store(2));
    words.extend(ds_read_b32(3, 1));
    words.extend(store(3));
    let native = native(&words, true, 64, 512);
    let off = lowered(&native, &HashSet::new(), 0);
    let on = lowered(&native, &HashSet::new(), opt::LDSSYNC);
    assert_eq!(off.barriers, 2);
    assert_eq!(on.barriers, 2);
}

/// A console wave64 program of one wave in two host waves of 32 that exchange through LDS
/// (a `v_readlane` makes them): a lane read of a value every lane holds (`v_mov v6, s2`),
/// and of one each lane holds its own (`v0`). With `UNIFORM` the first needs no exchange.
#[test]
fn a_lane_read_of_a_value_every_lane_holds_needs_no_exchange() {
    let mut words = vec![0x7E0C_0202]; // v_mov_b32 v6, s2
    words.extend(vop3(0x360, 7, V + 6, 128 + 37, 0)); // v_readlane_b32 s7, v6, 37
    words.extend(vop3(0x360, 12, V, 128 + 37, 0)); // v_readlane_b32 s12, v0, 37
    words.push(0x7E00_0207); // v_mov_b32 v0, s7
    words.push(0x7E02_020C); // v_mov_b32 v1, s12
    words.extend(vop3(0x11D, 0, V, V + 1, 0)); // v_xor_b32 v0, v0, v1
    words.extend(store(0));
    let native = native(&words, true, 64, 0);
    let off = lowered(&native, &HashSet::new(), 0);
    let on = lowered(&native, &HashSet::new(), opt::UNIFORM);
    eprintln!("barriers: {} without UNIFORM, {} with", off.barriers, on.barriers);
    assert!(off.barriers >= 4, "two exchanges: {}", off.barriers);
    assert_eq!(on.barriers, off.barriers - 2, "one exchange left");
}

/// `v_mul_u32_u24` / `v_mad_u32_u24` / `v_mul_hi_u32` on operands known to fit lose their
/// masks (and the high product its multiply) with `MUL24`; on operands that may not, the
/// unsigned mask is one `and` and the signed form is as before.
#[test]
fn multiplies_of_24_bits_keep_operands_that_fit() {
    // v1 = s0, v2 = s1 (any value); v3 = v1 >> 16, v4 = v2 >> 16 (16 bits)
    let mut head = vec![0x7E02_0200, 0x7E04_0201]; // v_mov_b32 v1, s0; v_mov_b32 v2, s1
    head.extend(vop3(0x116, 3, 128 + 16, V + 1, 0)); // v_lshrrev_b32 v3, 16, v1
    head.extend(vop3(0x116, 4, 128 + 16, V + 2, 0)); // v_lshrrev_b32 v4, 16, v2
    let cases: [(&str, [u32; 2], bool); 6] = [
        ("mul_u24 fits", vop3(0x10B, 0, V + 3, V + 4, 0), true),
        ("mul_i24 fits", vop3(0x109, 0, V + 3, V + 4, 0), true),
        ("mad_u24 fits", vop3(0x143, 0, V + 3, V + 4, V + 1), true),
        ("mul_hi_u32 fits", vop3(0x16A, 0, V + 3, V + 4, 0), true),
        ("mul_u24 any", vop3(0x10B, 0, V + 1, V + 2, 0), true),
        ("mul_i24 any", vop3(0x109, 0, V + 1, V + 2, 0), false),
    ];
    for (name, inst, smaller) in cases {
        let mut words = head.clone();
        words.extend(inst);
        words.extend(store(0));
        let native = native(&words, false, 1, 0);
        let off = lowered(&native, &HashSet::new(), 0);
        let on = lowered(&native, &HashSet::new(), opt::MUL24);
        eprintln!("{name}: {} -> {} bytes", off.dxil.len(), on.dxil.len());
        if smaller {
            assert!(on.dxil.len() < off.dxil.len(), "{name}: {} -> {}", off.dxil.len(), on.dxil.len());
        } else {
            assert_eq!(on.dxil, off.dxil, "{name}");
        }
    }
}

type Model = fn(u32, u32, u32) -> u32;

/// The 24-bit multiplies and high products of `multiplies_of_24_bits_keep_operands_that_fit`
/// and more, each leaving v0 (stored to dword 0), with the console's result of s0..s2.
fn multiply_cases() -> Vec<(&'static str, Vec<u32>, Model)> {
    let sh = |n: u32| 128 + n;
    let head = |mut w: Vec<u32>| {
        let mut words = vec![0x7E02_0200, 0x7E04_0201, 0x7E06_0202]; // v1..v3 = s0..s2
        words.append(&mut w);
        words.extend(store(0));
        words
    };
    vec![
        ("mul_u24 any", head(vop3(0x10B, 0, V + 1, V + 2, 0).to_vec()), |a, b, _| {
            (a & 0xFF_FFFF).wrapping_mul(b & 0xFF_FFFF)
        }),
        ("mul_i24 any", head(vop3(0x109, 0, V + 1, V + 2, 0).to_vec()), |a, b, _| {
            let s = |x: u32| ((x << 8) as i32 >> 8) as u32;
            s(a).wrapping_mul(s(b))
        }),
        (
            "mul_u24 fits",
            head([vop3(0x116, 5, sh(16), V + 1, 0), vop3(0x116, 6, sh(8), V + 2, 0), vop3(0x10B, 0, V + 5, V + 6, 0)].concat()),
            |a, b, _| (a >> 16).wrapping_mul(b >> 8),
        ),
        (
            "mul_i24 fits",
            head([vop3(0x116, 5, sh(9), V + 1, 0), vop3(0x116, 6, sh(9), V + 2, 0), vop3(0x109, 0, V + 5, V + 6, 0)].concat()),
            |a, b, _| (a >> 9).wrapping_mul(b >> 9),
        ),
        (
            "mul_i24 one fits",
            head([vop3(0x116, 5, sh(9), V + 1, 0), vop3(0x109, 0, V + 5, V + 2, 0)].concat()),
            |a, b, _| (a >> 9).wrapping_mul(((b << 8) as i32 >> 8) as u32),
        ),
        (
            "mul_u24 high bits",
            head([vop3(0x116, 5, sh(7), V + 1, 0), vop3(0x10B, 0, V + 5, V + 2, 0)].concat()),
            |a, b, _| ((a >> 7) & 0xFF_FFFF).wrapping_mul(b & 0xFF_FFFF),
        ),
        (
            "mul_hi_u32 zero",
            head([vop3(0x116, 5, sh(16), V + 1, 0), vop3(0x116, 6, sh(16), V + 2, 0), vop3(0x16A, 0, V + 5, V + 6, 0)].concat()),
            |a, b, _| ((u64::from(a >> 16) * u64::from(b >> 16)) >> 32) as u32,
        ),
        (
            "mul_hi_u32 not zero",
            head([vop3(0x116, 5, sh(15), V + 1, 0), vop3(0x116, 6, sh(16), V + 2, 0), vop3(0x16A, 0, V + 5, V + 6, 0)].concat()),
            |a, b, _| ((u64::from(a >> 15) * u64::from(b >> 16)) >> 32) as u32,
        ),
        ("mul_hi_u32 any", head(vop3(0x16A, 0, V + 1, V + 2, 0).to_vec()), |a, b, _| {
            ((u64::from(a) * u64::from(b)) >> 32) as u32
        }),
        (
            "mul_hi_i32 halves",
            head([vop3(0x116, 5, sh(1), V + 1, 0), vop3(0x116, 6, sh(1), V + 2, 0), vop3(0x16C, 0, V + 5, V + 6, 0)].concat()),
            |a, b, _| ((i64::from((a >> 1) as i32) * i64::from((b >> 1) as i32)) >> 32) as u32,
        ),
        ("mul_hi_i32 any", head(vop3(0x16C, 0, V + 1, V + 2, 0).to_vec()), |a, b, _| {
            ((i64::from(a as i32) * i64::from(b as i32)) >> 32) as u32
        }),
        ("mad_i24 any", head(vop3(0x142, 0, V + 1, V + 2, V + 3).to_vec()), |a, b, c| {
            let s = |x: u32| ((x << 8) as i32 >> 8) as u32;
            s(a).wrapping_mul(s(b)).wrapping_add(c)
        }),
        (
            "mad_u24 fits",
            head([vop3(0x116, 5, sh(16), V + 1, 0), vop3(0x143, 0, V + 5, V + 2, V + 3)].concat()),
            |a, b, c| (a >> 16).wrapping_mul(b & 0xFF_FFFF).wrapping_add(c),
        ),
    ]
}

/// Edge operands, then pseudo-random ones (xorshift32).
fn operand_sets() -> Vec<[u32; 3]> {
    let edges = [0u32, 1, 0x7F_FFFF, 0x80_0000, 0xFF_FFFF, 0x100_0000, 0x7FFF_FFFF, 0x8000_0000, u32::MAX];
    let mut sets = Vec::new();
    for &a in &edges {
        for &b in &edges {
            sets.push([a, b, a ^ b ^ 0x1234_5678]);
        }
    }
    let mut x = 0x9E37_79B9u32;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    for _ in 0..256 {
        sets.push([next(), next(), next()]);
    }
    sets
}

/// The differential test of the opt bits on the CPU: each program lowered with no bit,
/// with `MUL24` alone and with every bit (the module passes too), run over the finished
/// module ([`super::bitcode::Module::eval`]) on every operand set; all three store the
/// console's result.
#[test]
fn multiplies_with_and_without_the_opt_bits_agree_on_the_cpu() {
    let sets = operand_sets();
    for (name, words, model) in multiply_cases() {
        let native = native(&words, false, 1, 0);
        for bits in [0, opt::MUL24, opt::ALL] {
            // the user-data dwords: s0..s3 are dwords 0..3 (the table's address after them)
            let ins: Vec<Vec<u32>> = sets.iter().map(|s| vec![s[0], s[1], s[2], 0, 0, 0, 0, 0]).collect();
            super::shader::TEST_EVAL_IN.with(|c| *c.borrow_mut() = ins);
            let l = lowered(&native, &HashSet::new(), bits);
            super::shader::TEST_EVAL_IN.with(|c| c.borrow_mut().clear());
            assert!(l.user_data.iter().all(|u| u.root != RootType::Constants || u.index == u.dword), "{name}");
            let outs = super::shader::TEST_EVAL_OUT.with(|c| std::mem::take(&mut *c.borrow_mut()));
            assert_eq!(outs.len(), sets.len());
            for (s, out) in sets.iter().zip(outs) {
                let out = out.unwrap_or_else(|| panic!("{name}, opt {bits:#x}: not run on the CPU"));
                let got = out.iter().rev().find(|(at, _)| *at == 0).map(|x| x.1);
                assert_eq!(got, Some(model(s[0], s[1], s[2])), "{name}, opt {bits:#x}: {s:x?}");
            }
        }
        eprintln!("{name}: {} operand sets agree with and without the opt bits", sets.len());
    }
}
