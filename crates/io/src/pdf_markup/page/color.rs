//! Colour spaces (ISO 32000 §8.6): the device spaces, CIE-based ones read as their device
//! equivalent (CalGray, CalRGB, ICCBased by `/N`), Lab, Indexed, Separation / DeviceN through
//! their tint transform, and Pattern.

use std::rc::Rc;

use super::func::Func;
use crate::pdf_markup::{Dict, Obj, Pdf};

const MAX_DEPTH: usize = 6;

pub(super) enum Cs {
    Gray,
    Rgb,
    Cmyk,
    Lab { white: [f64; 3], range: [f64; 4] },
    Indexed { base: Rc<Cs>, hival: usize, table: Vec<u8> },
    Tint { alt: Rc<Cs>, f: Func, n: usize },
    Pattern(Option<Rc<Cs>>),
}

pub(super) fn gray(v: f64) -> [u8; 3] {
    let g = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [g, g, g]
}

pub(super) fn rgb(r: f64, g: f64, b: f64) -> [u8; 3] {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [c(r), c(g), c(b)]
}

pub(super) fn cmyk(c: f64, m: f64, y: f64, k: f64) -> [u8; 3] {
    let k = 1.0 - k.clamp(0.0, 1.0);
    rgb((1.0 - c.clamp(0.0, 1.0)) * k, (1.0 - m.clamp(0.0, 1.0)) * k, (1.0 - y.clamp(0.0, 1.0)) * k)
}

