//! `victory prewarm`: the first-boot prewarm (see `victory_gpu::pipeline::prewarm`).
//!
//! - `manifest`: from a warm cache and the game files it was warmed from, the list of what the
//!   runs translated and compiled, by hash only (no code, no shader bytes: it can ship).
//! - `run`: makes all of it into this machine's caches from its own game files, on N-1 cores at
//!   background priority, printing `PREWARM <phase> <done> <total>` lines for a launcher.
//!   Stopping it (killing the process) keeps what was done; the next run goes on from there.
//! - `run --check`: makes nothing; says how much of the manifest the caches hold, which is
//!   what the next boot finds there (shader cache hits, pipeline library loads).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use victory_hle::modules::d3d12x::prewarm as pw;
use victory_shader::{Container, FourCC, HwStage, native_programs};

use crate::shader::{MAX_FILE, sources};

const HEADER: &str = "# victory prewarm manifest 1";

/// What a manifest lists.
#[derive(Default)]
pub struct Manifest {
    /// Container hash (16 hex digits) -> stage.
    shaders: BTreeMap<String, String>,
    /// Title pipeline key -> how it is made offline (`c`, `g`, `r`).
    pipelines: BTreeMap<String, char>,
}

impl Manifest {
    fn read(path: &Path) -> Result<Manifest> {
        let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
        anyhow::ensure!(text.starts_with(HEADER), "{} is not a prewarm manifest", path.display());
        let mut m = Manifest::default();
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let mut w = line.split_whitespace();
            match (w.next(), w.next(), w.next()) {
                (Some("s"), Some(hash), Some(stage)) if is_hex(hash, 16) => {
                    m.shaders.insert(hash.to_owned(), stage.to_owned());
                }
                (Some("p"), Some(kind @ ("c" | "g" | "r")), Some(key)) if is_hex(key, 32) => {
                    m.pipelines.insert(key.to_owned(), kind.chars().next().unwrap_or('r'));
                }
                _ => {}
            }
        }
        Ok(m)
    }

    fn write(&self, path: &Path, note: &str) -> Result<()> {
        let mut out = format!(
            "{HEADER}\n# Hashes only: BLAKE3 prefixes of the title's shader containers (s <hash> <stage>) and of the\n\
             # title pipeline keys a warm cache's runs asked for (p <c|g|r> <key>: c compute, g a record of the\n\
             # title's pipeline cache, r made by the run only). No code or shader bytes. {note}\n"
        );
        for (hash, stage) in &self.shaders {
            out += &format!("s {hash} {stage}\n");
        }
        for (key, kind) in &self.pipelines {
            out += &format!("p {kind} {key}\n");
        }
        std::fs::write(path, out).with_context(|| path.display().to_string())
    }

    fn keys(&self, kind: char) -> HashSet<String> {
        self.pipelines.iter().filter(|(_, k)| **k == kind).map(|(key, _)| key.clone()).collect()
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One of the title's containers as the title passes it, its hash and stage.
struct Found {
    hash: String,
    stage: &'static str,
    bytes: Vec<u8>,
}

/// Every container in `paths` (files up to `MAX_FILE`), each once, handed to `each` on
/// `threads` workers at background priority. `want` picks by hash before the work.
fn each_container(paths: &[PathBuf], threads: usize, want: &(dyn Fn(&str) -> bool + Sync), each: &(dyn Fn(Found) + Sync)) {
    let seen = Mutex::new(HashSet::new());
    for root in paths {
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() || entry.metadata().map_or(0, |m| m.len()) > MAX_FILE {
                continue;
            }
            let Ok(data) = std::fs::read(entry.path()) else { continue };
            let jobs = sources(&data);
            let next = AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..threads.min(jobs.len()) {
                    s.spawn(|| {
                        pw::background();
                        while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                            let Some(bytes) = job.bytes() else { continue };
                            let Ok(container) = Container::parse(&bytes) else { continue };
                            // As the title passes it: no more than the container (`victory warm`).
                            let Some(total) = bytes.get(0x18..0x1c).and_then(|b| b.try_into().ok()) else { continue };
                            let bytes = &bytes[..(u32::from_le_bytes(total) as usize).min(bytes.len())];
                            let hash = blake3::hash(bytes).to_hex()[..16].to_owned();
                            if !want(&hash) || !seen.lock().unwrap_or_else(|e| e.into_inner()).insert(hash.clone()) {
                                continue;
                            }
                            let Some(dxil) = container.part(FourCC::DXIL).map(|p| p.data) else { continue };
                            let Ok(programs) = native_programs(dxil) else { continue };
                            let Some(stage) = programs.iter().find_map(|p| stage_name(p.stage)) else { continue };
                            each(Found { hash, stage, bytes: bytes.to_vec() });
                        }
                    });
                }
            });
        }
    }
}

