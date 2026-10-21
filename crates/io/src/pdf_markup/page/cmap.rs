//! Character maps of composite fonts: the predefined Identity-H/V, embedded CMap streams
//! (codespace ranges, `cidchar` / `cidrange`) and `/ToUnicode` maps (`bfchar` / `bfrange`), read
//! by a bounded token scanner. Other predefined CMap names read as 2-byte identity.

use std::collections::HashMap;

use crate::pdf_markup::{Lex, Obj};

const MAX_TOKENS: usize = 600_000;
const MAX_ENTRIES: usize = 200_000;
/// Largest code span one `cidrange` / `bfrange` may cover.
const MAX_SPAN: u32 = 0x1_0000;

#[derive(Default)]
pub(super) struct CMap {
    /// (byte length, low, high) codespace ranges.
    space: Vec<(usize, u32, u32)>,
    /// (byte length, low, high, first CID).
    ranges: Vec<(usize, u32, u32, u32)>,
    pub vertical: bool,
    pub identity: bool,
    /// `/ToUnicode` entries: code → text.
    pub unicode: HashMap<u32, String>,
}

enum Tok {
    Obj(Obj),
    Kw(String),
}

fn tokens(data: &[u8]) -> Vec<Tok> {
    let mut l = Lex { d: data, p: 0 };
    let mut out = Vec::new();
    while out.len() < MAX_TOKENS {
        l.skip_ws();
        let Some(b) = l.peek() else { break };
        let before = l.p;
        if b.is_ascii_alphabetic() {
            out.push(Tok::Kw(String::from_utf8_lossy(l.word()).into_owned()));
        } else if let Some(o) = l.obj(0) {
            out.push(Tok::Obj(o));
        } else if l.p == before {
            l.p += 1;
        }
    }
    out
}

fn code(s: &[u8]) -> Option<(usize, u32)> {
    if s.is_empty() || s.len() > 4 {
        return None;
    }
    Some((s.len(), s.iter().fold(0u32, |a, b| (a << 8) | u32::from(*b))))
}

impl CMap {
    pub(super) fn identity(vertical: bool) -> CMap {
        CMap { identity: true, vertical, ..CMap::default() }
    }

    /// Reads a CMap program (embedded CMap or ToUnicode).
    pub(super) fn parse(data: &[u8]) -> CMap {
        let mut m = CMap::default();
        let toks = tokens(data);
        let mut i = 0;
        while i < toks.len() {
            let Some(Tok::Kw(kw)) = toks.get(i) else {
                // `/WMode 1 def`
                if let (Some(Tok::Obj(Obj::Name(n))), Some(Tok::Obj(Obj::Num(v)))) = (toks.get(i), toks.get(i + 1))
                    && n == "WMode"
                {
                    m.vertical = *v == 1.0;
                }
                i += 1;
                continue;
            };
            i += 1;
            let end =
                |from: usize, what: &str| toks.iter().skip(from).position(|t| matches!(t, Tok::Kw(k) if k == what)).map_or(toks.len(), |p| from + p);
            match kw.as_str() {
                "begincodespacerange" => {
                    let e = end(i, "endcodespacerange");
                    for p in toks.get(i..e).unwrap_or_default().chunks(2) {
                        if let [Tok::Obj(Obj::Str(a)), Tok::Obj(Obj::Str(b))] = p
                            && let (Some((n, lo)), Some((_, hi))) = (code(a), code(b))
                            && m.space.len() < 1000
                        {
                            m.space.push((n, lo, hi));
                        }
                    }
                    i = e;
                }
                "begincidrange" | "beginbfrange" => {
                    let bf = kw == "beginbfrange";
                    let e = end(i, if bf { "endbfrange" } else { "endcidrange" });
                    for p in toks.get(i..e).unwrap_or_default().chunks(3) {
                        let [Tok::Obj(Obj::Str(a)), Tok::Obj(Obj::Str(b)), Tok::Obj(third)] = p else { continue };
                        let (Some((n, lo)), Some((_, hi))) = (code(a), code(b)) else { continue };
                        if hi < lo || hi - lo > MAX_SPAN || m.ranges.len() + m.unicode.len() > MAX_ENTRIES {
                            continue;
                        }
                        if bf {
                            match third {
                                Obj::Str(s) => {
                                    let base = utf16(s);
                                    for (k, c) in (lo..=hi).enumerate() {
                                        let mut t = base.clone();
                                        if let Some(last) = t.pop() {
                                            t.push(char::from_u32(last as u32 + k as u32).unwrap_or(last));
                                        }
                                        m.unicode.insert(c, t);
                                    }
                                }
                                Obj::Arr(list) => {
                                    for (c, s) in (lo..=hi).zip(list) {
                                        if let Obj::Str(s) = s {
                                            m.unicode.insert(c, utf16(s));
                                        }
                                    }
                                }
                                _ => {}
                            }
                        } else if let Obj::Num(c) = third {
                            m.ranges.push((n, lo, hi, *c as u32));
                        }
                    }
                    i = e;
                }
                "begincidchar" | "beginbfchar" => {
                    let bf = kw == "beginbfchar";
                    let e = end(i, if bf { "endbfchar" } else { "endcidchar" });
                    for p in toks.get(i..e).unwrap_or_default().chunks(2) {
                        let [Tok::Obj(Obj::Str(a)), Tok::Obj(second)] = p else { continue };
                        let Some((n, c)) = code(a) else { continue };
                        if m.ranges.len() + m.unicode.len() > MAX_ENTRIES {
                            continue;
                        }
                        match second {
                            Obj::Str(s) if bf => {
                                m.unicode.insert(c, utf16(s));
                            }
                            Obj::Num(v) if !bf => m.ranges.push((n, c, c, *v as u32)),
                            _ => {}
                        }
                    }
                    i = e;
                }
                _ => {}
            }
        }
        m
    }

