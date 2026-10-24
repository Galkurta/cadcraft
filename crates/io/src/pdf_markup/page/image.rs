//! Image XObjects and inline images (ISO 32000 §8.9) decoded to straight RGBA: the colour spaces
//! of `color`, 1/2/4/8/16 bits per component, `/Decode`, `/ImageMask` stencils, `/SMask` and
//! `/Mask` (stencil or colour key), DCT (JPEG through the `image` crate). JPX, JBIG2 and CCITT
//! images are not decoded.

use std::rc::Rc;

use super::color::Cs;
use crate::pdf_markup::{Dict, Obj, Pdf, image_size};

/// Largest image, in pixels.
pub(super) const MAX_PIXELS: u64 = 32 << 20;
const MAX_SIDE: u64 = 65_536;
const MAX_MASK_DEPTH: usize = 2;

pub(super) struct Img {
    pub w: u32,
    pub h: u32,
    /// Straight (not premultiplied) RGBA. A stencil's colour is meant to be replaced.
    pub rgba: Vec<u8>,
    pub stencil: bool,
}

/// Everything an image needs from the interpreter.
pub(super) struct Env<'a, 'p> {
    pub pdf: &'p mut Pdf<'a>,
    /// Pixels still allowed to be decoded for this page.
    pub budget: &'p mut u64,
    pub notes: &'p mut Vec<String>,
}

fn note(notes: &mut Vec<String>, s: &str) {
    if notes.len() < 64 && !notes.iter().any(|n| n == s) {
        notes.push(s.to_string());
    }
}

fn raw(row: &[u8], idx: usize, bpc: usize) -> u32 {
    match bpc {
        8 => u32::from(row.get(idx).copied().unwrap_or(0)),
        16 => u32::from(row.get(idx * 2).copied().unwrap_or(0)) << 8 | u32::from(row.get(idx * 2 + 1).copied().unwrap_or(0)),
        _ => {
            let bit = idx * bpc;
            let byte = row.get(bit / 8).copied().unwrap_or(0);
            u32::from(byte >> (8 - bpc - bit % 8)) & ((1 << bpc) - 1)
        }
    }
}