fn stage_name(stage: HwStage) -> Option<&'static str> {
    match stage {
        HwStage::Vs => Some("vertex"),
        HwStage::Ps => Some("pixel"),
        HwStage::Ms => Some("mesh"),
        HwStage::Gs => Some("geometry"),
        HwStage::Cs => Some("compute"),
        _ => None,
    }
}

fn stage_static(stage: &str) -> &'static str {
    ["vertex", "pixel", "mesh", "geometry", "compute"].into_iter().find(|s| *s == stage).unwrap_or("compute")
}

/// The shader archives of `paths` (their `shaders.rpf`), for the graphics records.
fn archives(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|p| if p.is_file() { p.clone() } else { p.join("shaders.rpf") })
        .filter(|p| p.is_file() && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("shaders.rpf")))
        .collect()
}

/// Paths given, else the configured game folder's shaders.rpf (where the title's shaders are),
/// else the configured folders.
fn game_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let all = crate::default_paths(paths.clone())?;
    if !paths.is_empty() {
        return Ok(all);
    }
    let rpf = archives(&all);
    Ok(if rpf.is_empty() { all } else { rpf })
}

fn jobs_or_default(jobs: Option<usize>) -> usize {
    // N-1 cores: one is left to the rest of the machine.
    jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1)).max(1))
}

fn pct(n: usize, of: usize) -> f64 {
    if of == 0 { 100.0 } else { n as f64 * 100.0 / of as f64 }
}

/// `victory prewarm manifest`: what the configured (warm) cache holds, by hash.
pub fn manifest(paths: Vec<PathBuf>, out: &Path, jobs: Option<usize>, cand_key: bool) -> Result<()> {
    let cfg = victory_core::Config::load()?;
    if cand_key {
        pw::cand_key();
    }
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    anyhow::ensure!(pw::prepare_host(), "no host GPU: the cache keys depend on it");
    let paths = game_paths(paths)?;
    let threads = jobs_or_default(jobs);
    let began = std::time::Instant::now();
    let found = AtomicUsize::new(0);
    let shaders = Mutex::new(BTreeMap::new());
    let compute = Mutex::new(HashMap::new());
    each_container(&paths, threads, &|_| true, &|f| {
        found.fetch_add(1, Ordering::Relaxed);
        // (by runs: those before fx-prewarm keyed their compute shaders before the view heap
        // probe, see `shader_cached_by_runs`)
        if pw::shader_cached_by_runs(&f.bytes) {
            if f.stage == "compute" {
                compute.lock().unwrap().insert(pw::compute_key(&f.bytes), f.hash.clone());
            }
            shaders.lock().unwrap().insert(f.hash, f.stage.to_owned());
        }
    });
    let shaders = shaders.into_inner().unwrap();
    let compute = compute.into_inner().unwrap();
    let index = cfg.cache_subdir("precompiled")?.join("shaders.rpf.index");
    let mut graphics = HashSet::new();
    for archive in archives(&paths) {
        graphics.extend(pw::graphics_keys(&archive, Some(&index), threads).map_err(anyhow::Error::msg)?);
    }
    let early_file = cfg.cache_subdir("pipelines")?.join("early-pipelines.txt");
    let early = std::fs::read_to_string(&early_file).unwrap_or_default();
    let mut m = Manifest { shaders, ..Default::default() };
    for key in early.lines().map(str::trim).filter(|k| is_hex(k, 32)) {
        let kind = if compute.contains_key(key) { 'c' } else if graphics.contains(key) { 'g' } else { 'r' };
        m.pipelines.insert(key.to_owned(), kind);
    }
    let [c, g, r] = ['c', 'g', 'r'].map(|k| m.keys(k).len());
    m.write(out, &format!("{} shaders, {} pipelines.", m.shaders.len(), m.pipelines.len()))?;
    println!(
        "manifest     {} of {} containers in the game files are in the cache; {} pipeline keys on the early list: {c} compute, {g} graphics records, {r} run-only; {:.0} s -> {}",
        m.shaders.len(),
        found.load(Ordering::Relaxed),
        m.pipelines.len(),
        began.elapsed().as_secs_f64(),
        out.display()
    );
    Ok(())
}

