//! The load cache (`VICTORY_LOAD_CACHE=1`): the decoded output of the
//! title's file requests, recorded on one boot and served on the next with
//! no read of the dump and no decode.
//!
//! - Key: the file (its path, size and modification time, hashed when the
//!   title opens it), the offset, the source, destination and intermediate
//!   sizes, and the codec bits. A changed dump file changes its size or time
//!   and so only misses.
//! - Each entry also keeps a hash of the compressed source it was decoded
//!   from (`VICTORY_LOAD_CACHE_VERIFY=1` reads the source again and serves
//!   only when it matches: no USB saving then, only the decode) and of its
//!   own bytes (checked on every hit: a torn or rotten entry is a miss).
//! - Storage: one append-only file, `loadcache.bin`, in
//!   `VICTORY_LOAD_CACHE_DIR` (default `<VICTORY_CACHE_DIR>\loadcache`).
//!   A record is a 64-byte header and the bytes. The index is rebuilt at
//!   start by walking the headers; a torn tail is cut off.
//! - Size cap `VICTORY_LOAD_CACHE_MB` (20480). Recording stops at the cap;
//!   at the next start a file over the cap is rewritten keeping the most
//!   recently used entries (hits append to `loadcache.lru`) until it is at
//!   three quarters of the cap.
//!
//! Writes go through one writer thread; a request never waits for the disk
//! on a miss (recordings beyond 512 MB in flight are dropped).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

const MAGIC: u32 = 0x3143_4c56; // "VLC1"
const HEADER: usize = 64;
const MAX_PENDING: u64 = 512 << 20;

/// What identifies a request's decoded output.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Key {
    pub file: u64,
    pub offset: u64,
    pub source_size: u32,
    pub destination_size: u32,
    pub intermediate_size: u32,
    pub codec: u32,
}

impl Key {
    fn bytes(&self) -> [u8; 32] {
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&self.file.to_le_bytes());
        b[8..16].copy_from_slice(&self.offset.to_le_bytes());
        b[16..20].copy_from_slice(&self.source_size.to_le_bytes());
        b[20..24].copy_from_slice(&self.destination_size.to_le_bytes());
        b[24..28].copy_from_slice(&self.intermediate_size.to_le_bytes());
        b[28..32].copy_from_slice(&self.codec.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8]) -> Key {
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        Key {
            file: u64_at(0),
            offset: u64_at(8),
            source_size: u32_at(16),
            destination_size: u32_at(20),
            intermediate_size: u32_at(24),
            codec: u32_at(28),
        }
    }

    fn short(&self) -> u64 {
        hash(&self.bytes())
    }
}

/// 64 bits of BLAKE3.
pub(super) fn hash(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(blake3::hash(bytes).as_bytes()[..8].try_into().unwrap())
}

/// A file's identity for keys: its path (case folded), size and time.
pub(super) fn file_ident(path: &str, file: &File) -> u64 {
    let meta = file.metadata().ok();
    let len = meta.as_ref().map_or(0, |m| m.len());
    let time = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64);
    let mut h = blake3::Hasher::new();
    h.update(path.to_lowercase().as_bytes());
    h.update(&len.to_le_bytes());
    h.update(&time.to_le_bytes());
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap())
}

struct Entry {
    /// Where the bytes start in the file.
    at: u64,
    len: u32,
    source_hash: u64,
    data_hash: u64,
    boot: u64,
    used: AtomicBool,
}

enum Msg {
    Record(Key, u64, Vec<u8>),
    Used(u64),
    Flush(Sender<()>),
}

#[derive(Default)]
pub(super) struct Counters {
    pub hits: AtomicU64,
    pub hit_bytes: AtomicU64,
    pub misses: AtomicU64,
    pub recorded: AtomicU64,
    pub recorded_bytes: AtomicU64,
    pub dropped: AtomicU64,
    pub bad: AtomicU64,
}

/// What the writer thread shares with the requests.
struct Shared {
    index: RwLock<HashMap<Key, Entry>>,
    pending: AtomicU64,
    size: AtomicU64,
    counters: Counters,
}

pub(super) struct Cache {
    shared: Arc<Shared>,
    reader: File,
    tx: Mutex<Option<Sender<Msg>>>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    cap: u64,
    pub verify: bool,
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(writer) = self.writer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = writer.join();
        }
    }
}

fn header(key: &Key, source_hash: u64, len: u32, data_hash: u64, boot: u64) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..36].copy_from_slice(&key.bytes());
    h[36..44].copy_from_slice(&source_hash.to_le_bytes());
    h[44..48].copy_from_slice(&len.to_le_bytes());
    h[48..56].copy_from_slice(&data_hash.to_le_bytes());
    h[56..64].copy_from_slice(&boot.to_le_bytes());
    h
}

