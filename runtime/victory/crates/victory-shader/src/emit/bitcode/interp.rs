//! A CPU interpreter for lowered modules (tests only): runs `main` of a
//! [`Module`] the lowering built, one lane, with every `dx.op` call answered
//! by the test (resources, inputs) so that what a translated program
//! computes can be compared with what the console program means.
//! Grown from the optimiser's interpreter of random programs (`opt.rs`),
//! with aggregates (`extractvalue` of `dx.op` results, constant structs).

use super::{Bin, Cast, Const, Module, Op, Term, TypeDef, V, pred};
use std::cell::RefCell;

thread_local! {
    static LAST: RefCell<Option<Module>> = const { RefCell::new(None) };
}

/// Keeps the module a lowering finished on this thread (`Shader::finish`).
pub(crate) fn keep(m: Module) {
    LAST.with(|l| *l.borrow_mut() = Some(m));
}

/// The module the last lowering on this thread finished.
pub(crate) fn last() -> Option<Module> {
    LAST.with(|l| l.borrow_mut().take())
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

fn signed(bits: u32, x: u64) -> i64 {
    let s = 64 - bits.min(64);
    ((x << s) as i64) >> s
}

impl Module {
    /// Bits of an integer type; None for floats and the rest.
    fn bits_of(&self, ty: super::Ty) -> Option<u32> {
        match self.types[ty.0 as usize] {
            TypeDef::Int(b) => Some(b),
            _ => None,
        }
    }

    fn const_value(&self, c: u32) -> Vec<u64> {
        let (ty, k) = &self.consts[c as usize];
        match k {
            Const::Int(x) => vec![*x as u64 & self.bits_of(*ty).map_or(u64::MAX, mask)],
            Const::Float(bits) => vec![u64::from(*bits)],
            Const::Undef => vec![0; self.width(*ty)],
            Const::Aggregate(fields) => fields.iter().flat_map(|&f| self.const_value(f)).collect(),
        }
    }

    /// Scalars in a value of type `ty` (structs flattened).
    fn width(&self, ty: super::Ty) -> usize {
        match &self.types[ty.0 as usize] {
            TypeDef::Struct(_, fields) => fields.iter().map(|&f| self.width(f)).sum(),
            TypeDef::Array(n, of) => *n as usize * self.width(*of),
            _ => 1,
        }
    }
}

/// Runs `main` of `m` (one lane). `call` answers each `dx.op` call: its
/// name without `dx.op.` and its arguments (the opcode first; each a list
/// of scalars, aggregates flattened); it returns the result's scalars (none
/// for void). Integers are kept masked to their width, floats as f32 bits.
/// Panics on an instruction it does not know or after `steps` blocks.
pub(crate) fn run(m: &Module, call: &mut dyn FnMut(&str, &[Vec<u64>]) -> Vec<u64>) {
    run_observed(m, call, &mut |_, _| {});
}

/// [`run`], with `seen` told each instruction's value as it is made.
pub(crate) fn run_observed(
    m: &Module,
    call: &mut dyn FnMut(&str, &[Vec<u64>]) -> Vec<u64>,
    seen: &mut dyn FnMut(u32, &[u64]),
) {
    let mut vals: Vec<Option<Vec<u64>>> = vec![None; m.insts.len()];
    // Shared and private memory, by pointer (a pointer is [PTR, global, index...]); a place
    // never stored reads a number made from the pointer.
    let mut memory: std::collections::HashMap<Vec<u64>, u64> = std::collections::HashMap::new();
    const PTR: u64 = 0x5054_5200_0000_0000;
    let eval = |vals: &[Option<Vec<u64>>], v: V| -> Vec<u64> {
        match v {
            V::Inst(i) => vals[i as usize].clone().expect("a value defined before its use"),
            V::Const(c) => m.const_value(c),
            V::Global(g) => vec![PTR, u64::from(g)],
            _ => panic!("a function as a value"),
        }
    };
    let read = |memory: &std::collections::HashMap<Vec<u64>, u64>, p: &Vec<u64>| -> u64 {
        if let Some(&x) = memory.get(p) {
            return x;
        }
        // A constant table: its initialiser's element.
        if p.len() >= 2
            && let Some(init) = m.globals.get(p[1] as usize).and_then(|g| g.init)
        {
            let flat = m.const_value(init);
            let at = p.get(3).copied().unwrap_or(0) as usize;
            return flat.get(at).copied().unwrap_or(0);
        }
        p.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, &x| (h ^ x).wrapping_mul(0x100_0000_01B3)) & 0xFFFF_FFFF
    };
    let one = |vals: &[Option<Vec<u64>>], v: V| eval(vals, v)[0];
    let (mut b, mut from) = (0usize, usize::MAX);
    for _ in 0..100_000 {
        let blk = &m.blocks[b];
        let joined: Vec<(u32, Vec<u64>)> = blk
            .phis
            .iter()
            .map(|&p| {
                let Op::Phi(incoming) = &m.insts[p as usize].op else {
                    panic!("a join")
                };
                let v = incoming.iter().find(|x| x.1 == from).expect("a value for the edge taken");
                (p, eval(&vals, v.0))
            })
            .collect();
        for (p, x) in joined {
            vals[p as usize] = Some(x);
        }
        for &i in &blk.body {
            let inst = &m.insts[i as usize];
            let bits = m.bits_of(inst.ty);
            let float = |x: u64| f32::from_bits(x as u32);
            let r: Vec<u64> = match &inst.op {
                Op::Bin(op, x, y) => {
                    let (x, y) = (one(&vals, *x), one(&vals, *y));
                    vec![match bits {
                        Some(bits) => {
                            let r = match op {
                                Bin::Add => x.wrapping_add(y),
                                Bin::Sub => x.wrapping_sub(y),
                                Bin::Mul => x.wrapping_mul(y),
                                Bin::UDiv => x.checked_div(y).unwrap_or(0),
                                Bin::URem => x.checked_rem(y).unwrap_or(0),
                                Bin::SDiv => {
                                    signed(bits, x).checked_div(signed(bits, y)).unwrap_or(0) as u64
                                }
                                Bin::And => x & y,
                                Bin::Or => x | y,
                                Bin::Xor => x ^ y,
                                Bin::Shl => x.checked_shl(y as u32).unwrap_or(0),
                                Bin::LShr => x.checked_shr(y as u32).unwrap_or(0),
                                Bin::AShr => (signed(bits, x) >> y.min(63)) as u64,
                            };
                            r & mask(bits)
                        }
                        None => {
                            let (x, y) = (float(x), float(y));
                            let r = match op {
                                Bin::Add => x + y,
                                Bin::Sub => x - y,
                                Bin::Mul => x * y,
                                Bin::SDiv | Bin::UDiv => x / y,
                                _ => panic!("a float operation {op:?}"),
                            };
                            u64::from(r.to_bits())
                        }
                    }]
                }
                Op::Cmp(p, x, y) => {
                    let wide = m.bits_of(m.type_of(*x));
                    let (x, y) = (one(&vals, *x), one(&vals, *y));
                    let holds = match wide {
                        Some(bits) => {
                            let (sx, sy) = (signed(bits, x), signed(bits, y));
                            match *p {
                                pred::EQ => x == y,
                                pred::NE => x != y,
                                pred::IUGT => x > y,
                                pred::IUGE => x >= y,
                                pred::IULT => x < y,
                                pred::IULE => x <= y,
                                pred::SGT => sx > sy,
                                pred::SGE => sx >= sy,
                                pred::SLT => sx < sy,
                                _ => sx <= sy,
                            }
                        }
                        None => {
                            let (x, y) = (float(x), float(y));
                            if x.is_nan() || y.is_nan() {
                                p & 8 != 0
                            } else {
                                (p & 1 != 0 && x == y) || (p & 2 != 0 && x > y) || (p & 4 != 0 && x < y)
                            }
                        }
                    };
                    vec![u64::from(holds)]
                }
                Op::Select(c, t, f) => {
                    if one(&vals, *c) != 0 {
                        eval(&vals, *t)
                    } else {
                        eval(&vals, *f)
                    }
                }
                Op::Cast(op, x) => {
                    let from_bits = m.bits_of(m.type_of(*x));
                    let x = one(&vals, *x);
                    vec![match (op, from_bits, bits) {
                        (Cast::Bitcast, _, _) | (Cast::ZExt, _, _) => x,
                        (Cast::SExt, Some(f), Some(t)) => signed(f, x) as u64 & mask(t),
                        (Cast::Trunc, _, Some(t)) => x & mask(t),
                        (Cast::UiToFp, _, None) => u64::from((x as f32).to_bits()),
                        (Cast::SiToFp, Some(f), None) => u64::from((signed(f, x) as f32).to_bits()),
                        (Cast::FpToUi, _, Some(t)) => float(x) as u64 & mask(t),
                        (Cast::FpToSi, _, Some(t)) => float(x) as i64 as u64 & mask(t),
                        _ => panic!("a cast {op:?}"),
                    }]
                }
                Op::Extract(agg, k) => {
                    // Fields of the structs the lowering extracts from are
                    // scalars: field k is scalar k.
                    vec![eval(&vals, *agg)[*k as usize]]
                }
                Op::Call(f, args) => {
                    let a: Vec<Vec<u64>> = args.iter().map(|&v| eval(&vals, v)).collect();
                    let name = m.funcs[*f as usize].name.as_str();
                    let name = name.strip_prefix("dx.op.").unwrap_or(name);
                    let mut r = call(name, &a);
                    // An answer longer than the result (a test's made-up numbers): its scalars.
                    r.truncate(m.width(inst.ty));
                    r
                }
                Op::Gep(_, p, idx) => {
                    let mut ptr = eval(&vals, *p);
                    assert_eq!(ptr[0], PTR, "a pointer");
                    if ptr.len() > 2 {
                        // From an element: its first index adds to the last one.
                        let first = one(&vals, idx[0]);
                        let last = ptr.len() - 1;
                        ptr[last] = ptr[last].wrapping_add(first) & 0xFFFF_FFFF;
                        ptr.extend(idx[1..].iter().map(|&v| one(&vals, v)));
                    } else {
                        ptr.extend(idx.iter().map(|&v| one(&vals, v)));
                    }
                    ptr
                }
                Op::Load(p) => {
                    let p = eval(&vals, *p);
                    vec![read(&memory, &p)]
                }
                Op::Store(p, v) => {
                    let p = eval(&vals, *p);
                    let v = one(&vals, *v);
                    memory.insert(p, v);
                    vec![]
                }
                Op::AtomicRmw(op, p, v) => {
                    let p = eval(&vals, *p);
                    let v = one(&vals, *v);
                    let old = read(&memory, &p);
                    let new = (old.wrapping_mul(31) ^ v.wrapping_add(*op as u64)) & 0xFFFF_FFFF;
                    memory.insert(p, new);
                    vec![old]
                }
                Op::CmpXchg(p, e, n) => {
                    let p = eval(&vals, *p);
                    let (e, n) = (one(&vals, *e), one(&vals, *n));
                    let old = read(&memory, &p);
                    if old == e {
                        memory.insert(p, n);
                    }
                    vec![old, u64::from(old == e)]
                }
                op => panic!("an instruction the interpreter does not run: {op:?}"),
            };
            seen(i, &r);
            vals[i as usize] = Some(r);
        }
        from = b;
        b = match blk.term.expect("a terminator") {
            Term::Br(t) => t,
            Term::CondBr(c, t, f) => {
                if one(&vals, c) != 0 {
                    t
                } else {
                    f
                }
            }
            Term::Ret => return,
        };
    }
    panic!("no end after 100,000 blocks");
}