impl Cs {
    /// The colour space `o` names or holds; `res` is the resource dictionary names are looked
    /// up in (`/ColorSpace`).
    pub(super) fn load(pdf: &mut Pdf, o: &Obj, res: &Dict, depth: usize) -> Option<Rc<Cs>> {
        if depth > MAX_DEPTH {
            return None;
        }
        match pdf.resolve(o) {
            Obj::Name(n) => match n.as_str() {
                "DeviceGray" | "G" | "CalGray" => Some(Rc::new(Cs::Gray)),
                "DeviceRGB" | "RGB" | "CalRGB" => Some(Rc::new(Cs::Rgb)),
                "DeviceCMYK" | "CMYK" => Some(Rc::new(Cs::Cmyk)),
                "Pattern" => Some(Rc::new(Cs::Pattern(None))),
                _ => {
                    let Obj::Dict(spaces) = pdf.val(res, "ColorSpace") else { return None };
                    let named = spaces.get(&n)?.clone();
                    // A name that names itself is not a colour space.
                    if matches!(&named, Obj::Name(m) if *m == n) {
                        return None;
                    }
                    Cs::load(pdf, &named, res, depth + 1)
                }
            },
            Obj::Arr(a) => {
                let Obj::Name(kind) = pdf.resolve(a.first()?) else { return None };
                match kind.as_str() {
                    "DeviceGray" | "G" | "CalGray" => Some(Rc::new(Cs::Gray)),
                    "DeviceRGB" | "RGB" | "CalRGB" => Some(Rc::new(Cs::Rgb)),
                    "DeviceCMYK" | "CMYK" => Some(Rc::new(Cs::Cmyk)),
                    "ICCBased" => {
                        let Obj::Stream(d, ..) = pdf.resolve(a.get(1)?) else { return None };
                        match pdf.num_of(&d, "N")? as i64 {
                            1 => Some(Rc::new(Cs::Gray)),
                            3 => Some(Rc::new(Cs::Rgb)),
                            4 => Some(Rc::new(Cs::Cmyk)),
                            _ => None,
                        }
                    }
                    "Lab" => {
                        let Obj::Dict(d) = pdf.resolve(a.get(1)?) else { return None };
                        let w = pdf.nums_of(&d, "WhitePoint").filter(|w| w.len() == 3)?;
                        let r = pdf.nums_of(&d, "Range").filter(|r| r.len() == 4).unwrap_or_else(|| vec![-100.0, 100.0, -100.0, 100.0]);
                        Some(Rc::new(Cs::Lab { white: [w[0], w[1], w[2]], range: [r[0], r[1], r[2], r[3]] }))
                    }
                    "Indexed" | "I" => {
                        let base = Cs::load(pdf, a.get(1)?, res, depth + 1)?;
                        let Obj::Num(hv) = pdf.resolve(a.get(2)?) else { return None };
                        let hival = (hv.max(0.0) as usize).min(255);
                        let table = match pdf.resolve(a.get(3)?) {
                            Obj::Str(s) => s,
                            Obj::Stream(d, x, y, id) => pdf.decode(&d, x, y, id)?,
                            _ => return None,
                        };
                        Some(Rc::new(Cs::Indexed { base, hival, table }))
                    }
                    "Separation" | "DeviceN" => {
                        let n = match pdf.resolve(a.get(1)?) {
                            Obj::Arr(names) => names.len(),
                            _ => 1,
                        };
                        if n == 0 || n > 32 {
                            return None;
                        }
                        let alt = Cs::load(pdf, a.get(2)?, res, depth + 1)?;
                        let f = Func::load(pdf, a.get(3)?, 0)?;
                        Some(Rc::new(Cs::Tint { alt, f, n }))
                    }
                    "Pattern" => {
                        let under = match a.get(1) {
                            Some(o) => Cs::load(pdf, o, res, depth + 1),
                            None => None,
                        };
                        Some(Rc::new(Cs::Pattern(under)))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The number of components of a colour.
    pub(super) fn n(&self) -> usize {
        match self {
            Cs::Gray | Cs::Indexed { .. } => 1,
            Cs::Rgb | Cs::Lab { .. } => 3,
            Cs::Cmyk => 4,
            Cs::Tint { n, .. } => *n,
            Cs::Pattern(u) => u.as_ref().map_or(0, |u| u.n()),
        }
    }

    /// The colour `sc` selects when nothing was set.
    pub(super) fn initial(&self) -> Vec<f64> {
        match self {
            Cs::Cmyk => vec![0.0, 0.0, 0.0, 1.0],
            Cs::Tint { n, .. } => vec![1.0; *n],
            Cs::Lab { range, .. } => vec![0.0, range[0].max(0.0).min(range[1]), range[2].max(0.0).min(range[3])],
            _ => vec![0.0; self.n()],
        }
    }

    pub(super) fn rgb(&self, c: &[f64]) -> [u8; 3] {
        let g = |i: usize| c.get(i).copied().unwrap_or(0.0);
        match self {
            Cs::Gray => gray(g(0)),
            Cs::Rgb => rgb(g(0), g(1), g(2)),
            Cs::Cmyk => cmyk(g(0), g(1), g(2), g(3)),
            Cs::Lab { white, range } => {
                let (l, a, b) = (g(0).clamp(0.0, 100.0), g(1).clamp(range[0], range[1]), g(2).clamp(range[2], range[3]));
                let m = (l + 16.0) / 116.0;
                let (lx, ly, lz) = (m + a / 500.0, m, m - b / 200.0);
                let f = |t: f64| if t >= 6.0 / 29.0 { t * t * t } else { 108.0 / 841.0 * (t - 4.0 / 29.0) };
                let (x, y, z) = (white[0] * f(lx), white[1] * f(ly), white[2] * f(lz));
                // XYZ (D50-ish white kept as is) to linear sRGB, then the sRGB curve.
                let lin = [3.2406 * x - 1.5372 * y - 0.4986 * z, -0.9689 * x + 1.8758 * y + 0.0415 * z, 0.0557 * x - 0.2040 * y + 1.0570 * z];
                let enc = |v: f64| {
                    let v = v.clamp(0.0, 1.0);
                    if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
                };
                rgb(enc(lin[0]), enc(lin[1]), enc(lin[2]))
            }
            Cs::Indexed { base, hival, table } => {
                let i = (g(0).max(0.0) as usize).min(*hival);
                let n = base.n().max(1);
                let comps: Vec<f64> = (0..n).map(|k| f64::from(table.get(i * n + k).copied().unwrap_or(0)) / 255.0).collect();
                // A Lab base's bytes span its range, not 0..1.
                match &**base {
                    Cs::Lab { range, .. } => {
                        base.rgb(&[comps[0] * 100.0, range[0] + comps[1] * (range[1] - range[0]), range[2] + comps[2] * (range[3] - range[2])])
                    }
                    _ => base.rgb(&comps),
                }
            }
            Cs::Tint { alt, f, .. } => alt.rgb(&f.eval(c)),
            Cs::Pattern(u) => u.as_ref().map_or([0; 3], |u| u.rgb(c)),
        }
    }
}