/// Walks the records of `file`: the index, and where the good part ends.
fn scan(file: &mut File) -> std::io::Result<(HashMap<Key, Entry>, u64)> {
    let total = file.seek(SeekFrom::End(0))?;
    let mut index = HashMap::new();
    let mut at = 0u64;
    let mut h = [0u8; HEADER];
    while at + HEADER as u64 <= total {
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut h)?;
        if u32::from_le_bytes(h[0..4].try_into().unwrap()) != MAGIC {
            break;
        }
        let key = Key::from_bytes(&h[4..36]);
        let len = u32::from_le_bytes(h[44..48].try_into().unwrap());
        let end = at + HEADER as u64 + u64::from(len);
        if end > total {
            break;
        }
        index.entry(key).or_insert(Entry {
            at: at + HEADER as u64,
            len,
            source_hash: u64::from_le_bytes(h[36..44].try_into().unwrap()),
            data_hash: u64::from_le_bytes(h[48..56].try_into().unwrap()),
            boot: u64::from_le_bytes(h[56..64].try_into().unwrap()),
            used: AtomicBool::new(false),
        });
        at = end;
    }
    Ok((index, at))
}

/// Rewrites the data file keeping the most recently used entries up to
/// `keep` bytes.
fn compact(dir: &Path, index: HashMap<Key, Entry>, keep: u64) -> std::io::Result<()> {
    let data = dir.join("loadcache.bin");
    let tmp = dir.join("loadcache.bin.tmp");
    let mut entries: Vec<(Key, Entry)> = index.into_iter().collect();
    entries.sort_by(|a, b| b.1.boot.cmp(&a.1.boot).then(a.1.at.cmp(&b.1.at)));
    let mut from = File::open(&data)?;
    let mut to = std::io::BufWriter::with_capacity(8 << 20, File::create(&tmp)?);
    let mut written = 0u64;
    let mut buf = Vec::new();
    for (key, e) in entries {
        let n = HEADER as u64 + u64::from(e.len);
        if written + n > keep {
            continue;
        }
        buf.resize(e.len as usize, 0);
        from.seek(SeekFrom::Start(e.at))?;
        from.read_exact(&mut buf)?;
        to.write_all(&header(&key, e.source_hash, e.len, e.data_hash, e.boot))?;
        to.write_all(&buf)?;
        written += n;
    }
    to.flush()?;
    drop(to);
    drop(from);
    std::fs::rename(&tmp, &data)?;
    let _ = std::fs::remove_file(dir.join("loadcache.lru"));
    Ok(())
}