/// The image stream `d` with raw (filtered) data `data`.
pub(super) fn load(env: &mut Env, d: &Dict, data: Vec<u8>, pending: Option<(String, Dict)>, res: &Dict, depth: usize) -> Option<Img> {
    let dim = |env: &mut Env, k: &str| env.pdf.num_of(d, k).filter(|v| *v >= 1.0 && *v <= MAX_SIDE as f64).map(|v| v as u64);
    let (mut w, mut h) = (dim(env, "Width")?, dim(env, "Height")?);
    let stencil = matches!(env.pdf.val(d, "ImageMask"), Obj::Bool(true));
    let mut bpc = if stencil { 1 } else { env.pdf.num_of(d, "BitsPerComponent").unwrap_or(8.0) as usize };
    let mut cs = if stencil {
        Rc::new(Cs::Gray)
    } else {
        match d.get("ColorSpace") {
            Some(o) => Cs::load(env.pdf, o, res, 0),
            None => None,
        }
        .unwrap_or_else(|| Rc::new(Cs::Gray))
    };
    let mut samples = data;
    let mut jpeg = false;
    if let Some((filter, _)) = pending {
        if !matches!(filter.as_str(), "DCTDecode" | "DCT") {
            note(env.notes, &format!("image filter {filter} is not drawn"));
            return None;
        }
        let (jw, jh) = image_size(&samples)?;
        if u64::from(jw) * u64::from(jh) > MAX_PIXELS || u64::from(jw) * u64::from(jh) > *env.budget {
            return None;
        }
        let Ok(dynimg) = image::load_from_memory_with_format(&samples, image::ImageFormat::Jpeg) else {
            note(env.notes, "a JPEG image could not be decoded");
            return None;
        };
        let comps = dynimg.color().channel_count();
        (w, h) = (u64::from(dynimg.width()), u64::from(dynimg.height()));
        samples = if comps == 1 { dynimg.into_luma8().into_raw() } else { dynimg.into_rgb8().into_raw() };
        bpc = 8;
        jpeg = true;
        let want = if comps == 1 { 1 } else { 3 };
        if cs.n() != want || matches!(&*cs, Cs::Indexed { .. }) {
            cs = Rc::new(if want == 1 { Cs::Gray } else { Cs::Rgb });
        }
    }
    let _ = jpeg;
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return None;
    }
    let px = w * h;
    if px > MAX_PIXELS || px > *env.budget {
        return None;
    }
    *env.budget -= px;
    let (wz, hz) = (w as usize, h as usize);
    let n = if stencil { 1 } else { cs.n().max(1) };
    let row_bytes = (wz * n * bpc).div_ceil(8);
    let maxv = ((1u64 << bpc) - 1) as f64;
    let indexed = matches!(&*cs, Cs::Indexed { .. });
    let dec = env.pdf.nums_of(d, "Decode").filter(|v| v.len() >= 2 * n);
    let dec_of = |k: usize| -> (f64, f64) {
        match &dec {
            Some(v) => (v.get(2 * k).copied().unwrap_or(0.0), v.get(2 * k + 1).copied().unwrap_or(1.0)),
            None if indexed => (0.0, maxv),
            None => (0.0, 1.0),
        }
    };
    let key = match env.pdf.val(d, "Mask") {
        Obj::Arr(a) if !stencil => {
            let v: Vec<f64> = a.iter().filter_map(|o| if let Obj::Num(n) = env.pdf.resolve(o) { Some(n) } else { None }).collect();
            (v.len() >= 2 * n).then_some(v)
        }
        _ => None,
    };
    let mut rgba = vec![255u8; px as usize * 4];
    let rows = |hz: usize| samples.chunks(row_bytes.max(1)).chain(std::iter::repeat(&[][..])).take(hz);
    if stencil {
        let invert = dec.as_ref().is_some_and(|v| v.first().copied().unwrap_or(0.0) > v.get(1).copied().unwrap_or(1.0));
        for (y, row) in rows(hz).enumerate() {
            for x in 0..wz {
                let painted = (raw(row, x, 1) == 0) != invert;
                if let Some(p) = rgba.get_mut((y * wz + x) * 4..(y * wz + x) * 4 + 4) {
                    p.copy_from_slice(&[0, 0, 0, if painted { 255 } else { 0 }]);
                }
            }
        }
    } else {
        // One-component spaces through a lookup table of every value.
        let lut: Option<Vec<[u8; 3]>> = (n == 1 && bpc <= 8).then(|| {
            let (d0, d1) = dec_of(0);
            (0..=(1usize << bpc) - 1).map(|v| cs.rgb(&[d0 + v as f64 * (d1 - d0) / maxv])).collect()
        });
        let direct = matches!(&*cs, Cs::Rgb) && bpc == 8 && dec.is_none() && key.is_none();
        let mut comps = vec![0.0f64; n];
        let mut rawv = vec![0u32; n];
        for (y, row) in rows(hz).enumerate() {
            for x in 0..wz {
                let at = (y * wz + x) * 4;
                let Some(p) = rgba.get_mut(at..at + 4) else { continue };
                if let Some(lut) = &lut {
                    let v = raw(row, x, bpc) as usize;
                    let c = lut.get(v).copied().unwrap_or([0; 3]);
                    p[..3].copy_from_slice(&c);
                    if let Some(k) = &key
                        && v as f64 >= k[0]
                        && v as f64 <= k[1]
                    {
                        p[3] = 0;
                    }
                } else if direct {
                    for (k, o) in p.iter_mut().take(3).enumerate() {
                        *o = row.get(x * 3 + k).copied().unwrap_or(0);
                    }
                } else {
                    for k in 0..n {
                        let v = raw(row, x * n + k, bpc);
                        rawv[k] = v;
                        let (d0, d1) = dec_of(k);
                        comps[k] = d0 + f64::from(v) * (d1 - d0) / maxv;
                    }
                    p[..3].copy_from_slice(&cs.rgb(&comps));
                    if let Some(k) = &key
                        && rawv.iter().enumerate().all(|(i, v)| f64::from(*v) >= k[2 * i] && f64::from(*v) <= k[2 * i + 1])
                    {
                        p[3] = 0;
                    }
                }
            }
        }
    }
    let mut img = Img { w: w as u32, h: h as u32, rgba, stencil };
    if stencil || depth >= MAX_MASK_DEPTH {
        return Some(img);
    }
    // An explicit soft mask or stencil mask becomes the alpha channel.
    let (mask_obj, soft) = match (d.get("SMask"), d.get("Mask")) {
        (Some(o), _) => (Some(o.clone()), true),
        (None, Some(o)) => (Some(o.clone()), false),
        _ => (None, false),
    };
    if let Some(m) = mask_obj
        && let Obj::Stream(md, a, b, id) = env.pdf.resolve(&m)
        && let Some((mdata, mp)) = env.pdf.decode_raw(&md, env.pdf.data.get(a..b)?.to_vec(), Some(id))
        && let Some(mask) = load(env, &md, mdata, mp, res, depth + 1)
    {
        img = apply_mask(img, &mask, soft);
    }
    Some(img)
}

/// `img` with the alpha of `mask` (a gray image for a soft mask, a stencil otherwise), the
/// larger of the two sizes kept.
fn apply_mask(img: Img, mask: &Img, soft: bool) -> Img {
    let (w, h) = (img.w.max(mask.w), img.h.max(mask.h));
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return img;
    }
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let at = |i: &Img| {
                ((u64::from(y) * u64::from(i.h) / u64::from(h)) as usize * i.w as usize + (u64::from(x) * u64::from(i.w) / u64::from(w)) as usize) * 4
            };
            let (a, b) = (at(&img), at(mask));
            let px = img.rgba.get(a..a + 3).unwrap_or(&[0, 0, 0]);
            let alpha = if soft { mask.rgba.get(b).copied().unwrap_or(255) } else { mask.rgba.get(b + 3).copied().unwrap_or(255) };
            let base = img.rgba.get(a + 3).copied().unwrap_or(255);
            rgba.extend([
                px.first().copied().unwrap_or(0),
                px.get(1).copied().unwrap_or(0),
                px.get(2).copied().unwrap_or(0),
                (u16::from(alpha) * u16::from(base) / 255) as u8,
            ]);
        }
    }
    Img { w, h, rgba, stencil: false }
}