/// `victory prewarm run [--check]`.
pub fn run(paths: Vec<PathBuf>, file: &Path, jobs: Option<usize>, check: bool) -> Result<()> {
    let m = Manifest::read(file)?;
    let cfg = victory_core::Config::load()?;
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    pw::background();
    let gpu = pw::prepare_host();
    if check {
        pw::load_only();
    }
    let paths = game_paths(paths)?;
    let threads = jobs_or_default(jobs);
    let began = std::time::Instant::now();
    let compute_keys = m.keys('c');
    let graphics_keys = m.keys('g');
    println!(
        "prewarm      {} shaders, {} compute and {} graphics pipelines listed; {threads} threads{}",
        m.shaders.len(),
        compute_keys.len(),
        graphics_keys.len(),
        if check { "; checking only" } else { "" }
    );

    // Shaders (and compute pipelines).
    let total = m.shaders.len();
    let [done, hit, translated, refused, c_ok, c_miss] = [const { AtomicUsize::new(0) }; 6];
    each_container(&paths, threads, &|h| m.shaders.contains_key(h), &|f| {
        let compute = f.stage == "compute" && gpu && {
            let key = pw::compute_key(&f.bytes);
            compute_keys.contains(&key)
        };
        let was = pw::shader_cached(&f.bytes);
        if was {
            hit.fetch_add(1, Ordering::Relaxed);
        }
        if !check || compute {
            let w = if check && !was {
                // a compute pipeline whose shader is not translated is not loaded either
                pw::Warmed { translated: false, new: false, pipeline: None }
            } else {
                pw::warm(stage_static(f.stage), &f.bytes, compute)
            };
            if w.new {
                translated.fetch_add(1, Ordering::Relaxed);
            }
            if !w.translated && !check {
                refused.fetch_add(1, Ordering::Relaxed);
            }
            if compute {
                (if w.pipeline == Some(true) { &c_ok } else { &c_miss }).fetch_add(1, Ordering::Relaxed);
            }
        }
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(250) || n == total {
            println!("PREWARM shaders {n} {total}");
            if !check {
                pw::save(false);
            }
        }
    });
    let found = done.load(Ordering::Relaxed);
    let hit = hit.load(Ordering::Relaxed);

    // Graphics records.
    let mut g_report = None;
    if gpu && !graphics_keys.is_empty() {
        let index = cfg.cache_subdir("precompiled")?.join("shaders.rpf.index");
        let g_total = graphics_keys.len();
        for archive in archives(&paths) {
            let r = pw::graphics(&archive, Some(&index), threads, &graphics_keys, &|_, r| {
                println!("PREWARM pipelines {} {g_total}", (r.made + r.failed + r.left_out).min(g_total));
                if !check {
                    pw::save(false);
                }
            })
            .map_err(anyhow::Error::msg)?;
            g_report = Some(r);
        }
        println!("PREWARM pipelines {g_total} {g_total}");
    }
    let (made, loaded) = pw::pipeline_counts();

    // The early list: every listed pipeline starts when the title creates it.
    let run_only = m.pipelines.len() - compute_keys.len() - graphics_keys.len();
    let early_before = m.pipelines.keys().filter(|k| pw::early_listed(k)).count();
    if !check {
        pw::list_early(m.pipelines.keys().map(String::as_str));
        pw::save(true);
    }

    let g_ok = g_report.map_or(0, |r| r.made);
    let g_miss = graphics_keys.len().saturating_sub(g_ok);
    let (c_ok, c_miss) = (c_ok.into_inner(), c_miss.into_inner());
    println!(
        "prewarm      shaders: {total} listed, {found} found in the game files; {hit} were in the cache, {} translated now, {} do not translate",
        translated.into_inner(),
        refused.into_inner()
    );
    println!(
        "prewarm      pipelines: compute {c_ok} ready, {c_miss} not; graphics records {g_ok} ready, {g_miss} not; {run_only} run-only keys; driver compiled {made}, library loaded {loaded}; {early_before} of {} keys were on the early list{}",
        m.pipelines.len(),
        if gpu { "" } else { " (no host GPU: pipelines skipped)" }
    );
    if check {
        let p_ok = c_ok + g_ok;
        let p_all = compute_keys.len() + graphics_keys.len();
        println!(
            "next boot    shader cache hits {hit}/{total} ({:.1}%); pipeline library loads {p_ok}/{p_all} ({:.1}%), compiled now {made}; early list {early_before}/{} ({:.1}%)",
            pct(hit, total),
            pct(p_ok, p_all),
            m.pipelines.len(),
            pct(early_before, m.pipelines.len())
        );
    }
    println!("prewarm      {:.0} s", began.elapsed().as_secs_f64());
    Ok(())
}
