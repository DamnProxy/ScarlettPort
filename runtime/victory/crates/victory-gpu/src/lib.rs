//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Planned:
//! `present` (swap chain).

#[cfg(all(test, windows))]
mod agility;
#[cfg(windows)]
mod ab;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg(windows)]
mod cardmeter;
#[cfg(windows)]
mod gpuprof;
pub mod console;
pub mod cores;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod drvwatch;
#[cfg(windows)]
mod draw;
pub mod dxgi;
pub mod fence;
pub mod frame;
#[cfg(windows)]
pub mod framelog;
#[cfg(windows)]
mod host;
#[cfg(windows)]
pub mod fsr;
#[cfg(windows)]
mod over;
mod overlay;
#[cfg(windows)]
mod indirect;
#[cfg(windows)]
mod inplace;
pub mod list;
pub mod native;
pub mod object;
pub mod pipeline;
#[cfg(windows)]
mod plaincopy;
#[cfg(windows)]
mod ranges;
#[cfg(windows)]
mod primitive;
#[cfg(all(windows, feature = "diagnostics"))]
mod fxprof;
#[cfg(all(windows, feature = "diagnostics"))]
mod profile;
pub mod query;
#[cfg(windows)]
mod mem2;
#[cfg(windows)]
mod memcopy;
mod memopt;
mod narrow;
#[cfg(windows)]
mod translate;
#[cfg(windows)]
mod readback;
#[cfg_attr(not(windows), allow(dead_code))]
mod readfix;
mod rebar;
#[cfg(windows)]
mod regions;
pub mod queue;
#[cfg(windows)]
mod queue2;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod census;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
mod shadow;
#[cfg(windows)]
mod safe_copy;
#[cfg(windows)]
mod tiles;
pub mod signature;
#[cfg(windows)]
pub mod splash;
#[cfg(windows)]
mod texture;
#[cfg(windows)]
mod watch;
#[cfg(windows)]
mod zerocopy;
mod vram;

#[cfg(windows)]
pub use host::on_failure;
#[cfg(windows)]
pub use census::title_memory_reserved;
#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_section, title_memory_view};
#[cfg(windows)]
pub use zerocopy::sections as zero_copy_sections;
#[cfg(windows)]
pub use regions::{title_memory_not_placeholder, title_memory_placeholder};
#[cfg(windows)]
pub use mem2::use_cpu_free;
#[cfg(windows)]
pub use shadow::gpu_only_exposed;
#[cfg(windows)]
pub use translate::{
    enabled as mem2_gpu_only_noview, flush_unmaps as translate_flush_unmaps, lazy as remap_lazy, lazy_call, title_memory_translated,
    title_memory_untranslated, shown as title_memory_in_table, write_through as title_memory_write_through, parking as remap_park,
};
#[cfg(windows)]
pub use watch::{
    count_remap, physical_written, title_memory_gpu_only, title_memory_reading, use_title_memory, title_memory_remapped,
    title_memory_remapping, title_memory_remapping_is_global, title_memory_remapping_many, title_memory_remapped_many,
    writing as kernel_will_write, title_memory_written, Remapping,
};

/// Whether `VICTORY_MEM2_GPU_ONLY_NOSNAP` is on (xmem then keeps the record
/// of which pages the CPU can reach).
#[cfg(windows)]
pub fn mem2_nosnap() -> bool {
    mem2::nosnap()
}

/// Total installed physical RAM in bytes, queried once from the OS.
#[cfg(windows)]
pub(crate) fn total_system_ram_bytes() -> Option<u64> {
    static TOTAL: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *TOTAL.get_or_init(|| {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: plain data the call fills in.
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: a sized structure.
        let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
        (ok && m.ullTotalPhys > 0).then_some(m.ullTotalPhys)
    })
}
