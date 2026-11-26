//! The Windows side of `hidpad`: finding Sony pads (SetupDi over the HID
//! class), one reader thread per pad, rumble out through that same thread.
//!
//! A blocking `ReadFile` and a `WriteFile` on one synchronous handle
//! serialise, so the reader thread is the only one that touches a handle:
//! after each input report it sends the rumble the title last asked for if
//! it changed (and now and then while it is on, as a pad stops a rumble
//! that is not renewed). Reports come at 250 Hz or more, so rumble lags by
//! one report at most.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use victory_core::kbm::PadState;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HIDD_ATTRIBUTES, HIDP_CAPS, HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature,
    HidD_GetHidGuid, HidD_GetPreparsedData, HidP_GetCaps,
};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
};

use super::{Bus, Model, Output, SLOTS, ids_from_path, model_of, needs_full_mode, output_report, parse};

/// How often to look for pads that came or went.
const SCAN: Duration = Duration::from_secs(1);
/// A rumble that is on is sent again this often.
const RENEW: Duration = Duration::from_millis(250);
/// The full-mode switch is retried this often while the short report comes.
const SWITCH_RETRY: Duration = Duration::from_secs(1);
const NO_SLOT: usize = usize::MAX;

/// What the reader thread and the title's polling share for one pad.
struct Pad {
    id: u64,
    model: Model,
    bus: Bus,
    /// The last decoded state; `None` until the first usable report.
    state: Mutex<Option<PadState>>,
    alive: AtomicBool,
    /// Wanted motor speeds: low in the high half, high in the low half.
    rumble: AtomicU32,
    /// The slot the title sees it in (the player indicator).
    slot: AtomicUsize,
}

