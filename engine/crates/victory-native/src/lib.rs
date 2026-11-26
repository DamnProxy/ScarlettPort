//! Native port infrastructure: replaces title functions with PC-native
//! implementations.
//!
//! The title's x86-64 code runs in our process. A *native module* swaps some
//! of its functions for Rust `extern "C"` (Win64) ones by patching the entry
//! with an absolute jump, optionally keeping a trampoline to the original.
//!
//! Flow: the loader maps and relocates the image, applies its own byte hooks,
//! then calls [`install`]. That enables the modules named in `VICTORY_NATIVE`
//! (comma list, or `all`) whose build matches, each calling [`replace`], and
//! seals the registry: no patch is written once title code may be running.
//!
//! No title code lives here: a module names an address and the guard bytes it
//! expects, and this snapshot registers no module (see `modules`).

pub mod modules;
pub mod plan;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
pub use sys::write_code;

use std::sync::Mutex;

pub use plan::{ABS_JMP_LEN, PlanError};

/// Environment variable naming the native modules to enable (`a,b` or `all`).
pub const NATIVE_ENV: &str = "VICTORY_NATIVE";

/// Whether a replacement keeps a way back to the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceMode {
    /// Jump only; the original is gone.
    Jump,
    /// Jump, plus a trampoline running the original (its relocated prologue,
    /// then the rest in place). The trampoline's address is returned.
    WithOriginal,
}

/// One replacement request.
#[derive(Debug, Clone, Copy)]
pub struct Replacement {
    /// What it is, for the log.
    pub what: &'static str,
    /// The function's RVA in the image.
    pub rva: usize,
    /// The bytes the entry must hold (version guard), at least [`ABS_JMP_LEN`].
    pub guard: &'static [u8],
    /// The function's size if known: enables the body checks.
    pub len: Option<usize>,
}

/// Why [`replace`] refused.
#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    #[error("no image installed yet (the loader calls victory_native::install)")]
    NoImage,
    #[error("sealed: title code may be running, patches are only written before it starts")]
    Sealed,
    #[error("rva {0:#x} is outside the image")]
    OutOfImage(usize),
    #[error("rva {0:#x} overlaps a replacement already made")]
    Overlap(usize),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("memory: {0}")]
    Memory(String),
    #[error("not supported on this platform")]
    Unsupported,
}

/// A native module: a named group of replacements for one build.
pub struct NativeModule {
    pub name: &'static str,
    /// Prefix of the BLAKE3 (hex) of the original image it targets.
    pub build: &'static str,
    /// Makes its replacements through [`replace`]; returns how many it made.
    pub install: fn() -> Result<usize, ReplaceError>,
}

struct Registry {
    base: usize,
    size: usize,
    sealed: bool,
    /// Patched entry ranges (rva, len).
    patched: Vec<(usize, usize)>,
}

static REG: Mutex<Registry> = Mutex::new(Registry { base: 0, size: 0, sealed: false, patched: Vec::new() });

fn lock() -> std::sync::MutexGuard<'static, Registry> {
    REG.lock().unwrap_or_else(|e| e.into_inner())
}

/// Parses `VICTORY_NATIVE`: does it enable `name`?
pub fn wants(list: &str, name: &str) -> bool {
    list.split([',', ';', ' '])
        .map(str::trim)
        .any(|m| m.eq_ignore_ascii_case(name) || m.eq_ignore_ascii_case("all"))
}

/// The image's base once installed (for modules computing addresses).
pub fn image_base() -> Option<usize> {
    let r = lock();
    (r.base != 0).then_some(r.base)
}

/// Replaces the title function at `rva` with `fn_ptr`.
/// Returns the trampoline to the original for [`ReplaceMode::WithOriginal`].
///
/// Thread-safe (one registry lock around plan and write); refuses after the
/// registry is sealed, on a guard mismatch, and on overlap with an earlier
/// replacement. The loader's own byte hooks run first; one at the same place
/// makes the guard fail.
pub fn replace(r: &Replacement, fn_ptr: *const (), mode: ReplaceMode) -> Result<Option<usize>, ReplaceError> {
    let mut reg = lock();
    if reg.base == 0 {
        return Err(ReplaceError::NoImage);
    }
    if reg.sealed {
        return Err(ReplaceError::Sealed);
    }
    let span = r.len.unwrap_or(0).max(r.guard.len());
    if r.rva.checked_add(span).is_none_or(|e| e > reg.size) {
        return Err(ReplaceError::OutOfImage(r.rva));
    }
    if reg.patched.iter().any(|&(a, n)| r.rva < a + n && a < r.rva + ABS_JMP_LEN.max(r.guard.len())) {
        return Err(ReplaceError::Overlap(r.rva));
    }
    let res = imp_replace(&reg, r, fn_ptr as usize, mode);
    match &res {
        Ok((stolen, tramp)) => {
            reg.patched.push((r.rva, *stolen));
            tracing::info!(what = r.what, rva = format_args!("{:#x}", r.rva), trampoline = tramp.is_some(), "native: replaced");
        }
        Err(e) => tracing::warn!(what = r.what, rva = format_args!("{:#x}", r.rva), "native: refused: {e}"),
    }
    res.map(|(_, t)| t)
}

#[cfg(windows)]
fn imp_replace(reg: &Registry, r: &Replacement, target: usize, mode: ReplaceMode) -> Result<(usize, Option<usize>), ReplaceError> {
    // SAFETY: base/size describe the mapped image (install's contract); the
    // registry is unsealed, so no title code runs.
    unsafe { sys::replace(reg.base, reg.size, r, target, mode) }
}

#[cfg(not(windows))]
fn imp_replace(_: &Registry, _: &Replacement, _: usize, _: ReplaceMode) -> Result<(usize, Option<usize>), ReplaceError> {
    Err(ReplaceError::Unsupported)
}

/// Called by the loader for every mapped image, after relocation and its own
/// hooks, before any title code runs. Enables the `VICTORY_NATIVE` modules
/// matching `hash_hex`, then seals. Returns how many replacements were made.
///
/// # Safety
/// `base..base+size` is the mapped image and no title code has run.
pub unsafe fn install(base: usize, size: usize, hash_hex: &str) -> usize {
    let Ok(list) = std::env::var(NATIVE_ENV) else { return 0 };
    let chosen: Vec<&NativeModule> = modules::ALL
        .iter()
        .filter(|m| wants(&list, m.name) && hash_hex.starts_with(m.build))
        .collect();
    if chosen.is_empty() {
        return 0;
    }
    {
        let mut r = lock();
        if r.base != 0 && r.base != base {
            tracing::warn!("native: a second image matched; only the first is patched");
            return 0;
        }
        *r = Registry { base, size, sealed: false, patched: Vec::new() };
    }
    let mut n = 0;
    for m in chosen {
        match (m.install)() {
            Ok(k) => {
                tracing::info!(module = m.name, replaced = k, "native module on");
                n += k;
            }
            Err(e) => tracing::warn!(module = m.name, "native module failed: {e}"),
        }
    }
    lock().sealed = true;
    n
}

/// For tests: point the registry at a buffer and unseal it.
#[doc(hidden)]
pub fn reset_for_test(base: usize, size: usize) {
    *lock() = Registry { base, size, sealed: false, patched: Vec::new() };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_list_parsing() {
        assert!(wants("hash", "hash"));
        assert!(wants("render, Hash", "hash"));
        assert!(wants("all", "hash"));
        assert!(!wants("hashx", "hash"));
        assert!(!wants("", "hash"));
    }
}
