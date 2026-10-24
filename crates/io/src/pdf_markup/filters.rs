//! The stream filters other than Flate (LZW, ASCIIHex, ASCII85, RunLength) and the predictors.
//! Every decoder takes the most output bytes it may produce and returns `None` for bad data.

/// LZW (`/EarlyChange` 1 is the default): codes of 9-12 bits, most significant bit first.
pub(super) fn lzw(data: &[u8], early_change: bool, limit: usize) -> Option<Vec<u8>> {
    let early = usize::from(early_change);
    // Entry `code - 258` is (prefix code, appended byte).
    let mut table: Vec<(u16, u8)> = Vec::new();
    let (mut out, mut chain) = (Vec::new(), Vec::new());
    let (mut bits, mut acc, mut have, mut prev) = (9usize, 0u32, 0usize, None::<u16>);
    for &byte in data {
        acc = (acc << 8) | u32::from(byte);
        have += 8;
        while have >= bits {
            let code = ((acc >> (have - bits)) & ((1 << bits) - 1)) as u16;
            have -= bits;
            acc &= (1 << have) - 1;
            match code {
                256 => {
                    table.clear();
                    (bits, prev) = (9, None);
                    continue;
                }
                257 => return Some(out),
                _ => {}
            }
            // The string of `code`, reversed in `chain`; a code just being defined is the
            // previous string plus its own first byte.
            let fresh = usize::from(code) == 258 + table.len();
            let mut cur = if fresh { prev? } else { code };
            chain.clear();
            while cur >= 256 {
                let &(p, b) = table.get(usize::from(cur) - 258)?;
                chain.push(b);
                cur = p;
            }
            chain.push(cur as u8);
            let first = *chain.last()?;
            if out.len().checked_add(chain.len() + usize::from(fresh))? > limit {
                return None;
            }
            out.extend(chain.iter().rev());
            if fresh {
                out.push(first);
            }
            if let Some(p) = prev
                && table.len() < 4096 - 258
            {
                table.push((p, first));
            }
            prev = Some(code);
            let n = 258 + table.len() + early;
            bits = if n >= 2048 {
                12
            } else if n >= 1024 {
                11
            } else if n >= 512 {
                10
            } else {
                9
            };
        }
    }
    // A stream without its end-of-data code is read as far as it goes.
    Some(out)
}

pub(super) fn ascii_hex(data: &[u8]) -> Vec<u8> {
    let (mut out, mut hi) = (Vec::new(), None::<u8>);
    for &b in data {
        if b == b'>' {
            break;
        }
        if let Some(d) = (b as char).to_digit(16) {
            match hi.take() {
                Some(h) => out.push(h * 16 + d as u8),
                None => hi = Some(d as u8),
            }
        }
    }
    out.extend(hi.map(|h| h * 16));
    out
}

pub(super) fn ascii85(data: &[u8]) -> Option<Vec<u8>> {
    let data = data.strip_prefix(b"<~").unwrap_or(data);
    let (mut out, mut group, mut n) = (Vec::new(), [0u8; 5], 0usize);
    let flush = |out: &mut Vec<u8>, group: &[u8; 5], n: usize| -> Option<()> {
        // A short last group is padded with `u` and keeps one byte fewer than it has digits.
        let v = group.iter().enumerate().try_fold(0u64, |a, (i, d)| Some(a * 85 + u64::from(if i < n { *d } else { 84 })))?;
        let v = u32::try_from(v).ok()?;
        out.extend(v.to_be_bytes().iter().take(n.checked_sub(1)?));
        Some(())
    };
    for &b in data {
        match b {
            b'~' => break,
            b'z' if n == 0 => out.extend([0u8; 4]),
            b'!'..=b'u' => {
                *group.get_mut(n)? = b - b'!';
                n += 1;
                if n == 5 {
                    flush(&mut out, &group, 5)?;
                    n = 0;
                }
            }
            b if super::is_ws(b) => {}
            _ => return None,
        }
    }
    match n {
        0 => {}
        1 => return None,
        _ => flush(&mut out, &group, n)?,
    }
    Some(out)
}

pub(super) fn run_length(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    let (mut out, mut i) = (Vec::new(), 0usize);
    while let Some(&n) = data.get(i) {
        i += 1;
        match n {
            128 => break,
            0..=127 => {
                let run = data.get(i..i + usize::from(n) + 1)?;
                out.extend_from_slice(run);
                i += run.len();
            }
            _ => {
                let b = *data.get(i)?;
                i += 1;
                out.extend(std::iter::repeat_n(b, 257 - usize::from(n)));
            }
        }
        if out.len() > limit {
            return None;
        }
    }
    Some(out)
}

/// Undoes a predictor: PNG (10-15, a filter byte per row) or TIFF 2 (8-bit samples only).
pub(super) fn predict(data: Vec<u8>, pred: usize, colors: usize, bpc: usize, cols: usize, limit: usize) -> Option<Vec<u8>> {
    if pred < 2 {
        return Some(data);
    }
    let bits = colors.checked_mul(bpc)?;
    let bpp = bits.div_ceil(8).max(1);
    let rowlen = bits.checked_mul(cols)?.div_ceil(8);
    if rowlen == 0 || rowlen > limit {
        return None;
    }
    if pred == 2 {
        if bpc != 8 {
            return None;
        }
        let mut out = data;
        for row in out.chunks_mut(rowlen) {
            for i in colors..row.len() {
                let left = row.get(i - colors).copied().unwrap_or(0);
                if let Some(x) = row.get_mut(i) {
                    *x = x.wrapping_add(left);
                }
            }
        }
        return Some(out);
    }
    if pred < 10 {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    let mut prev = vec![0u8; rowlen];
    for chunk in data.chunks(rowlen + 1) {
        let ft = chunk.first().copied().unwrap_or(0);
        let mut row = Vec::with_capacity(rowlen);
        for i in 0..rowlen {
            let x = chunk.get(i + 1).copied().unwrap_or(0);
            let left = i.checked_sub(bpp).and_then(|j| row.get(j)).copied().unwrap_or(0);
            let up = prev.get(i).copied().unwrap_or(0);
            let ul = i.checked_sub(bpp).and_then(|j| prev.get(j)).copied().unwrap_or(0);
            let v = match ft {
                0 => x,
                1 => x.wrapping_add(left),
                2 => x.wrapping_add(up),
                3 => x.wrapping_add(((u16::from(left) + u16::from(up)) / 2) as u8),
                4 => {
                    let (a, b, c) = (i16::from(left), i16::from(up), i16::from(ul));
                    let pp = a + b - c;
                    let (pa, pb, pc) = ((pp - a).abs(), (pp - b).abs(), (pp - c).abs());
                    x.wrapping_add(if pa <= pb && pa <= pc {
                        left
                    } else if pb <= pc {
                        up
                    } else {
                        ul
                    })
                }
                _ => return None,
            };
            row.push(v);
        }
        out.extend_from_slice(&row);
        prev = row;
    }
    Some(out)
}
