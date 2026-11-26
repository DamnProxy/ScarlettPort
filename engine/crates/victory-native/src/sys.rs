//! Windows side: writing code, and the trampoline arena near the image.

use crate::{ReplaceError, ReplaceMode, Replacement, plan};
use std::sync::Mutex;
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualProtect,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Writes `new` over code at `at`, which must hold `expect` (same length).
/// The shared primitive for the loader's byte hooks and the replacements.
///
/// # Safety
/// `at..at+expect.len()` is mapped code no thread is executing.
pub unsafe fn write_code(at: usize, expect: &[u8], new: &[u8]) -> bool {
    if new.len() != expect.len() {
        return false;
    }
    let p = at as *mut u8;
    // SAFETY: mapped, per the contract.
    if unsafe { std::slice::from_raw_parts(p, expect.len()) } != expect {
        return false;
    }
    let mut old = 0;
    // SAFETY: only those bytes' protection, restored below.
    if unsafe { VirtualProtect(p as _, new.len(), PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
        return false;
    }
    // SAFETY: writable now.
    unsafe {
        std::ptr::copy_nonoverlapping(new.as_ptr(), p, new.len());
        VirtualProtect(p as _, new.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), p as _, new.len());
    }
    true
}

const ARENA: usize = 64 * 1024;

struct Arena {
    at: usize,
    used: usize,
}

static ARENAS: Mutex<Vec<Arena>> = Mutex::new(Vec::new());

/// Reserves an executable 64 KB block within ±2 GB of `near`, so relocated
/// RIP-relative operands still reach.
fn alloc_near(near: usize, size: usize) -> Option<usize> {
    const STEP: usize = 64 * 1024;
    const REACH: usize = 0x7000_0000;
    let lo = near.saturating_sub(REACH);
    let hi = near + size + REACH;
    let tries = |start: usize, down: bool| {
        let mut a = start & !(STEP - 1);
        loop {
            if a < lo + STEP || a > hi {
                return None;
            }
            // SAFETY: asks for fresh memory at a hint; the system refuses if taken.
            let p = unsafe { VirtualAlloc(a as _, ARENA, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READ) };
            if !p.is_null() {
                return Some(p as usize);
            }
            a = if down { a - STEP } else { a + STEP };
        }
    };
    tries(near + size + STEP, false).or_else(|| tries(near.saturating_sub(STEP), true))
}

/// Space for `len` trampoline bytes within reach of the image.
fn tramp_slot(base: usize, size: usize, len: usize) -> Option<usize> {
    let mut arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
    let reach = |a: usize| a.abs_diff(base) < 0x7000_0000 && (a + ARENA).abs_diff(base + size) < 0x7000_0000;
    let len16 = (len + 15) & !15;
    if let Some(a) = arenas.iter_mut().find(|a| reach(a.at) && a.used + len16 <= ARENA) {
        let at = a.at + a.used;
        a.used += len16;
        return Some(at);
    }
    let at = alloc_near(base, size)?;
    arenas.push(Arena { at, used: len16 });
    Some(at)
}

/// # Safety
/// As `crate::install`.
pub(crate) unsafe fn replace(
    base: usize,
    size: usize,
    r: &Replacement,
    target: usize,
    mode: ReplaceMode,
) -> Result<(usize, Option<usize>), ReplaceError> {
    let ip = base + r.rva;
    let avail = r.len.unwrap_or(64).max(r.guard.len()).min(size - r.rva);
    // SAFETY: inside the mapped image (checked by the caller).
    let code = unsafe { std::slice::from_raw_parts(ip as *const u8, avail) };

    // Plan once without a trampoline to learn the size, then place it.
    let mut p = plan::plan(code, ip as u64, r.guard, target as u64, r.len, None)?;
    let mut tramp = None;
    if mode == ReplaceMode::WithOriginal {
        // the relocated prologue grows at most ~4 bytes per instruction
        let budget = p.stolen * 3 + plan::ABS_JMP_LEN + 32;
        let at = tramp_slot(base, size, budget).ok_or_else(|| ReplaceError::Memory("no trampoline space within ±2 GB".into()))?;
        p = plan::plan(code, ip as u64, r.guard, target as u64, r.len, Some(at as u64))?;
        let t = p.trampoline.as_ref().expect("asked for");
        if t.len() > budget {
            return Err(ReplaceError::Memory("trampoline larger than its slot".into()));
        }
        let mut old = 0;
        // SAFETY: our own fresh slot; protection restored right after.
        unsafe {
            VirtualProtect(at as _, t.len(), PAGE_EXECUTE_READWRITE, &mut old);
            std::ptr::copy_nonoverlapping(t.as_ptr(), at as *mut u8, t.len());
            VirtualProtect(at as _, t.len(), old, &mut old);
            FlushInstructionCache(GetCurrentProcess(), at as _, t.len());
        }
        tramp = Some(at);
    }
    let before = code[..p.stolen].to_vec();
    // SAFETY: the entry, guarded; no title code runs (unsealed registry).
    if !unsafe { write_code(ip, &before, &p.entry) } {
        return Err(ReplaceError::Memory("VirtualProtect failed on the entry".into()));
    }
    Ok((p.stolen, tramp))
}

#[cfg(test)]
mod tests {
    use crate::*;

    /// Leaf: `lea eax,[rcx+rdx]` padded with NOPs; ret. Patch it to a Rust fn
    /// that calls the original through the trampoline and doubles the result.
    #[test]
    fn patched_function_runs_native_and_trampoline_runs_original() {
        use windows_sys::Win32::System::Memory::*;
        static mut ORIG: usize = 0;
        extern "C" fn doubled(a: i32, b: i32) -> i32 {
            // SAFETY: set before the call.
            let f: extern "C" fn(i32, i32) -> i32 = unsafe { std::mem::transmute(ORIG) };
            f(a, b) * 2
        }
        let mut code = vec![0x8D, 0x04, 0x11]; // lea eax,[rcx+rdx]
        code.extend_from_slice(&[0x90; 13]);
        code.push(0xC3);
        // SAFETY: a fresh RWX buffer for the synthetic function.
        let buf = unsafe { VirtualAlloc(std::ptr::null(), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE) } as usize;
        // SAFETY: inside the buffer.
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), buf as *mut u8, code.len()) };
        let guard: &'static [u8] = Box::leak(code[..16].to_vec().into_boxed_slice());
        reset_for_test(buf, 4096);
        let r = Replacement { what: "test", rva: 0, guard, len: Some(code.len()) };
        let t = replace(&r, doubled as *const (), ReplaceMode::WithOriginal).unwrap().unwrap();
        // SAFETY: single test thread uses it.
        unsafe { ORIG = t };
        // SAFETY: the patched buffer is a function of that type now.
        let f: extern "C" fn(i32, i32) -> i32 = unsafe { std::mem::transmute(buf) };
        assert_eq!(f(3, 4), 14);
        assert!(matches!(replace(&r, doubled as *const (), ReplaceMode::Jump), Err(ReplaceError::Overlap(0))));
    }
}
