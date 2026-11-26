//! `VICTORY_SHADER_OPT` bisection over real programs (ignored; run by hand):
//! every container in `VICTORY_OPT_BISECT_DIR` (`<hash>.dxbc` files) is
//! translated with no pass and with each pass bit alone (and every pass), and
//! both modules run on the CPU (`bitcode::interp`, one lane) against the same
//! made-up world: inputs, constant buffers and resource reads give numbers
//! made from their arguments (reads also from how many stores came before),
//! wave operations from theirs and the discards before. What each run
//! stores, outputs and discards, in order, must be the same.
//! Writes `VICTORY_OPT_BISECT_OUT` (a line per program and bit).

use std::io::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::bitcode::{interp, opt};

fn mix(h: u64, x: u64) -> u64 {
    let mut z = (h ^ x).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn hash_call(seed: u64, name: &str, args: &[Vec<u64>], extra: u64) -> u64 {
    let mut h = mix(seed, extra);
    for b in name.bytes() {
        h = mix(h, u64::from(b));
    }
    for a in args {
        h = mix(h, 0xA5);
        for &x in a {
            h = mix(h, x);
        }
    }
    h
}

/// The scalars of a made-up result: floats for `.f32`/`.f16`, else integers, some small.
fn made_up(name: &str, h: u64) -> Vec<u64> {
    let float = name.ends_with(".f32") || name.ends_with(".f16") || name.ends_with(".f64");
    let bits = if name.ends_with(".i1") { 1 } else if name.ends_with(".i16") || name.ends_with(".f16") { 16 } else { 32 };
    (0..8u64)
        .map(|k| {
            let r = mix(h, k);
            if float && bits == 32 {
                let f: f32 = match r % 8 {
                    0 => 0.0,
                    1 => 1.0,
                    2 => -1.0,
                    3 => 0.5,
                    _ => ((r >> 8) % 2001) as f32 / 100.0 - 10.0,
                };
                u64::from(f.to_bits())
            } else if bits == 1 {
                r & 1
            } else if r.is_multiple_of(4) {
                (r >> 8) % 16
            } else {
                r & if bits == 16 { 0xFFFF } else { 0xFFFF_FFFF }
            }
        })
        .collect()
}

/// What a run of `m` does under `seed`: its writes in order, or how it failed.
fn trace(m: &super::bitcode::Module, seed: u64) -> Result<Vec<String>, String> {
    trace_observed(m, seed, &mut |_, _| {})
}

fn trace_observed(m: &super::bitcode::Module, seed: u64, seen: &mut dyn FnMut(u32, &[u64])) -> Result<Vec<String>, String> {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out = Vec::new();
        let (mut stores, mut discards) = (0u64, 0u64);
        interp::run_observed(m, &mut |name, a| {
            let fmt = |a: &[Vec<u64>]| format!("{name} {a:x?}");
            if name.starts_with("bufferStore")
                || name.starts_with("rawBufferStore")
                || name.starts_with("textureStore")
                || name.starts_with("atomicBinOp")
                || name.starts_with("atomicCompareExchange")
                || name == "barrier"
            {
                out.push(fmt(a));
                stores += 1;
                made_up(name, hash_call(seed, name, a, stores))
            } else if name.starts_with("store")
                || name.starts_with("emit")
                || name.starts_with("cut")
                || name.starts_with("setMeshOutputCounts")
            {
                out.push(fmt(a));
                vec![]
            } else if name == "discard" {
                out.push(fmt(a));
                discards += 1;
                vec![]
            } else if name.starts_with("wave") && name != "waveGetLaneIndex" && name != "waveGetLaneCount"
                || name.starts_with("quad")
                || name.starts_with("isHelperLane")
            {
                made_up(name, hash_call(seed, name, a, 1 << 40 | discards))
            } else if name.starts_with("bufferLoad")
                || name.starts_with("rawBufferLoad")
                || name.starts_with("textureLoad")
                || name.starts_with("sample")
                || name.starts_with("textureGather")
                || name.starts_with("calculateLOD")
            {
                made_up(name, hash_call(seed, name, a, 2 << 40 | stores))
            } else if name == "binary.i32" && (37..=40).contains(&a[0][0]) {
                let (x, y) = (a[1][0] as u32, a[2][0] as u32);
                let r = match a[0][0] {
                    37 => (x as i32).max(y as i32) as u32,
                    38 => (x as i32).min(y as i32) as u32,
                    39 => x.max(y),
                    _ => x.min(y),
                };
                vec![u64::from(r)]
            } else {
                made_up(name, hash_call(seed, name, a, 3 << 40))
            }
        }, seen);
        out
    }));
    r.map_err(|e| {
        e.downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
            .chars()
            .take(60)
            .collect()
    })
}