    /// Splits `s` into character codes and the CID of each: `(code, cid, byte length)`.
    pub(super) fn decode(&self, s: &[u8]) -> Vec<(u32, u32, usize)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < s.len() && out.len() < s.len() {
            let n = if self.identity || self.space.is_empty() {
                2.min(s.len() - i)
            } else {
                // The first codespace length whose range holds the code; else the shortest.
                let mut pick = self.space.iter().map(|(n, _, _)| *n).min().unwrap_or(1);
                for len in 1..=4usize {
                    let Some(c) = s.get(i..i + len).and_then(code) else { break };
                    if self.space.iter().any(|(n, lo, hi)| *n == len && c.1 >= *lo && c.1 <= *hi) {
                        pick = len;
                        break;
                    }
                }
                pick.min(s.len() - i).max(1)
            };
            let c = s.get(i..i + n).and_then(code).map_or(0, |c| c.1);
            let cid = if self.identity { c } else { self.cid(n, c) };
            out.push((c, cid, n));
            i += n;
        }
        out
    }

    fn cid(&self, n: usize, c: u32) -> u32 {
        self.ranges.iter().find(|(rn, lo, hi, _)| *rn == n && c >= *lo && c <= *hi).map_or(0, |(_, lo, _, first)| first + (c - lo))
    }
}

fn utf16(s: &[u8]) -> String {
    let u: Vec<u16> = s.chunks(2).map(|p| p.iter().fold(0u16, |a, b| (a << 8) | u16::from(*b))).collect();
    String::from_utf16_lossy(&u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_embedded_cmap_maps_codes_to_cids() {
        let src = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap /WMode 1 def\n\
            1 begincodespacerange <00> <7F> endcodespacerange 1 begincodespacerange <8000> <FFFF> endcodespacerange\n\
            2 begincidrange <20> <7E> 1 <8140> <817E> 633 endcidrange 1 begincidchar <8200> 99 endcidchar endcmap";
        let m = CMap::parse(src);
        assert!(m.vertical);
        assert_eq!(m.decode(b"A\x81\x41"), vec![(0x41, 0x22, 1), (0x8141, 634, 2)]);
        assert_eq!(m.decode(&[0x82, 0x00]), vec![(0x8200, 99, 2)]);
        assert_eq!(CMap::identity(false).decode(&[0, 5, 1]), vec![(5, 5, 2), (1, 1, 1)]);
    }

    #[test]
    fn tounicode_ranges_and_chars() {
        let m = CMap::parse(b"1 beginbfchar <0003> <0020> endbfchar 1 beginbfrange <0010> <0012> <0041> endbfrange");
        assert_eq!(m.unicode.get(&3).map(String::as_str), Some(" "));
        assert_eq!(m.unicode.get(&0x11).map(String::as_str), Some("B"));
    }

    #[test]
    fn hostile_cmaps_do_not_panic() {
        let base = b"1 begincodespacerange <00> <FF> endcodespacerange 1 begincidrange <00> <FFFFFFFF> 0 endcidrange 1 beginbfrange <0000> <FFFF> [<41>] endbfrange begincidchar <".to_vec();
        for n in 0..base.len() {
            let m = CMap::parse(&base[..n]);
            let _ = m.decode(b"abc");
        }
        for i in 0..base.len() {
            let mut f = base.clone();
            f[i] ^= 0xFF;
            let _ = CMap::parse(&f).decode(&[1, 2, 3, 4]);
        }
    }
}
