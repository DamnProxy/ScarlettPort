//! Platform-neutral patch planning: which bytes go over a title function's
//! entry, and what the trampoline back to the original holds. Pure functions
//! over byte buffers, so they are unit-tested on synthetic code.

use iced_x86::{
    BlockEncoder, BlockEncoderOptions, Decoder, DecoderOptions, FlowControl, Instruction,
    InstructionBlock,
};

/// `jmp qword ptr [rip+0]` followed by the 64-bit target: 14 bytes, reaches anywhere.
pub const ABS_JMP_LEN: usize = 14;

/// The absolute jump to `target`.
pub fn abs_jmp(target: u64) -> [u8; ABS_JMP_LEN] {
    let mut b = [0u8; ABS_JMP_LEN];
    b[..6].copy_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    b[6..].copy_from_slice(&target.to_le_bytes());
    b
}

/// Why a patch was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("prologue guard mismatch: the bytes at the entry are not the expected ones")]
    GuardMismatch,
    #[error("prologue guard is {0} bytes; at least {ABS_JMP_LEN} are needed to cover the jump")]
    GuardTooShort(usize),
    #[error("function is {0} bytes, shorter than the {ABS_JMP_LEN}-byte jump")]
    FunctionTooShort(usize),
    #[error("undecodable instruction at entry+{0:#x}")]
    BadInstruction(usize),
    #[error("the function ends (ret/jmp) at entry+{0:#x}, inside the bytes the jump overwrites")]
    EndsInsideStolen(usize),
    #[error("a branch at entry+{from:#x} lands at entry+{to:#x}, inside the overwritten bytes")]
    BranchIntoStolen { from: usize, to: usize },
    #[error("cannot relocate the prologue into the trampoline: {0}")]
    Relocate(String),
}

/// What to write.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Bytes for the entry: the absolute jump, then `int3` up to an instruction boundary.
    pub entry: Vec<u8>,
    /// How many entry bytes the jump replaces (whole instructions).
    pub stolen: usize,
    /// Trampoline bytes to place at `tramp_ip` (relocated prologue + jump back), if asked.
    pub trampoline: Option<Vec<u8>>,
}