impl Cache {
    /// Opens (or makes) the cache in `dir`, compacting it first when it is
    /// over `cap` bytes. `boot` stamps this process's recordings and hits.
    pub(super) fn open(dir: &Path, cap: u64, boot: u64, verify: bool) -> std::io::Result<Arc<Cache>> {
        std::fs::create_dir_all(dir)?;
        let data = dir.join("loadcache.bin");
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&data)?;
        let (mut index, mut good) = scan(&mut file)?;
        // Last uses from earlier boots.
        if let Ok(bytes) = std::fs::read(dir.join("loadcache.lru")) {
            let used: HashMap<u64, u64> = bytes
                .chunks_exact(16)
                .map(|c| (u64::from_le_bytes(c[..8].try_into().unwrap()), u64::from_le_bytes(c[8..].try_into().unwrap())))
                .collect();
            for (key, e) in index.iter_mut() {
                if let Some(&b) = used.get(&key.short()) {
                    e.boot = e.boot.max(b);
                }
            }
        }
        if good > cap {
            drop(file);
            let before = good;
            compact(dir, index, cap / 4 * 3)?;
            file = OpenOptions::new().read(true).write(true).open(&data)?;
            (index, good) = scan(&mut file)?;
            tracing::info!(before_mb = before >> 20, after_mb = good >> 20, "dstorage: load cache over its cap, kept the most recently used");
        }
        file.set_len(good)?;
        file.seek(SeekFrom::Start(good))?;
        let lru = OpenOptions::new().append(true).create(true).open(dir.join("loadcache.lru"))?;
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            index: RwLock::new(index),
            pending: AtomicU64::new(0),
            size: AtomicU64::new(good),
            counters: Counters::default(),
        });
        let for_writer = Arc::clone(&shared);
        let writer = std::thread::Builder::new()
            .name("dstorage load cache".into())
            .spawn(move || for_writer.write(rx, file, lru, boot))?;
        Ok(Arc::new(Cache {
            shared,
            reader: File::open(&data)?,
            tx: Mutex::new(Some(tx)),
            writer: Mutex::new(Some(writer)),
            cap,
            verify,
        }))
    }

    pub(super) fn counters(&self) -> &Counters {
        &self.shared.counters
    }

    fn send(&self, msg: Msg) {
        if let Some(tx) = &*self.tx.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = tx.send(msg);
        }
    }

    pub(super) fn entries(&self) -> usize {
        self.shared.index.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub(super) fn size(&self) -> u64 {
        self.shared.size.load(Ordering::Relaxed)
    }

    pub(super) fn contains(&self, key: &Key) -> bool {
        self.shared.contains(key)
    }

    /// The recorded output for `key`, checked against its hash, and with
    /// `source` against the source it was decoded from.
    pub(super) fn get(&self, key: &Key, source: Option<&[u8]>) -> Option<Vec<u8>> {
        let (at, len, source_hash, data_hash, first) = {
            let index = self.shared.index.read().unwrap_or_else(|e| e.into_inner());
            let Some(e) = index.get(key) else {
                self.shared.counters.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            (e.at, e.len, e.source_hash, e.data_hash, !e.used.swap(true, Ordering::Relaxed))
        };
        if source.is_some_and(|s| hash(s) != source_hash) {
            self.forget(key);
            return None;
        }
        let mut bytes = vec![0u8; len as usize];
        let ok = read_at(&self.reader, &mut bytes, at).is_ok() && hash(&bytes) == data_hash;
        if !ok {
            self.forget(key);
            return None;
        }
        if first {
            self.send(Msg::Used(key.short()));
        }
        self.shared.counters.hits.fetch_add(1, Ordering::Relaxed);
        self.shared.counters.hit_bytes.fetch_add(u64::from(len), Ordering::Relaxed);
        Some(bytes)
    }

    fn forget(&self, key: &Key) {
        self.shared.counters.bad.fetch_add(1, Ordering::Relaxed);
        self.shared.counters.misses.fetch_add(1, Ordering::Relaxed);
        self.shared.index.write().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    /// Hands a decoded output to the writer (dropped when the cap is
    /// reached or too much is already waiting).
    pub(super) fn record(&self, key: Key, source_hash: u64, bytes: Vec<u8>) {
        let n = bytes.len() as u64;
        let s = &self.shared;
        let pending = s.pending.load(Ordering::Relaxed);
        if u32::try_from(n).is_err()
            || s.size.load(Ordering::Relaxed) + pending + HEADER as u64 + n > self.cap
            || pending + n > MAX_PENDING
        {
            s.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        s.pending.fetch_add(n, Ordering::Relaxed);
        self.send(Msg::Record(key, source_hash, bytes));
    }

    /// Returns once everything handed to the writer so far is written.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn flush(&self) {
        let (tx, rx) = channel();
        self.send(Msg::Flush(tx));
        let _ = rx.recv();
    }
}

impl Shared {
    fn contains(&self, key: &Key) -> bool {
        self.index.read().unwrap_or_else(|e| e.into_inner()).contains_key(key)
    }

    fn write(&self, rx: Receiver<Msg>, mut file: File, mut lru: File, boot: u64) {
        let mut failed = false;
        for msg in rx {
            match msg {
                Msg::Flush(done) => {
                    let _ = done.send(());
                }
                Msg::Used(short) => {
                    let mut b = [0u8; 16];
                    b[..8].copy_from_slice(&short.to_le_bytes());
                    b[8..].copy_from_slice(&boot.to_le_bytes());
                    let _ = lru.write_all(&b);
                }
                Msg::Record(key, source_hash, bytes) => {
                    let n = bytes.len() as u64;
                    self.pending.fetch_sub(n, Ordering::Relaxed);
                    if failed || self.contains(&key) {
                        continue;
                    }
                    let at = self.size.load(Ordering::Relaxed);
                    let data_hash = hash(&bytes);
                    let h = header(&key, source_hash, n as u32, data_hash, boot);
                    let wrote = file.write_all(&h).and_then(|()| file.write_all(&bytes));
                    if let Err(error) = wrote {
                        // A part written is cut off at the next start.
                        tracing::warn!(%error, "dstorage: the load cache stops recording");
                        failed = true;
                        continue;
                    }
                    self.size.store(at + HEADER as u64 + n, Ordering::Relaxed);
                    self.counters.recorded.fetch_add(1, Ordering::Relaxed);
                    self.counters.recorded_bytes.fetch_add(n, Ordering::Relaxed);
                    self.index.write().unwrap_or_else(|e| e.into_inner()).entry(key).or_insert(Entry {
                        at: at + HEADER as u64,
                        len: n as u32,
                        source_hash,
                        data_hash,
                        boot,
                        used: AtomicBool::new(true),
                    });
                }
            }
        }
    }
}

#[cfg(windows)]
fn read_at(file: &File, out: &mut [u8], at: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < out.len() {
        match file.seek_read(&mut out[done..], at + done as u64)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => done += n,
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn read_at(file: &File, out: &mut [u8], at: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(out, at)
}

/// `VICTORY_LOAD_CACHE=1`.
pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_LOAD_CACHE").is_ok_and(|v| v.trim() == "1"))
}

/// One line with the dstorage summary while the cache is on.
pub(super) fn log_summary() {
    let Some(cache) = get() else {
        return;
    };
    let c = cache.counters();
    let take = |a: &AtomicU64| a.swap(0, Ordering::Relaxed);
    tracing::info!(
        hits = take(&c.hits),
        hit_mb = take(&c.hit_bytes) >> 20,
        misses = take(&c.misses),
        recorded = take(&c.recorded),
        recorded_mb = take(&c.recorded_bytes) >> 20,
        dropped = take(&c.dropped),
        bad = take(&c.bad),
        entries = cache.entries(),
        mb = cache.size() >> 20,
        "dstorage: load cache"
    );
}

/// The process's cache: `Some` once `VICTORY_LOAD_CACHE=1` and it opened
/// (it opens on a thread of its own; requests before that just miss).
pub(super) fn get() -> Option<&'static Cache> {
    static CACHE: OnceLock<Option<Arc<Cache>>> = OnceLock::new();
    static READY: OnceLock<Arc<Cache>> = OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    if let Some(cache) = READY.get() {
        return Some(cache);
    }
    if !enabled() {
        return None;
    }
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("dstorage load cache open".into()).spawn(|| {
            let opened = CACHE.get_or_init(|| {
                let dir = std::env::var_os("VICTORY_LOAD_CACHE_DIR")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("VICTORY_CACHE_DIR").filter(|v| !v.is_empty()).map(|d| PathBuf::from(d).join("loadcache")))?;
                let cap = std::env::var("VICTORY_LOAD_CACHE_MB")
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(20480)
                    << 20;
                let verify = std::env::var("VICTORY_LOAD_CACHE_VERIFY").ok().as_deref().map(str::trim) == Some("1");
                let boot = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let started = std::time::Instant::now();
                match Cache::open(&dir, cap, boot, verify) {
                    Ok(cache) => {
                        tracing::info!(
                            dir = %dir.display(),
                            entries = cache.entries(),
                            mb = cache.size() >> 20,
                            cap_mb = cap >> 20,
                            verify,
                            open_ms = started.elapsed().as_millis() as u64,
                            "dstorage: load cache open (VICTORY_LOAD_CACHE)"
                        );
                        Some(cache)
                    }
                    Err(error) => {
                        tracing::warn!(dir = %dir.display(), %error, "dstorage: no load cache");
                        None
                    }
                }
            });
            if let Some(cache) = opened {
                let _ = READY.set(Arc::clone(cache));
            }
        });
    });
    READY.get().map(|c| &**c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u64) -> Key {
        Key {
            file: 7,
            offset: n * 4096,
            source_size: 100,
            destination_size: 4096,
            intermediate_size: 0,
            codec: 1,
        }
    }

    fn bytes(n: u64, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u64 * 31 + n * 7) as u8).collect()
    }

    #[test]
    fn records_and_serves_byte_identical_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 1 << 30, 1, false).unwrap();
        for n in 0..50 {
            cache.record(key(n), n, bytes(n, 1000 + n as usize * 13));
        }
        cache.flush();
        assert_eq!(cache.get(&key(3), None).unwrap(), bytes(3, 1039));
        drop(cache);
        let again = Cache::open(dir.path(), 1 << 30, 2, false).unwrap();
        assert_eq!(again.entries(), 50);
        for n in 0..50 {
            assert_eq!(again.get(&key(n), None).unwrap(), bytes(n, 1000 + n as usize * 13));
        }
        assert!(again.get(&key(99), None).is_none());
    }

    /// What a hit writes is byte for byte what a fresh decode writes.
    #[test]
    fn a_hit_is_the_fresh_decode() {
        let dir = tempfile::tempdir().unwrap();
        let plain: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 & 0x3f).collect();
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
        let fresh = |out: &mut [u8]| {
            miniz_oxide::inflate::decompress_slice_iter_to_slice(out, std::iter::once(&packed[..]), true, false).unwrap()
        };
        let k = Key {
            file: 42,
            offset: 0x1234_5000,
            source_size: packed.len() as u32,
            destination_size: plain.len() as u32 + 100,
            intermediate_size: 0,
            codec: 1,
        };
        let mut first = vec![0xccu8; plain.len() + 100];
        let n = fresh(&mut first);
        let cache = Cache::open(dir.path(), 1 << 30, 1, false).unwrap();
        cache.record(k, hash(&packed), first[..n].to_vec());
        cache.flush();
        drop(cache);
        let cache = Cache::open(dir.path(), 1 << 30, 2, true).unwrap();
        let hit = cache.get(&k, Some(&packed)).unwrap();
        let mut served = vec![0xccu8; plain.len() + 100];
        served[..hit.len()].copy_from_slice(&hit);
        assert_eq!(served, first);
        assert_eq!(&served[..plain.len()], &plain[..]);
        // Another offset or codec is another key.
        assert!(cache.get(&Key { offset: k.offset + 1, ..k }, None).is_none());
        assert!(cache.get(&Key { codec: 3, ..k }, None).is_none());
    }

    #[test]
    fn a_different_source_or_rotten_bytes_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 1 << 30, 1, true).unwrap();
        let source = b"compressed".to_vec();
        cache.record(key(1), hash(&source), bytes(1, 5000));
        cache.record(key(2), 0, bytes(2, 5000));
        cache.flush();
        assert!(cache.get(&key(1), Some(b"changed!!!")).is_none());
        assert!(!cache.contains(&key(1)));
        drop(cache);
        // Rot one byte of entry 2's data.
        let path = dir.path().join("loadcache.bin");
        let mut all = std::fs::read(&path).unwrap();
        let last = all.len() - 10;
        all[last] ^= 0xff;
        std::fs::write(&path, &all).unwrap();
        let again = Cache::open(dir.path(), 1 << 30, 2, false).unwrap();
        assert!(again.get(&key(2), None).is_none());
        assert_eq!(again.get(&key(1), Some(&source)).unwrap(), bytes(1, 5000));
    }

    #[test]
    fn a_torn_tail_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 1 << 30, 1, false).unwrap();
        cache.record(key(1), 0, bytes(1, 3000));
        cache.record(key(2), 0, bytes(2, 3000));
        cache.flush();
        drop(cache);
        let path = dir.path().join("loadcache.bin");
        let len = std::fs::metadata(&path).unwrap().len();
        OpenOptions::new().write(true).open(&path).unwrap().set_len(len - 100).unwrap();
        let again = Cache::open(dir.path(), 1 << 30, 2, false).unwrap();
        assert_eq!(again.entries(), 1);
        again.record(key(3), 0, bytes(3, 3000));
        again.flush();
        drop(again);
        let third = Cache::open(dir.path(), 1 << 30, 3, false).unwrap();
        assert_eq!(third.get(&key(1), None).unwrap(), bytes(1, 3000));
        assert_eq!(third.get(&key(3), None).unwrap(), bytes(3, 3000));
        assert!(third.get(&key(2), None).is_none());
    }

    #[test]
    fn over_the_cap_keeps_the_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        let cap = 64 * 1024;
        let cache = Cache::open(dir.path(), cap, 1, false).unwrap();
        for n in 0..14 {
            cache.record(key(n), 0, bytes(n, 4000));
        }
        cache.flush();
        drop(cache);
        // Boot 2 uses entry 0 and 1 only.
        let cache = Cache::open(dir.path(), cap, 2, false).unwrap();
        assert!(cache.get(&key(0), None).is_some());
        assert!(cache.get(&key(1), None).is_some());
        cache.flush();
        drop(cache);
        // Boot 3 with a smaller cap: compacted, the used ones kept.
        let cache = Cache::open(dir.path(), 20_000, 3, false).unwrap();
        assert!(cache.size() <= 15_000);
        assert_eq!(cache.get(&key(0), None).unwrap(), bytes(0, 4000));
        assert_eq!(cache.get(&key(1), None).unwrap(), bytes(1, 4000));
        // Recording stops at the cap.
        for n in 20..40 {
            cache.record(key(n), 0, bytes(n, 4000));
        }
        cache.flush();
        assert!(cache.size() <= 20_000);
    }
}
