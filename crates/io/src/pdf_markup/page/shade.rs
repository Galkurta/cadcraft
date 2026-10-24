//! Shadings (ISO 32000 §8.7.4.5): function-based (type 1), axial (2) and radial (3), painted
//! per pixel into a layer. The mesh types (4–7) are not drawn.

use std::rc::Rc;

use tiny_skia::{Pixmap, Transform};

use super::color::Cs;
use super::func::Func;
use crate::pdf_markup::{Dict, Obj, Pdf};

pub(super) struct Shading {
    kind: u8,
    cs: Rc<Cs>,
    pub background: Option<[u8; 3]>,
    /// Type 1: the domain; 2, 3: the coordinates.
    coords: Vec<f64>,
    t: [f64; 2],
    extend: [bool; 2],
    func: Func,
    /// Type 1: the matrix from the domain to the target space.
    matrix: [f64; 6],
}

pub(super) fn load(pdf: &mut Pdf, o: &Obj, res: &Dict, notes: &mut Vec<String>) -> Option<Shading> {
    let d = match pdf.resolve(o) {
        Obj::Dict(d) => d,
        Obj::Stream(d, ..) => d,
        _ => return None,
    };
    let kind = pdf.num_of(&d, "ShadingType")? as u8;
    if !matches!(kind, 1..=3) {
        let s = "mesh shadings (types 4-7) are not drawn";
        if !notes.iter().any(|n| n == s) {
            notes.push(s.into());
        }
        return None;
    }
    let cs = Cs::load(pdf, d.get("ColorSpace")?, res, 0)?;
    let func = Func::load(pdf, d.get("Function")?, 0)?;
    let background = pdf.nums_of(&d, "Background").map(|b| cs.rgb(&b));
    let (coords, t, extend, matrix) = match kind {
        1 => {
            let dom = pdf.nums_of(&d, "Domain").filter(|v| v.len() == 4).unwrap_or_else(|| vec![0.0, 1.0, 0.0, 1.0]);
            let m = pdf.nums_of(&d, "Matrix").filter(|v| v.len() == 6).unwrap_or_else(|| vec![1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
            (dom, [0.0, 1.0], [false; 2], [m[0], m[1], m[2], m[3], m[4], m[5]])
        }
        _ => {
            let c = pdf.nums_of(&d, "Coords").filter(|v| v.len() == if kind == 2 { 4 } else { 6 })?;
            let t = pdf.nums_of(&d, "Domain").filter(|v| v.len() == 2).map_or([0.0, 1.0], |v| [v[0], v[1]]);
            let e = match pdf.val(&d, "Extend") {
                Obj::Arr(a) if a.len() == 2 => [matches!(pdf.resolve(&a[0]), Obj::Bool(true)), matches!(pdf.resolve(&a[1]), Obj::Bool(true))],
                _ => [false; 2],
            };
            (c, t, e, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0])
        }
    };
    Some(Shading { kind, cs, background, coords, t, extend, func, matrix })
}

impl Shading {
    fn color(&self, vals: &[f64]) -> [u8; 3] {
        self.cs.rgb(&self.func.eval(vals))
    }

    /// The shading as a `w` × `h` layer whose top left pixel is device pixel `origin`;
    /// `to_dev` maps the shading's space to device pixels. Pixels the shading does not cover
    /// stay transparent (or take the background when `use_bg`). `None` when the budget of
    /// pixels is spent or the transform cannot be inverted.
    pub(super) fn paint(&self, w: u32, h: u32, origin: (i32, i32), to_dev: Transform, use_bg: bool, budget: &mut u64) -> Option<Pixmap> {
        let px = u64::from(w) * u64::from(h);
        if px == 0 || px > *budget {
            return None;
        }
        *budget -= px;
        let mut pm = Pixmap::new(w, h)?;
        let inv = to_dev.invert()?;
        let c = |i: usize| self.coords.get(i).copied().unwrap_or(0.0);
        // 256 colours along t for the 1-input types.
        let lut: Vec<[u8; 3]> = if self.kind == 1 {
            Vec::new()
        } else {
            (0..256).map(|i| self.color(&[self.t[0] + (self.t[1] - self.t[0]) * f64::from(i) / 255.0])).collect()
        };
        let at = |s: f64| lut.get((s.clamp(0.0, 1.0) * 255.0).round() as usize).copied();
        let m = &self.matrix;
        let minv = {
            let det = m[0] * m[3] - m[1] * m[2];
            (det.abs() > 1e-12)
                .then(|| [m[3] / det, -m[1] / det, -m[2] / det, m[0] / det, (m[2] * m[5] - m[3] * m[4]) / det, (m[1] * m[4] - m[0] * m[5]) / det])
        };
        let bg = if use_bg { self.background } else { None };
        let stride = w as usize * 4;
        for (py, row) in pm.data_mut().chunks_mut(stride.max(4)).enumerate() {
            for (pxi, p) in row.chunks_mut(4).enumerate() {
                let (dx, dy) = (f64::from(origin.0) + pxi as f64 + 0.5, f64::from(origin.1) + py as f64 + 0.5);
                let x = f64::from(inv.sx) * dx + f64::from(inv.kx) * dy + f64::from(inv.tx);
                let y = f64::from(inv.ky) * dx + f64::from(inv.sy) * dy + f64::from(inv.ty);
                let col = match self.kind {
                    1 => minv.and_then(|mi| {
                        let (u, v) = (mi[0] * x + mi[2] * y + mi[4], mi[1] * x + mi[3] * y + mi[5]);
                        (u >= c(0) && u <= c(1) && v >= c(2) && v <= c(3)).then(|| self.color(&[u, v]))
                    }),
                    2 => {
                        let (dxa, dya) = (c(2) - c(0), c(3) - c(1));
                        let len2 = dxa * dxa + dya * dya;
                        let s = if len2 < 1e-18 { 0.0 } else { ((x - c(0)) * dxa + (y - c(1)) * dya) / len2 };
                        self.extended(s).and_then(at)
                    }
                    _ => {
                        // Circles (x0, y0, r0) → (x1, y1, r1): the largest s with r(s) ≥ 0.
                        let (cx, cy, dr) = (c(3) - c(0), c(4) - c(1), c(5) - c(2));
                        let (px0, py0) = (x - c(0), y - c(1));
                        let a = cx * cx + cy * cy - dr * dr;
                        let b = px0 * cx + py0 * cy + c(2) * dr;
                        let cc = px0 * px0 + py0 * py0 - c(2) * c(2);
                        let cands = if a.abs() < 1e-12 {
                            if b.abs() < 1e-12 { [None, None] } else { [Some(cc / (2.0 * b)), None] }
                        } else {
                            let disc = b * b - a * cc;
                            if disc < 0.0 { [None, None] } else { [Some((b + disc.sqrt()) / a), Some((b - disc.sqrt()) / a)] }
                        };
                        let mut best: Option<f64> = None;
                        for s in cands.into_iter().flatten() {
                            if c(2) + s * dr < 0.0 {
                                continue;
                            }
                            if let Some(s) = self.extended(s)
                                && best.is_none_or(|b| s > b)
                            {
                                best = Some(s);
                            }
                        }
                        best.and_then(at)
                    }
                };
                if let Some(rgb) = col.or(bg)
                    && let Some(dst) = p.get_mut(..4)
                {
                    dst.copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
            }
        }
        Some(pm)
    }

    /// `s` after the extension rules, `None` when it falls outside an unextended end.
    fn extended(&self, s: f64) -> Option<f64> {
        if s < 0.0 {
            self.extend[0].then_some(0.0)
        } else if s > 1.0 {
            self.extend[1].then_some(1.0)
        } else {
            Some(s)
        }
    }
}
