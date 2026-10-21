//! Just enough of a bare CFF font program (`/FontFile3` `/Type1C` and `/CIDFontType0C`): the
//! glyph count, whether it is CID-keyed and its charset, and a wrapper that puts it in a minimal
//! OpenType container so skrifa can draw its glyphs.

use std::collections::HashMap;

pub(super) struct Cff {
    pub n_glyphs: usize,
    pub cid_keyed: bool,
    /// CID of every glyph (CID-keyed fonts only).
    pub cid_to_gid: HashMap<u32, u32>,
}

fn u16_at(d: &[u8], i: usize) -> Option<usize> {
    Some(usize::from(u16::from_be_bytes(d.get(i..i.checked_add(2)?)?.try_into().ok()?)))
}

/// `(count, end offset, byte range of every element)` of the INDEX at `off`.
fn index(d: &[u8], off: usize) -> Option<(usize, usize, Vec<(usize, usize)>)> {
    let count = u16_at(d, off)?;
    if count == 0 {
        return Some((0, off.checked_add(2)?, Vec::new()));
    }
    let size = usize::from(*d.get(off.checked_add(2)?)?);
    if !(1..=4).contains(&size) {
        return None;
    }
    let offs_at = off.checked_add(3)?;
    let data_at = offs_at.checked_add(count.checked_add(1)?.checked_mul(size)?)?.checked_sub(1)?;
    let read = |k: usize| -> Option<usize> {
        let s = d.get(offs_at.checked_add(k.checked_mul(size)?)?..)?.get(..size)?;
        Some(s.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b)))
    };
    let mut ranges = Vec::new();
    for k in 0..count.min(65_535) {
        let (a, b) = (read(k)?, read(k + 1)?);
        if b < a {
            return None;
        }
        ranges.push((data_at.checked_add(a)?, data_at.checked_add(b)?));
    }
    let end = data_at.checked_add(read(count)?)?;
    Some((count, end, ranges))
}

/// The operators of a DICT with their (first) operand: `(operator, operand)`.
fn dict(d: &[u8]) -> Vec<(u16, f64)> {
    let mut out = Vec::new();
    let (mut i, mut first) = (0usize, None::<f64>);
    while let Some(&b) = d.get(i) {
        i += 1;
        match b {
            0..=21 => {
                let op = if b == 12 {
                    let n = d.get(i).copied().unwrap_or(0);
                    i += 1;
                    0x0C00 | u16::from(n)
                } else {
                    u16::from(b)
                };
                if let Some(v) = first.take() {
                    out.push((op, v));
                } else {
                    out.push((op, 0.0));
                }
            }
            28 => {
                let v = i16::from_be_bytes([d.get(i).copied().unwrap_or(0), d.get(i + 1).copied().unwrap_or(0)]);
                i += 2;
                first.get_or_insert(f64::from(v));
            }
            29 => {
                let s: [u8; 4] = [0, 1, 2, 3].map(|k| d.get(i + k).copied().unwrap_or(0));
                i += 4;
                first.get_or_insert(f64::from(i32::from_be_bytes(s)));
            }
            30 => {
                // A real number: nibbles up to 0xF; the value is not needed.
                while let Some(&n) = d.get(i) {
                    i += 1;
                    if n & 0x0F == 0x0F || n >> 4 == 0x0F {
                        break;
                    }
                }
                first.get_or_insert(0.0);
            }
            32..=246 => {
                first.get_or_insert(f64::from(b) - 139.0);
            }
            247..=250 => {
                let v = (f64::from(b) - 247.0) * 256.0 + f64::from(d.get(i).copied().unwrap_or(0)) + 108.0;
                i += 1;
                first.get_or_insert(v);
            }
            251..=254 => {
                let v = -(f64::from(b) - 251.0) * 256.0 - f64::from(d.get(i).copied().unwrap_or(0)) - 108.0;
                i += 1;
                first.get_or_insert(v);
            }
            _ => {}
        }
        if out.len() > 256 {
            break;
        }
    }
    out
}

pub(super) fn parse(d: &[u8]) -> Option<Cff> {
    let hdr = usize::from(*d.get(2)?);
    let (_, after_names, _) = index(d, hdr)?;
    let (_, _, tops) = index(d, after_names)?;
    let (a, b) = *tops.first()?;
    let top = dict(d.get(a..b)?);
    let get = |op: u16| top.iter().find(|(o, _)| *o == op).map(|(_, v)| *v);
    let cs = get(17).filter(|v| *v > 0.0)? as usize;
    let n_glyphs = u16_at(d, cs)?;
    let cid_keyed = get(0x0C1E).is_some() || top.iter().any(|(o, _)| *o == 0x0C1E);
    let mut cid_to_gid = HashMap::new();
    if cid_keyed {
        let cso = get(15).filter(|v| *v > 2.0).map_or(0, |v| v as usize);
        if cso > 0 {
            let format = *d.get(cso)?;
            let mut p = cso + 1;
            let mut gid = 1u32;
            cid_to_gid.insert(0, 0);
            match format {
                0 => {
                    while (gid as usize) < n_glyphs {
                        cid_to_gid.insert(u16_at(d, p)? as u32, gid);
                        p += 2;
                        gid += 1;
                    }
                }
                1 | 2 => {
                    while (gid as usize) < n_glyphs {
                        let first = u16_at(d, p)? as u32;
                        let left = if format == 1 { usize::from(*d.get(p + 2)?) } else { u16_at(d, p + 2)? };
                        p += if format == 1 { 3 } else { 4 };
                        for k in 0..=left as u32 {
                            if gid as usize >= n_glyphs {
                                break;
                            }
                            cid_to_gid.insert(first + k, gid);
                            gid += 1;
                        }
                    }
                }
                _ => return None,
            }
        }
    }
    Some(Cff { n_glyphs, cid_keyed, cid_to_gid })
}

/// The CFF program in a minimal OpenType wrapper (`CFF `, `head`, `hhea`, `hmtx`, `maxp`: what
/// skrifa needs to draw glyphs; every advance reads as 1000).
pub(super) fn wrap(cff: &[u8], n_glyphs: usize) -> Vec<u8> {
    let n = n_glyphs.min(65_535);
    let mut head = vec![0u8; 54];
    head[..4].copy_from_slice(&[0, 1, 0, 0]);
    head[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes());
    head[18..20].copy_from_slice(&1000u16.to_be_bytes());
    let mut hhea = vec![0u8; 36];
    hhea[34..36].copy_from_slice(&(n as u16).to_be_bytes());
    let hmtx: Vec<u8> = (0..n).flat_map(|_| [3, 0xE8, 0, 0]).collect();
    let mut maxp = vec![0, 0, 0x50, 0];
    maxp.extend((n as u16).to_be_bytes());
    let tables: [(&[u8; 4], &[u8]); 5] = [(b"CFF ", cff), (b"head", &head), (b"hhea", &hhea), (b"hmtx", &hmtx), (b"maxp", &maxp)];
    let mut out = Vec::new();
    out.extend(b"OTTO");
    out.extend((tables.len() as u16).to_be_bytes());
    out.extend([0, 64, 0, 2, 0, 16]);
    let mut off = 12 + 16 * tables.len();
    for (tag, data) in &tables {
        out.extend(*tag);
        out.extend(0u32.to_be_bytes());
        out.extend((off as u32).to_be_bytes());
        out.extend((data.len() as u32).to_be_bytes());
        off += data.len().div_ceil(4) * 4;
    }
    for (_, data) in &tables {
        out.extend(*data);
        out.resize(out.len().div_ceil(4) * 4, 0);
    }
    out
}