/// The pads found so far, kept up to date by a scanning thread.
pub struct Hid {
    pads: Arc<Mutex<Vec<Arc<Pad>>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Hid {
    pub fn start() -> Hid {
        let pads: Arc<Mutex<Vec<Arc<Pad>>>> = Arc::default();
        let shared = pads.clone();
        std::thread::Builder::new()
            .name("hid-scan".into())
            .spawn(move || {
                // Paths already opened or turned down: not opened again
                // until they go away.
                let mut known: HashSet<String> = HashSet::new();
                let mut owned: Vec<(String, Arc<Pad>)> = Vec::new();
                loop {
                    let found = enumerate();
                    owned.retain(|(path, pad)| {
                        let live = pad.alive.load(Ordering::Relaxed);
                        if !live {
                            tracing::info!(id = pad.id, "Sony pad gone");
                            known.remove(path);
                        }
                        live
                    });
                    known.retain(|p| found.contains(p) || owned.iter().any(|(o, _)| o == p));
                    for path in found {
                        if !known.insert(path.clone()) {
                            continue;
                        }
                        if let Some(pad) = open(&path) {
                            lock(&shared).push(pad.clone());
                            owned.push((path, pad));
                        }
                    }
                    lock(&shared).retain(|p| p.alive.load(Ordering::Relaxed));
                    std::thread::sleep(SCAN);
                }
            })
            .ok();
        Hid { pads }
    }

    /// The pads that have reported, in connection order.
    pub fn snapshot(&self) -> Vec<(u64, PadState)> {
        lock(&self.pads)
            .iter()
            .filter_map(|p| (*lock(&p.state)).map(|s| (p.id, s)))
            .collect()
    }

    /// Motor speeds for a pad, sent by its thread with the next report.
    pub fn rumble(&self, id: u64, low: u16, high: u16) {
        if let Some(p) = lock(&self.pads).iter().find(|p| p.id == id) {
            p.rumble.store(u32::from(low) << 16 | u32::from(high), Ordering::Relaxed);
        }
    }

    /// Tells a pad which slot it sits in (its player indicator).
    pub fn set_slot(&self, id: u64, slot: Option<usize>) {
        if let Some(p) = lock(&self.pads).iter().find(|p| p.id == id) {
            p.slot.store(slot.unwrap_or(NO_SLOT), Ordering::Relaxed);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A stable id from a device path (FNV-1a).
fn id_of(path: &str) -> u64 {
    path.to_ascii_lowercase()
        .bytes()
        .fold(0xCBF2_9CE4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3))
}

/// Paths of the present HID interfaces whose IDs are a Sony pad's.
fn enumerate() -> Vec<String> {
    let mut found = Vec::new();
    // SAFETY: SetupDi calls with buffers of the sizes passed; the info set
    // is destroyed on every path out.
    unsafe {
        let mut guid = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(&guid, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set as isize == INVALID_HANDLE_VALUE as isize {
            return found;
        }
        let mut index = 0;
        loop {
            let mut data: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            data.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut data) == 0 {
                break;
            }
            index += 1;
            let mut need = 0_u32;
            SetupDiGetDeviceInterfaceDetailW(set, &data, std::ptr::null_mut(), 0, &mut need, std::ptr::null_mut());
            if need < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 {
                continue;
            }
            // 8-byte aligned storage for the variable-length detail.
            let mut buffer = vec![0_u64; (need as usize).div_ceil(8)];
            let detail = buffer.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &data, detail, need, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                continue;
            }
            let start = (&raw const (*detail).DevicePath).cast::<u16>();
            let mut len = 0;
            while *start.add(len) != 0 {
                len += 1;
            }
            let path = String::from_utf16_lossy(std::slice::from_raw_parts(start, len));
            if ids_from_path(&path).and_then(|(v, p)| model_of(v, p)).is_some() {
                found.push(path);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    found
}

/// A device handle, closed when dropped.
struct Handle(isize);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle we opened.
        unsafe { CloseHandle(self.0 as _) };
    }
}

struct Opened {
    handle: Handle,
    model: Model,
    bus: Bus,
    input_len: usize,
    output_len: usize,
    feature_len: usize,
    /// Opened without write access: no rumble.
    read_only: bool,
}

fn open_device(path: &str) -> Option<Opened> {
    let wide_path = wide(path);
    let create = |access: u32| {
        // SAFETY: a NUL-terminated path.
        let h = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        (h as isize != INVALID_HANDLE_VALUE as isize).then_some(Handle(h as isize))
    };
    let (handle, read_only) = match create(GENERIC_READ | GENERIC_WRITE) {
        Some(h) => (h, false),
        None => (create(GENERIC_READ)?, true),
    };
    let mut attributes = HIDD_ATTRIBUTES {
        Size: size_of::<HIDD_ATTRIBUTES>() as u32,
        VendorID: 0,
        ProductID: 0,
        VersionNumber: 0,
    };
    // SAFETY: a valid handle and a sized attributes block.
    if unsafe { !HidD_GetAttributes(handle.0 as _, &mut attributes) } {
        return None;
    }
    let model = model_of(attributes.VendorID, attributes.ProductID)?;
    let mut preparsed = 0;
    // SAFETY: as above; the preparsed data is freed before leaving.
    let caps = unsafe {
        if !HidD_GetPreparsedData(handle.0 as _, &mut preparsed) {
            return None;
        }
        let mut caps: HIDP_CAPS = std::mem::zeroed();
        let status = HidP_GetCaps(preparsed, &mut caps);
        HidD_FreePreparsedData(preparsed);
        if status != 0x0011_0000 {
            return None;
        }
        caps
    };
    // Generic Desktop: Game Pad (5) or Joystick (4). A DualSense and a
    // DS4 expose their touchpad, motion and audio elsewhere or not at all.
    if caps.UsagePage != 1 || !matches!(caps.Usage, 4 | 5) {
        return None;
    }
    let input_len = usize::from(caps.InputReportByteLength);
    Some(Opened {
        handle,
        model,
        bus: super::bus_of(input_len),
        input_len,
        output_len: usize::from(caps.OutputReportByteLength),
        feature_len: usize::from(caps.FeatureReportByteLength),
        read_only,
    })
}

fn write(handle: &Handle, report: &[u8]) -> bool {
    let mut written = 0;
    // SAFETY: a valid handle and a buffer of the length given.
    unsafe {
        WriteFile(handle.0 as _, report.as_ptr().cast(), report.len() as u32, &mut written, std::ptr::null_mut()) != 0
    }
}

/// Switches a Bluetooth pad from the short report to the full one: reading
/// its calibration feature report and sending the first output report. The
/// pad then sends 0x11 (DS4) or 0x31 (DualSense).
fn full_mode(o: &Opened, out: Output) {
    let feature = match o.model {
        Model::Ds4 => 0x02,
        Model::DualSense => 0x05,
    };
    let mut buffer = vec![0_u8; o.feature_len.max(64)];
    buffer[0] = feature;
    // SAFETY: a valid handle and a buffer of at least the declared size.
    unsafe { HidD_GetFeature(o.handle.0 as _, buffer.as_mut_ptr().cast::<c_void>(), buffer.len() as u32) };
    send(o, out);
}

fn send(o: &Opened, out: Output) -> bool {
    if o.read_only {
        return false;
    }
    let mut report = output_report(o.model, o.bus, out);
    // The device wants exactly its declared length.
    if o.output_len != 0 && o.output_len != report.len() {
        report.resize(o.output_len, 0);
    }
    write(&o.handle, &report)
}

fn open(path: &str) -> Option<Arc<Pad>> {
    let opened = open_device(path)?;
    let pad = Arc::new(Pad {
        id: id_of(path),
        model: opened.model,
        bus: opened.bus,
        state: Mutex::new(None),
        alive: AtomicBool::new(true),
        rumble: AtomicU32::new(0),
        slot: AtomicUsize::new(NO_SLOT),
    });
    tracing::info!(id = pad.id, model = ?pad.model, bus = ?pad.bus, "Sony pad found");
    let thread_pad = pad.clone();
    std::thread::Builder::new()
        .name("hid-pad".into())
        .spawn(move || {
            read_loop(&opened, &thread_pad);
            thread_pad.alive.store(false, Ordering::Relaxed);
        })
        .ok()?;
    Some(pad)
}

fn read_loop(o: &Opened, pad: &Pad) {
    let mut buffer = vec![0_u8; o.input_len.max(64)];
    let mut sent = (0_u16, 0_u16, NO_SLOT);
    let mut sent_at = Instant::now();
    let mut seq = 0_u8;
    let mut switched: Option<Instant> = None;
    let mut first = true;
    loop {
        let mut read = 0;
        // SAFETY: a valid handle and a buffer of the length given.
        let ok = unsafe {
            ReadFile(o.handle.0 as _, buffer.as_mut_ptr().cast(), buffer.len() as u32, &mut read, std::ptr::null_mut())
        };
        if ok == 0 {
            return; // unplugged, or the link dropped
        }
        let report = &buffer[..read as usize];
        let wanted = pad.rumble.load(Ordering::Relaxed);
        let slot = pad.slot.load(Ordering::Relaxed);
        let out = |seq: u8| Output {
            low: (wanted >> 16) as u16,
            high: wanted as u16,
            slot: if slot == NO_SLOT { 0 } else { slot.min(SLOTS - 1) },
            seq,
        };
        if needs_full_mode(o.model, o.bus, report) {
            if switched.is_none_or(|at| at.elapsed() >= SWITCH_RETRY) {
                switched = Some(Instant::now());
                full_mode(o, out(seq));
                seq = seq.wrapping_add(1);
                sent_at = Instant::now();
            }
        } else if first || (out(0).low, out(0).high, slot) != sent || (wanted != 0 && sent_at.elapsed() >= RENEW) {
            first = false;
            if send(o, out(seq)) {
                seq = seq.wrapping_add(1);
            }
            sent = (out(0).low, out(0).high, slot);
            sent_at = Instant::now();
        }
        if let Some(input) = parse(o.model, o.bus, report) {
            *lock(&pad.state) = Some(input.pad);
        }
    }
}
