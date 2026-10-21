//! PDF functions (ISO 32000 §7.10): sampled (type 0), exponential (2), stitching (3) and
//! PostScript calculator (4), for shadings and tint transforms. Every count, size, nesting depth
//! and program length is capped; a malformed function loads as `None`.

use crate::pdf_markup::{Dict, Obj, Pdf};

const MAX_SAMPLES: usize = 2_000_000;
const MAX_PROGRAM: usize = 4096;
const MAX_NEST: usize = 16;
const MAX_OUT: usize = 32;
const STACK: usize = 100;

pub(super) enum Func {
    Sampled {
        domain: Vec<f64>,
        range: Vec<f64>,
        size: Vec<usize>,
        encode: Vec<f64>,
        decode: Vec<f64>,
        n_out: usize,
        samples: Vec<f32>,
    },
    Exp {
        domain: [f64; 2],
        c0: Vec<f64>,
        c1: Vec<f64>,
        n: f64,
    },
    Stitch {
        domain: [f64; 2],
        funcs: Vec<Func>,
        bounds: Vec<f64>,
        encode: Vec<f64>,
    },
    Calc {
        domain: Vec<f64>,
        range: Vec<f64>,
        prog: Vec<Ins>,
    },
    /// An array of one-output functions sharing their input.
    Multi(Vec<Func>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Opc {
    Abs,
    Add,
    Atan,
    Ceiling,
    Cos,
    Cvi,
    Cvr,
    Div,
    Exp,
    Floor,
    Idiv,
    Ln,
    Log,
    Mod,
    Mul,
    Neg,
    Round,
    Sin,
    Sqrt,
    Sub,
    Truncate,
    And,
    Bitshift,
    Eq,
    Ge,
    Gt,
    Le,
    Lt,
    Ne,
    Not,
    Or,
    Xor,
    Copy,
    Dup,
    Exch,
    Index,
    Pop,
    Roll,
}

pub(super) enum Ins {
    Num(f64),
    Bool(bool),
    Op(Opc),
    If(Vec<Ins>),
    IfElse(Vec<Ins>, Vec<Ins>),
}

fn nums(pdf: &mut Pdf, d: &Dict, k: &str) -> Option<Vec<f64>> {
    pdf.nums_of(d, k)
}

impl Func {
    /// Loads the function `o` (a dictionary, a stream or an array of functions).
    pub(super) fn load(pdf: &mut Pdf, o: &Obj, depth: usize) -> Option<Func> {
        if depth > MAX_NEST {
            return None;
        }
        match pdf.resolve(o) {
            Obj::Arr(v) => {
                if v.is_empty() || v.len() > MAX_OUT {
                    return None;
                }
                v.iter().map(|f| Func::load(pdf, f, depth + 1)).collect::<Option<Vec<_>>>().map(Func::Multi)
            }
            Obj::Dict(d) => Func::from_dict(pdf, &d, None, depth),
            Obj::Stream(d, a, b, id) => {
                let data = pdf.decode(&d, a, b, id)?;
                Func::from_dict(pdf, &d, Some(data), depth)
            }
            _ => None,
        }
    }

    fn from_dict(pdf: &mut Pdf, d: &Dict, data: Option<Vec<u8>>, depth: usize) -> Option<Func> {
        let domain = nums(pdf, d, "Domain")?;
        if domain.len() < 2 || domain.len() % 2 != 0 || domain.len() > 8 {
            return None;
        }
        let range = nums(pdf, d, "Range").unwrap_or_default();
        match pdf.num_of(d, "FunctionType")? as i64 {
            0 => {
                let data = data?;
                let m = domain.len() / 2;
                let n_out = range.len() / 2;
                if m > 4 || n_out == 0 || n_out > MAX_OUT {
                    return None;
                }
                let size: Vec<usize> = nums(pdf, d, "Size")?.iter().map(|s| *s as usize).collect();
                if size.len() != m || size.contains(&0) {
                    return None;
                }
                let total = size.iter().try_fold(n_out, |a, s| a.checked_mul(*s)).filter(|t| *t <= MAX_SAMPLES)?;
                let bps = pdf.num_of(d, "BitsPerSample")? as usize;
                if !matches!(bps, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32) {
                    return None;
                }
                let encode =
                    nums(pdf, d, "Encode").filter(|e| e.len() == 2 * m).unwrap_or_else(|| size.iter().flat_map(|s| [0.0, (*s - 1) as f64]).collect());
                let decode = nums(pdf, d, "Decode").filter(|e| e.len() == range.len()).unwrap_or_else(|| range.clone());
                let max = ((1u64 << bps.min(32)) - 1) as f64;
                let mut samples = Vec::with_capacity(total);
                let mut bit = 0usize;
                for _ in 0..total {
                    let mut v = 0u64;
                    for _ in 0..bps {
                        let byte = data.get(bit / 8).copied().unwrap_or(0);
                        v = (v << 1) | u64::from((byte >> (7 - bit % 8)) & 1);
                        bit += 1;
                    }
                    samples.push((v as f64 / max) as f32);
                }
                Some(Func::Sampled { domain, range, size, encode, decode, n_out, samples })
            }
            2 => {
                let c0 = nums(pdf, d, "C0").unwrap_or_else(|| vec![0.0]);
                let c1 = nums(pdf, d, "C1").unwrap_or_else(|| vec![1.0]);
                if c0.len() != c1.len() || c0.is_empty() || c0.len() > MAX_OUT {
                    return None;
                }
                let n = pdf.num_of(d, "N")?;
                Some(Func::Exp { domain: [*domain.first()?, *domain.get(1)?], c0, c1, n })
            }
            3 => {
                let Obj::Arr(fs) = pdf.val(d, "Functions") else { return None };
                if fs.is_empty() || fs.len() > 1024 {
                    return None;
                }
                let funcs = fs.iter().map(|f| Func::load(pdf, f, depth + 1)).collect::<Option<Vec<_>>>()?;
                let bounds = nums(pdf, d, "Bounds").unwrap_or_default();
                let encode = nums(pdf, d, "Encode")?;
                if bounds.len() + 1 != funcs.len() || encode.len() < 2 * funcs.len() {
                    return None;
                }
                Some(Func::Stitch { domain: [*domain.first()?, *domain.get(1)?], funcs, bounds, encode })
            }
            4 => {
                let data = data?;
                if range.is_empty() || range.len() > 2 * MAX_OUT {
                    return None;
                }
                let prog = parse_program(&data)?;
                Some(Func::Calc { domain, range, prog })
            }
            _ => None,
        }
    }

    /// The function's value at `x` (inputs clipped to the domain, outputs to the range).
    pub(super) fn eval(&self, x: &[f64]) -> Vec<f64> {
        match self {
            Func::Multi(fs) => fs.iter().map(|f| f.eval(x).first().copied().unwrap_or(0.0)).collect(),
            Func::Exp { domain, c0, c1, n } => {
                let t = x.first().copied().unwrap_or(0.0).clamp(domain[0].min(domain[1]), domain[0].max(domain[1]));
                let p = if *n == 1.0 { t } else { t.powf(*n) };
                let p = if p.is_finite() { p } else { 0.0 };
                c0.iter().zip(c1).map(|(a, b)| a + p * (b - a)).collect()
            }
            Func::Stitch { domain, funcs, bounds, encode } => {
                let (lo, hi) = (domain[0].min(domain[1]), domain[0].max(domain[1]));
                let t = x.first().copied().unwrap_or(0.0).clamp(lo, hi);
                let k = bounds.iter().position(|b| t < *b).unwrap_or(bounds.len()).min(funcs.len().saturating_sub(1));
                let a = if k == 0 { lo } else { bounds.get(k - 1).copied().unwrap_or(lo) };
                let b = if k >= bounds.len() { hi } else { bounds.get(k).copied().unwrap_or(hi) };
                let (e0, e1) = (encode.get(2 * k).copied().unwrap_or(0.0), encode.get(2 * k + 1).copied().unwrap_or(1.0));
                let u = if (b - a).abs() < 1e-12 { e0 } else { e0 + (t - a) * (e1 - e0) / (b - a) };
                funcs.get(k).map_or_else(Vec::new, |f| f.eval(&[u]))
            }
            Func::Sampled { domain, range, size, encode, decode, n_out, samples } => {
                let m = size.len();
                // Position in the sample grid of every input.
                let mut pos = Vec::with_capacity(m);
                for i in 0..m {
                    let (d0, d1) = (domain.get(2 * i).copied().unwrap_or(0.0), domain.get(2 * i + 1).copied().unwrap_or(1.0));
                    let v = x.get(i).copied().unwrap_or(0.0).clamp(d0.min(d1), d0.max(d1));
                    let (e0, e1) = (encode.get(2 * i).copied().unwrap_or(0.0), encode.get(2 * i + 1).copied().unwrap_or(1.0));
                    let e = if (d1 - d0).abs() < 1e-12 { e0 } else { e0 + (v - d0) * (e1 - e0) / (d1 - d0) };
                    pos.push(e.clamp(0.0, (size.get(i).copied().unwrap_or(1) - 1) as f64));
                }
                let mut out = vec![0.0f64; *n_out];
                // Multilinear interpolation over the 2^m corners.
                for corner in 0..(1usize << m) {
                    let (mut w, mut idx, mut stride) = (1.0f64, 0usize, 1usize);
                    for i in 0..m {
                        let p = pos.get(i).copied().unwrap_or(0.0);
                        let lo = p.floor();
                        let f = p - lo;
                        let hi = corner >> i & 1 == 1;
                        let sz = size.get(i).copied().unwrap_or(1);
                        let at = (lo as usize + usize::from(hi)).min(sz - 1);
                        w *= if hi { f } else { 1.0 - f };
                        idx += at * stride;
                        stride *= sz;
                    }
                    if w == 0.0 {
                        continue;
                    }
                    for (j, o) in out.iter_mut().enumerate() {
                        *o += w * f64::from(samples.get(idx * n_out + j).copied().unwrap_or(0.0));
                    }
                }
                for (j, o) in out.iter_mut().enumerate() {
                    let (d0, d1) = (decode.get(2 * j).copied().unwrap_or(0.0), decode.get(2 * j + 1).copied().unwrap_or(1.0));
                    let v = d0 + *o * (d1 - d0);
                    let (r0, r1) = (range.get(2 * j).copied().unwrap_or(0.0), range.get(2 * j + 1).copied().unwrap_or(1.0));
                    *o = v.clamp(r0.min(r1), r0.max(r1));
                }
                out
            }
            Func::Calc { domain, range, prog } => {
                let mut st: Vec<(f64, bool)> = Vec::new();
                for (i, v) in x.iter().enumerate().take(domain.len() / 2) {
                    let (d0, d1) = (domain.get(2 * i).copied().unwrap_or(0.0), domain.get(2 * i + 1).copied().unwrap_or(1.0));
                    st.push((v.clamp(d0.min(d1), d0.max(d1)), false));
                }
                run(prog, &mut st);
                let n = range.len() / 2;
                // The last `n` values are the outputs (missing ones are 0).
                let skip = st.len().saturating_sub(n);
                let mut out: Vec<f64> = st.iter().skip(skip).map(|(v, _)| *v).collect();
                out.resize(n, 0.0);
                for (j, o) in out.iter_mut().enumerate() {
                    let (r0, r1) = (range.get(2 * j).copied().unwrap_or(0.0), range.get(2 * j + 1).copied().unwrap_or(1.0));
                    *o = if o.is_finite() { o.clamp(r0.min(r1), r0.max(r1)) } else { r0 };
                }
                out
            }
        }
    }
}

fn parse_program(data: &[u8]) -> Option<Vec<Ins>> {
    let text = String::from_utf8_lossy(data);
    // `{` and `}` are their own tokens.
    let spaced = text.replace('{', " { ").replace('}', " } ");
    let mut toks = spaced.split_ascii_whitespace();
    if toks.next()? != "{" {
        return None;
    }
    let mut count = 0usize;
    block(&mut toks, 0, &mut count)
}

fn block<'a>(toks: &mut impl Iterator<Item = &'a str>, depth: usize, count: &mut usize) -> Option<Vec<Ins>> {
    if depth > MAX_NEST {
        return None;
    }
    let mut out: Vec<Ins> = Vec::new();
    // A block just read, waiting for `if` or a second block and `ifelse`.
    let mut blocks: Vec<Vec<Ins>> = Vec::new();
    loop {
        let t = toks.next()?;
        *count += 1;
        if *count > MAX_PROGRAM {
            return None;
        }
        match t {
            "}" => return Some(out),
            "{" => blocks.push(block(toks, depth + 1, count)?),
            "if" => {
                let b = blocks.pop()?;
                out.push(Ins::If(b));
            }
            "ifelse" => {
                let b2 = blocks.pop()?;
                let b1 = blocks.pop()?;
                out.push(Ins::IfElse(b1, b2));
            }
            "true" => out.push(Ins::Bool(true)),
            "false" => out.push(Ins::Bool(false)),
            _ => {
                if let Ok(v) = t.parse::<f64>() {
                    if !v.is_finite() {
                        return None;
                    }
                    out.push(Ins::Num(v));
                } else {
                    out.push(Ins::Op(opcode(t)?));
                }
            }
        }
    }
}

fn opcode(t: &str) -> Option<Opc> {
    use Opc::*;
    Some(match t {
        "abs" => Abs,
        "add" => Add,
        "atan" => Atan,
        "ceiling" => Ceiling,
        "cos" => Cos,
        "cvi" => Cvi,
        "cvr" => Cvr,
        "div" => Div,
        "exp" => Exp,
        "floor" => Floor,
        "idiv" => Idiv,
        "ln" => Ln,
        "log" => Log,
        "mod" => Mod,
        "mul" => Mul,
        "neg" => Neg,
        "round" => Round,
        "sin" => Sin,
        "sqrt" => Sqrt,
        "sub" => Sub,
        "truncate" => Truncate,
        "and" => And,
        "bitshift" => Bitshift,
        "eq" => Eq,
        "ge" => Ge,
        "gt" => Gt,
        "le" => Le,
        "lt" => Lt,
        "ne" => Ne,
        "not" => Not,
        "or" => Or,
        "xor" => Xor,
        "copy" => Copy,
        "dup" => Dup,
        "exch" => Exch,
        "index" => Index,
        "pop" => Pop,
        "roll" => Roll,
        _ => return None,
    })
}

/// Runs a program; a stack underflow or overflow ends it quietly.
fn run(prog: &[Ins], st: &mut Vec<(f64, bool)>) -> Option<()> {
    for ins in prog {
        if st.len() > STACK {
            return None;
        }
        match ins {
            Ins::Num(v) => st.push((*v, false)),
            Ins::Bool(b) => st.push((f64::from(u8::from(*b)), true)),
            Ins::If(b) => {
                if st.pop()?.0 != 0.0 {
                    run(b, st)?;
                }
            }
            Ins::IfElse(a, b) => {
                if st.pop()?.0 != 0.0 {
                    run(a, st)?;
                } else {
                    run(b, st)?;
                }
            }
            Ins::Op(op) => exec(*op, st)?,
        }
    }
    Some(())
}

fn exec(op: Opc, st: &mut Vec<(f64, bool)>) -> Option<()> {
    use Opc::*;
    let num = |v: f64| (if v.is_finite() { v } else { 0.0 }, false);
    match op {
        Abs | Ceiling | Cos | Cvi | Cvr | Floor | Ln | Log | Neg | Round | Sin | Sqrt | Truncate => {
            let (a, _) = st.pop()?;
            let v = match op {
                Abs => a.abs(),
                Ceiling => a.ceil(),
                Cos => a.to_radians().cos(),
                Cvi | Truncate => a.trunc(),
                Floor => a.floor(),
                Ln => a.ln(),
                Log => a.log10(),
                Neg => -a,
                Round => (a + 0.5).floor(),
                Sin => a.to_radians().sin(),
                Sqrt => a.sqrt(),
                _ => a,
            };
            st.push(num(v));
        }
        Add | Atan | Div | Exp | Idiv | Mod | Mul | Sub | Bitshift => {
            let (b, _) = st.pop()?;
            let (a, _) = st.pop()?;
            let v = match op {
                Add => a + b,
                Sub => a - b,
                Mul => a * b,
                Div => a / b,
                Atan => {
                    let d = a.atan2(b).to_degrees();
                    if d < 0.0 { d + 360.0 } else { d }
                }
                Exp => a.powf(b),
                Idiv => (a.trunc() / b.trunc()).trunc(),
                Mod => a.trunc() % b.trunc(),
                _ => {
                    let (x, s) = (a as i64, b as i64);
                    if s >= 0 { x.checked_shl(s.min(63) as u32).unwrap_or(0) as f64 } else { (x >> (-s).min(63)) as f64 }
                }
            };
            st.push(num(v));
        }
        And | Or | Xor => {
            let (b, bb) = st.pop()?;
            let (a, ab) = st.pop()?;
            let (x, y) = (a as i64, b as i64);
            let v = match op {
                And => x & y,
                Or => x | y,
                _ => x ^ y,
            };
            st.push((v as f64, ab && bb));
        }
        Eq | Ge | Gt | Le | Lt | Ne => {
            let (b, _) = st.pop()?;
            let (a, _) = st.pop()?;
            let t = match op {
                Eq => a == b,
                Ge => a >= b,
                Gt => a > b,
                Le => a <= b,
                Lt => a < b,
                _ => a != b,
            };
            st.push((f64::from(u8::from(t)), true));
        }
        Not => {
            let (a, is_bool) = st.pop()?;
            st.push(if is_bool { (f64::from(u8::from(a == 0.0)), true) } else { (!(a as i64) as f64, false) });
        }
        Copy => {
            let n = st.pop()?.0 as usize;
            let from = st.len().checked_sub(n)?;
            if st.len() + n > STACK {
                return None;
            }
            let part: Vec<_> = st.get(from..)?.to_vec();
            st.extend(part);
        }
        Dup => {
            let v = *st.last()?;
            st.push(v);
        }
        Exch => {
            let n = st.len();
            if n < 2 {
                return None;
            }
            st.swap(n - 1, n - 2);
        }
        Index => {
            let n = st.pop()?.0 as usize;
            let v = *st.get(st.len().checked_sub(n)?.checked_sub(1)?)?;
            st.push(v);
        }
        Pop => {
            st.pop()?;
        }
        Roll => {
            let j = st.pop()?.0 as i64;
            let n = st.pop()?.0 as usize;
            let from = st.len().checked_sub(n)?;
            if n > 0 {
                let part = st.get_mut(from..)?;
                let k = j.rem_euclid(n as i64) as usize;
                part.rotate_right(k);
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc(src: &str, x: &[f64], outs: usize) -> Vec<f64> {
        let f = Func::Calc {
            domain: [0.0, 1.0].repeat(x.len().max(1)),
            range: [-100.0, 100.0].repeat(outs),
            prog: parse_program(src.as_bytes()).unwrap(),
        };
        f.eval(x)
    }

    #[test]
    fn calculator_arithmetic_stack_and_branches() {
        assert_eq!(calc("{ 2 mul 1 exch sub }", &[0.25], 1), vec![0.5]);
        assert_eq!(calc("{ dup 0.5 gt { pop 1 } { pop 0 } ifelse }", &[0.75], 1), vec![1.0]);
        assert_eq!(calc("{ dup 0.5 gt { pop 1 } { pop 0 } ifelse }", &[0.25], 1), vec![0.0]);
        assert_eq!(calc("{ 1 2 3 3 1 roll }", &[0.0], 3), vec![3.0, 1.0, 2.0]);
        assert_eq!(calc("{ 90 sin 4 sqrt add }", &[0.0], 1), vec![3.0]);
        assert_eq!(calc("{ true not { 1 } { 2 } ifelse }", &[0.0], 1), vec![2.0]);
    }

    #[test]
    fn bad_programs_are_rejected_or_run_quietly() {
        assert!(parse_program(b"{ frob }").is_none());
        assert!(parse_program(b"{ 1 2").is_none());
        assert!(parse_program(&b"{".repeat(100)).is_none());
        // Underflow / overflow end the program; the outputs are zero.
        assert_eq!(calc("{ pop pop pop add }", &[0.5], 1), vec![0.0]);
        let src = format!("{{ {} }}", "dup ".repeat(500));
        assert_eq!(calc(&src, &[0.5], 1).len(), 1);
        assert_eq!(calc("{ 1 0 div }", &[0.0], 1), vec![0.0]);
    }

    #[test]
    fn exponential_and_stitching() {
        let e = Func::Exp { domain: [0.0, 1.0], c0: vec![0.0, 1.0], c1: vec![1.0, 0.0], n: 1.0 };
        assert_eq!(e.eval(&[0.25]), vec![0.25, 0.75]);
        assert_eq!(e.eval(&[7.0]), vec![1.0, 0.0], "clipped to the domain");
        let s = Func::Stitch {
            domain: [0.0, 1.0],
            funcs: vec![
                Func::Exp { domain: [0.0, 1.0], c0: vec![0.0], c1: vec![10.0], n: 1.0 },
                Func::Exp { domain: [0.0, 1.0], c0: vec![5.0], c1: vec![0.0], n: 1.0 },
            ],
            bounds: vec![0.5],
            encode: vec![0.0, 1.0, 0.0, 1.0],
        };
        assert_eq!(s.eval(&[0.25]), vec![5.0]);
        assert_eq!(s.eval(&[0.75]), vec![2.5]);
    }

    #[test]
    fn sampled_interpolates_and_decodes() {
        // Two 8-bit samples 0 and 255 over [0, 1] → output 0..1.
        let f = Func::Sampled {
            domain: vec![0.0, 1.0],
            range: vec![0.0, 1.0],
            size: vec![2],
            encode: vec![0.0, 1.0],
            decode: vec![0.0, 1.0],
            n_out: 1,
            samples: vec![0.0, 1.0],
        };
        assert_eq!(f.eval(&[0.5]), vec![0.5]);
        assert_eq!(f.eval(&[f64::NAN]).len(), 1);
    }
}