thread_local! {
    static REST: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn rest() -> String {
    REST.with(|r| r.borrow().clone())
}

fn translate_with(bytes: &[u8], bits: u32) -> Option<(String, Vec<u8>, super::bitcode::Module)> {
    let _ = interp::last();
    let was = super::TEST_SHADER_OPT.replace(Some(bits));
    let r = std::panic::catch_unwind(|| super::translate(bytes));
    super::TEST_SHADER_OPT.set(was);
    let mut s = r.ok()?.ok()?;
    let m = interp::last()?;
    // What besides the code differs: the container's other parts, the rest of the shader.
    let mut rest = String::new();
    if let Ok(c) = crate::Container::parse(&s.dxil) {
        for p in &c.parts {
            // (the feature flags follow what the passes left: `Usage`)
            if p.fourcc != crate::FourCC::DXIL && p.fourcc != crate::FourCC(*b"HASH") && p.fourcc != crate::FourCC(*b"SFI0") {
                rest += &format!("{:?}{:x?}", p.fourcc, p.data);
            }
        }
    }
    let dxil = std::mem::take(&mut s.dxil);
    rest += &format!("{s:?}");
    REST.with(|r| *r.borrow_mut() = rest);
    Some((format!("{:?}", s.stage), dxil, m))
}

/// The host facts a run's translations are made with, on this thread:
/// `VICTORY_OPT_BISECT_WAVE` (the host's widest wave) and
/// `VICTORY_OPT_BISECT_HEAP` (`limit[,window,slot]`: the view heap's null
/// slot and its heap map).
fn host_like() {
    if let Some(w) = std::env::var("VICTORY_OPT_BISECT_WAVE").ok().and_then(|s| s.parse().ok()) {
        super::lower::TEST_HOST_WAVE.set(Some(w));
    }
    if let Ok(h) = std::env::var("VICTORY_OPT_BISECT_HEAP") {
        let n: Vec<u32> = h.split(',').filter_map(|s| s.parse().ok()).collect();
        let map = (n.len() == 3).then(|| (n[1], n[2]));
        if let Some(&limit) = n.first() {
            super::shader::TEST_VIEW_HEAP.set(Some((limit, map)));
        }
    }
}

#[test]
#[ignore]
fn shader_opt_bisect_over_real_programs() {
    let dir = std::env::var("VICTORY_OPT_BISECT_DIR").expect("VICTORY_OPT_BISECT_DIR");
    let outp = std::env::var("VICTORY_OPT_BISECT_OUT").expect("VICTORY_OPT_BISECT_OUT");
    let only: Vec<String> = std::env::var("VICTORY_OPT_BISECT_STAGES")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let threads: usize = std::env::var("VICTORY_OPT_BISECT_THREADS").ok().and_then(|s| s.parse().ok()).unwrap_or(16);
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    let mut bits: Vec<u32> = (0..11).map(|k| 1u32 << k).collect();
    bits.push(opt::ALL);
    let next = AtomicUsize::new(0);
    let out = Mutex::new(std::fs::File::create(&outp).unwrap());
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                host_like();
                while let Some(path) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let name = path.file_stem().unwrap().to_string_lossy().to_string();
                    let bytes = std::fs::read(path).unwrap();
                    let mut lines = Vec::new();
                    let Some((stage, dxil0, m0)) = translate_with(&bytes, 0) else {
                        continue;
                    };
                    let rest0 = rest();
                    if !only.is_empty() && !only.iter().any(|o| stage.contains(o.as_str())) {
                        continue;
                    }
                    let seeds = [1u64, 2, 3, 4];
                    let base: Vec<_> = seeds.iter().map(|&s| trace(&m0, s)).collect();
                    let usable = base.iter().any(|t| t.is_ok());
                    for &b in &bits {
                        let t = translate_with(&bytes, b);
                        let rest_same = rest() == rest0;
                        let r = match t {
                            None => "TRANSLATE-ERR".to_string(),
                            Some((_, d, _)) if d == dxil0 => "same-dxil".to_string(),
                            Some((_, _, m)) if !usable => {
                                let _ = m;
                                format!("changed-unrun({})", base[0].as_ref().err().cloned().unwrap_or_default())
                            }
                            Some((_, _, m)) => {
                                let mut verdict = "same-trace".to_string();
                                for (k, &s) in seeds.iter().enumerate() {
                                    let t = trace(&m, s);
                                    if t != base[k] {
                                        let first = match (&base[k], &t) {
                                            (Ok(x), Ok(y)) => {
                                                let i = x.iter().zip(y).take_while(|(a, b)| a == b).count();
                                                format!(
                                                    "at {i}/{}|{}: want {:?} got {:?}",
                                                    x.len(),
                                                    y.len(),
                                                    x.get(i).map(|s| s.chars().take(200).collect::<String>()),
                                                    y.get(i).map(|s| s.chars().take(200).collect::<String>())
                                                )
                                            }
                                            (x, y) => format!("want {:?} got {:?}", x.as_ref().map(|v| v.len()), y.as_ref().map(|v| v.len())),
                                        };
                                        verdict = format!("DIFF seed {s} {first}");
                                        break;
                                    }
                                }
                                verdict
                            }
                        };
                        let r = if rest_same { r } else { format!("REST {r}") };
                        lines.push(format!("{name}\t{stage}\t{b}\t{r}"));
                    }
                    let mut f = out.lock().unwrap();
                    for l in lines {
                        let _ = writeln!(f, "{l}");
                    }
                }
            });
        }
    });
}