/// Plans a patch of the function at `ip` whose current bytes are `code`.
///
/// * `guard`: the bytes the entry must hold (at least [`ABS_JMP_LEN`]); version guard.
/// * `fn_len`: the function's size if known; enables the too-short and
///   branch-into-overwritten-bytes checks over the whole body.
/// * `tramp_ip`: where the trampoline will live, if a trampoline is wanted.
///   RIP-relative operands and relative branches are re-encoded for that
///   address; it fails if they cannot reach (keep the trampoline within ±2 GB).
pub fn plan(
    code: &[u8],
    ip: u64,
    guard: &[u8],
    target: u64,
    fn_len: Option<usize>,
    tramp_ip: Option<u64>,
) -> Result<Plan, PlanError> {
    if guard.len() < ABS_JMP_LEN {
        return Err(PlanError::GuardTooShort(guard.len()));
    }
    if code.len() < guard.len() || &code[..guard.len()] != guard {
        return Err(PlanError::GuardMismatch);
    }
    if let Some(n) = fn_len
        && n < ABS_JMP_LEN
    {
        return Err(PlanError::FunctionTooShort(n));
    }
    let body = &code[..fn_len.unwrap_or(code.len()).min(code.len())];

    // Whole instructions covering the jump.
    let mut dec = Decoder::with_ip(64, body, ip, DecoderOptions::NONE);
    let mut stolen_insns: Vec<Instruction> = Vec::new();
    let mut stolen = 0usize;
    while stolen < ABS_JMP_LEN {
        if !dec.can_decode() {
            return Err(PlanError::BadInstruction(stolen));
        }
        let ins = dec.decode();
        if ins.is_invalid() {
            return Err(PlanError::BadInstruction(stolen));
        }
        stolen += ins.len();
        // A function that returns or jumps away before the jump's end would
        // have the jump spill over whatever follows it.
        if stolen < ABS_JMP_LEN
            && matches!(
                ins.flow_control(),
                FlowControl::Return | FlowControl::UnconditionalBranch | FlowControl::IndirectBranch
            )
        {
            return Err(PlanError::EndsInsideStolen(stolen - ins.len()));
        }
        stolen_insns.push(ins);
    }

    // Branches back into the overwritten bytes (other than the entry) would
    // land in the middle of the jump. Checked over the stolen instructions
    // and, when the size is known, the whole body.
    let lo = ip + 1;
    let hi = ip + stolen as u64;
    let check = |ins: &Instruction| -> Result<(), PlanError> {
        if matches!(
            ins.flow_control(),
            FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch | FlowControl::XbeginXabortXend
        ) {
            let t = ins.near_branch_target();
            if (lo..hi).contains(&t) {
                return Err(PlanError::BranchIntoStolen {
                    from: (ins.ip() - ip) as usize,
                    to: (t - ip) as usize,
                });
            }
        }
        Ok(())
    };
    for ins in &stolen_insns {
        check(ins)?;
    }
    if fn_len.is_some() {
        for ins in dec.iter() {
            if !ins.is_invalid() {
                check(&ins)?;
            }
        }
    }

    let mut entry = abs_jmp(target).to_vec();
    entry.resize(stolen, 0xCC);

    let trampoline = match tramp_ip {
        None => None,
        Some(t) => {
            let block = InstructionBlock::new(&stolen_insns, t);
            let enc = BlockEncoder::encode(64, block, BlockEncoderOptions::NONE)
                .map_err(|e| PlanError::Relocate(e.to_string()))?;
            let mut bytes = enc.code_buffer;
            bytes.extend_from_slice(&abs_jmp(ip + stolen as u64));
            Some(bytes)
        }
    };
    Ok(Plan { entry, stolen, trampoline })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_x86::{Code, Mnemonic};

    const IP: u64 = 0x1_4000_1000;
    const TARGET: u64 = 0x7FF6_1234_5678;

    fn decode_all(b: &[u8], ip: u64) -> Vec<Instruction> {
        Decoder::with_ip(64, b, ip, DecoderOptions::NONE).iter().collect()
    }

    /// A synthetic loop-shaped leaf: short jcc in the prologue, loop back to +0x10.
    fn hash_like() -> Vec<u8> {
        let mut v = vec![
            0x8A, 0x01, // mov al, [rcx]
            0x3C, 0x00, // cmp al, 0
            0x74, 0x10, // je +0x10 -> 0x16
            0x48, 0x83, 0xC1, 0x01, // add rcx, 1
            0x66, 0x0F, 0x1F, 0x44, 0x00, 0x00, // 6-byte nop
            0x01, 0xC2, // 0x10: add edx, eax
            0x8A, 0x01, // mov al, [rcx]
            0x84, 0xC0, // test al, al
        ];
        v.extend_from_slice(&[0x75, 0xF8]); // 0x16: jne -> 0x10
        v.push(0xC3); // 0x18: ret
        v
    }

    #[test]
    fn entry_is_abs_jump_padded_to_whole_instructions() {
        let c = hash_like();
        let p = plan(&c, IP, &c[..16], TARGET, Some(c.len()), None).unwrap();
        assert_eq!(p.stolen, 16);
        assert_eq!(&p.entry[..6], &[0xFF, 0x25, 0, 0, 0, 0]);
        assert_eq!(u64::from_le_bytes(p.entry[6..14].try_into().unwrap()), TARGET);
        assert_eq!(&p.entry[14..], &[0xCC, 0xCC]);
        let i = decode_all(&p.entry[..14], IP);
        assert_eq!(i[0].mnemonic(), Mnemonic::Jmp);
        assert!(p.trampoline.is_none());
    }

    #[test]
    fn short_jcc_in_prologue_is_relocated_to_its_original_target() {
        let c = hash_like();
        let tramp = IP + 0x10_0000;
        let p = plan(&c, IP, &c[..16], TARGET, Some(c.len()), Some(tramp)).unwrap();
        let t = p.trampoline.unwrap();
        let ins = decode_all(&t, tramp);
        let je = ins.iter().find(|i| i.mnemonic() == Mnemonic::Je).unwrap();
        assert_eq!(je.near_branch_target(), IP + 0x16);
        // the tail is the absolute jump back past the stolen bytes
        let tail = &t[t.len() - ABS_JMP_LEN..];
        assert_eq!(&tail[..6], &[0xFF, 0x25, 0, 0, 0, 0]);
        assert_eq!(u64::from_le_bytes(tail[6..].try_into().unwrap()), IP + 16);
    }

    #[test]
    fn rip_relative_operand_keeps_its_absolute_address() {
        let mut c = vec![0x48, 0x8B, 0x05, 0x00, 0x01, 0x00, 0x00]; // mov rax, [rip+0x100]
        c.extend_from_slice(&[0x48, 0x8D, 0x0D, 0x10, 0x00, 0x00, 0x00]); // lea rcx, [rip+0x10]
        c.extend_from_slice(&[0x48, 0x01, 0xC8, 0xC3]); // add rax, rcx; ret
        let tramp = IP - 0x4000_0000;
        let p = plan(&c, IP, &c[..14], TARGET, Some(c.len()), Some(tramp)).unwrap();
        assert_eq!(p.stolen, 14);
        let ins = decode_all(&p.trampoline.unwrap(), tramp);
        assert_eq!(ins[0].code(), Code::Mov_r64_rm64);
        assert!(ins[0].is_ip_rel_memory_operand());
        assert_eq!(ins[0].ip_rel_memory_address(), IP + 7 + 0x100);
        assert_eq!(ins[1].ip_rel_memory_address(), IP + 14 + 0x10);
    }

    #[test]
    fn rip_relative_out_of_reach_is_refused() {
        let mut c = vec![0x48, 0x8B, 0x05, 0x00, 0x01, 0x00, 0x00];
        c.extend_from_slice(&[0x48, 0x8D, 0x0D, 0x10, 0x00, 0x00, 0x00, 0xC3]);
        let far = IP + 0x1_0000_0000;
        assert!(matches!(
            plan(&c, IP, &c[..14], TARGET, None, Some(far)),
            Err(PlanError::Relocate(_))
        ));
    }

    #[test]
    fn guard_mismatch_and_short_guard_are_refused() {
        let c = hash_like();
        let mut g = c[..16].to_vec();
        g[3] ^= 1;
        assert_eq!(plan(&c, IP, &g, TARGET, None, None).unwrap_err(), PlanError::GuardMismatch);
        assert_eq!(
            plan(&c, IP, &c[..8], TARGET, None, None).unwrap_err(),
            PlanError::GuardTooShort(8)
        );
    }

    #[test]
    fn function_returning_inside_the_jump_is_refused() {
        // xor eax,eax; ret; then the next function
        let mut c = vec![0x31, 0xC0, 0xC3];
        c.extend_from_slice(&[0x90; 16]);
        assert_eq!(
            plan(&c, IP, &c[..14], TARGET, None, None).unwrap_err(),
            PlanError::EndsInsideStolen(2)
        );
        assert_eq!(
            plan(&c, IP, &c[..14], TARGET, Some(3), None).unwrap_err(),
            PlanError::FunctionTooShort(3)
        );
    }

    #[test]
    fn branch_back_into_overwritten_bytes_is_refused() {
        // 0: nop*5; 5: dec ecx (2); 7: nop*9; 0x10: jne -> 5; ret
        let mut c = vec![0x90; 5];
        c.extend_from_slice(&[0xFF, 0xC9]);
        c.extend_from_slice(&[0x90; 9]);
        c.extend_from_slice(&[0x75, 0xF3, 0xC3]);
        assert_eq!(
            plan(&c, IP, &c[..14], TARGET, Some(c.len()), None).unwrap_err(),
            PlanError::BranchIntoStolen { from: 0x10, to: 5 }
        );
    }
}