impl Module {
    /// Instruction `i` and what it is made of, `depth` deep (tests only).
    pub(crate) fn describe_around(&self, i: u32, depth: u32) -> String {
        let mut out = String::new();
        let mut seen = std::collections::HashSet::new();
        self.describe_into(V::Inst(i), depth, 0, &mut out, &mut seen);
        out
    }

    fn describe_into(&self, v: V, depth: u32, indent: usize, out: &mut String, seen: &mut std::collections::HashSet<u32>) {
        use std::fmt::Write as _;
        let pad = "  ".repeat(indent);
        match v {
            V::Const(c) => {
                let _ = writeln!(out, "{pad}k{c} = {:?} : {:?}", self.consts[c as usize].1, self.types[self.consts[c as usize].0.0 as usize]);
            }
            V::Inst(i) => {
                let inst = &self.insts[i as usize];
                let op = match &inst.op {
                    Op::Call(f, a) => format!("call {} {a:?}", self.funcs[*f as usize].name),
                    op => format!("{op:?}"),
                };
                let _ = writeln!(out, "{pad}%{i} = {op} : {:?}", self.types[inst.ty.0 as usize]);
                if depth == 0 || !seen.insert(i) {
                    return;
                }
                if matches!(inst.op, Op::Phi(_)) {
                    return;
                }
                super::for_each_operand(&inst.op, |w| self.describe_into(w, depth - 1, indent + 1, out, seen));
            }
            other => {
                let _ = writeln!(out, "{pad}{other:?}");
            }
        }
    }
}