/// For `VICTORY_OPT_BISECT_ONE=<file>` and `VICTORY_OPT_BISECT_BITS`, seed
/// `VICTORY_OPT_BISECT_SEED`: the first instruction (in the optimised run's
/// order) whose value differs from the same instruction's in the plain run,
/// with both modules' text around it.
#[test]
#[ignore]
fn shader_opt_bisect_one() {
    let path = std::env::var("VICTORY_OPT_BISECT_ONE").expect("file");
    let bits: u32 = std::env::var("VICTORY_OPT_BISECT_BITS").unwrap().parse().unwrap();
    let seed: u64 = std::env::var("VICTORY_OPT_BISECT_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let bytes = std::fs::read(path).unwrap();
    host_like();
    let (_, _, m0) = translate_with(&bytes, 0).unwrap();
    let rest0 = rest();
    let (_, _, m1) = translate_with(&bytes, bits).unwrap();
    let rest1 = rest();
    if rest0 != rest1 {
        let at = rest0.bytes().zip(rest1.bytes()).take_while(|(a, b)| a == b).count();
        let from = at.saturating_sub(300);
        println!("rest differs at {at}:
 plain: {}
 opt:   {}", &rest0[from..(at + 300).min(rest0.len())], &rest1[from..(at + 300).min(rest1.len())]);
    }
    let mut v0 = std::collections::HashMap::new();
    let _ = trace_observed(&m0, seed, &mut |i, r| {
        v0.entry(i).or_insert_with(|| r.to_vec());
    });
    let mut first = None;
    let _ = trace_observed(&m1, seed, &mut |i, r| {
        if first.is_none() && v0.get(&i).is_some_and(|x| x.as_slice() != r) {
            first = Some((i, v0[&i].clone(), r.to_vec()));
        }
    });
    println!("first differing: {first:x?}");
    if let Some((i, ..)) = first {
        println!("--- plain:
{}", m0.describe_around(i, 12));
        println!("--- optimised:
{}", m1.describe_around(i, 12));
    }
}
