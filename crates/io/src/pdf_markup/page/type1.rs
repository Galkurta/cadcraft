//! Minimal Adobe Type 1 font program decoder (PFA/PFB/raw `/FontFile` streams): eexec and charstring decryption, `/Subrs` + `/CharStrings` parsing, and a Type 1 charstring interpreter producing glyph outlines. Written from the public Type 1 Font Format specification; every limit below bounds hostile input.

use std::collections::HashMap;

const MAX_INPUT: usize = 8 << 20;
const MAX_ENTRIES: usize = 65_536;
const MAX_STEPS: usize = 20_000;
const MAX_PATH_OPS: usize = 20_000;
const MAX_DEPTH: usize = 10;
const MAX_STACK: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PathOp {
    Move(f64, f64),
    Line(f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

/// Glyph outline in glyph space (before FontMatrix): absolute coordinates, y up.
pub(super) struct Glyph {
    pub ops: Vec<PathOp>,
    pub advance: f64,
}

pub(super) struct Type1 {
    matrix: [f64; 6],
    encoding: Vec<Option<String>>,
    /// Decrypted private portion; `subrs`/`chars` hold still-encrypted byte ranges into it.
    private: Vec<u8>,
    len_iv: i64,
    subrs: HashMap<u32, (usize, usize)>,
    chars: HashMap<String, (usize, usize)>,
}

impl Type1 {
    /// `data` = decoded FontFile stream (PFA/raw or PFB). None if not a usable Type 1 font.
    pub(super) fn parse(data: &[u8]) -> Option<Type1> {
        if data.len() > MAX_INPUT {
            return None;
        }
        let flat;
        let data = if data.first() == Some(&0x80) {
            flat = unwrap_pfb(data);
            &flat[..]
        } else {
            data
        };
        let at = find(data, b"eexec")?;
        let clear = data.get(..at)?;
        let mut rest = data.get(at.checked_add(5)?..)?;
        while let Some((&b, tail)) = rest.split_first() {
            if !is_ws(b) {
                break;
            }
            rest = tail;
        }
        let hex = rest.len() >= 4 && rest.iter().take(4).all(u8::is_ascii_hexdigit);
        let cipher = if hex { unhex(rest) } else { rest.to_vec() };
        let private = decrypt(&cipher, 55665, 4);

        let mut font = Type1 {
            matrix: parse_matrix(clear).unwrap_or([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]),
            encoding: parse_encoding(clear),
            private,
            len_iv: 4,
            subrs: HashMap::new(),
            chars: HashMap::new(),
        };
        font.scan_private();
        if font.chars.is_empty() {
            return None;
        }
        Some(font)
    }

    pub(super) fn font_matrix(&self) -> [f64; 6] {
        self.matrix
    }

    pub(super) fn builtin_encoding(&self, code: u8) -> Option<&str> {
        self.encoding.get(usize::from(code))?.as_deref()
    }

    pub(super) fn glyph(&self, name: &str) -> Option<Glyph> {
        self.glyph_at(name, 0)
    }

    fn scan_private(&mut self) {
        let p = &self.private;
        let mut pos = 0;
        let mut win: [&[u8]; 4] = [b""; 4];
        let (mut subrs, mut chars) = (HashMap::new(), HashMap::new());
        let mut len_iv = 4;
        while let Some(t) = token(p, &mut pos) {
            if t == b"/lenIV" {
                if let Some(n) = token(p, &mut pos).and_then(int) {
                    len_iv = n;
                }
                continue;
            }
            if (t == b"RD" || t == b"-|")
                && let Some(n) = win[3..].first().copied().and_then(int)
            {
                let Ok(n) = usize::try_from(n) else { break };
                let start = pos.saturating_add(1);
                let Some(end) = start.checked_add(n).filter(|&e| e <= p.len()) else {
                    break;
                };
                let key = win.get(2).copied().unwrap_or(b"");
                if win.get(1).copied() == Some(b"dup") {
                    if let Some(i) = int(key).and_then(|i| u32::try_from(i).ok())
                        && subrs.len() < MAX_ENTRIES
                    {
                        subrs.insert(i, (start, end));
                    }
                } else if let Some(name) = key.strip_prefix(b"/")
                    && chars.len() < MAX_ENTRIES
                {
                    chars.insert(String::from_utf8_lossy(name).into_owned(), (start, end));
                }
                pos = end;
                win = [b""; 4];
                continue;
            }
            win = [win[1], win[2], win[3], t];
        }
        self.subrs = subrs;
        self.chars = chars;
        self.len_iv = len_iv;
    }

    /// Decrypted charstring bytes for a stored range.
    fn cs(&self, (s, e): (usize, usize)) -> Vec<u8> {
        let raw = self.private.get(s..e).unwrap_or(&[]);
        match usize::try_from(self.len_iv) {
            Ok(skip) => decrypt(raw, 4330, skip),
            Err(_) => raw.to_vec(),
        }
    }

    fn glyph_at(&self, name: &str, seac_depth: u8) -> Option<Glyph> {
        let code = self.cs(*self.chars.get(name)?);
        let mut st = State::new(seac_depth);
        self.exec(&code, &mut st, 0);
        st.close();
        Some(Glyph { ops: st.ops, advance: st.wx })
    }

    /// Runs one charstring; returns true when the glyph is finished (endchar/seac/limit hit).
    fn exec(&self, code: &[u8], st: &mut State, depth: usize) -> bool {
        let mut i = 0;
        while let Some(&b) = code.get(i) {
            i += 1;
            st.steps += 1;
            if st.steps > MAX_STEPS || st.full() {
                return true;
            }
            match b {
                32..=246 => st.num(f64::from(b) - 139.0),
                247..=254 => {
                    let Some(&w) = code.get(i) else { return true };
                    i += 1;
                    let w = f64::from(w);
                    st.num(if b <= 250 { (f64::from(b) - 247.0) * 256.0 + w + 108.0 } else { -(f64::from(b) - 251.0) * 256.0 - w - 108.0 });
                }
                255 => {
                    let Some(s) = i.checked_add(4).and_then(|e| code.get(i..e)) else {
                        return true;
                    };
                    let Ok(a) = <[u8; 4]>::try_from(s) else { return true };
                    i += 4;
                    st.num(f64::from(i32::from_be_bytes(a)));
                }
                1 | 3 => st.stack.clear(),
                4 => st.move_to(0.0, st.arg(0)),
                5 => st.line_to(st.arg(0), st.arg(1)),
                6 => st.line_to(st.arg(0), 0.0),
                7 => st.line_to(0.0, st.arg(0)),
                8 => st.curve_rel([st.arg(0), st.arg(1), st.arg(2), st.arg(3), st.arg(4), st.arg(5)]),
                9 => {
                    st.close();
                    st.stack.clear();
                }
                10 => {
                    let Some(idx) = st.stack.pop() else { continue };
                    if depth >= MAX_DEPTH {
                        return true;
                    }
                    let sub = u32::try_from(idx as i64).ok().and_then(|k| self.subrs.get(&k));
                    if let Some(&r) = sub
                        && self.exec(&self.cs(r), st, depth + 1)
                    {
                        return true;
                    }
                }
                11 => return false,
                13 => {
                    st.sbx = st.arg(0);
                    st.wx = st.arg(1);
                    (st.x, st.y) = (st.sbx, 0.0);
                    st.stack.clear();
                }
                14 => {
                    st.close();
                    return true;
                }
                21 => st.move_to(st.arg(0), st.arg(1)),
                22 => st.move_to(st.arg(0), 0.0),
                30 => st.curve_rel([0.0, st.arg(0), st.arg(1), st.arg(2), st.arg(3), 0.0]),
                31 => st.curve_rel([st.arg(0), 0.0, st.arg(1), st.arg(2), 0.0, st.arg(3)]),
                12 => {
                    let Some(&e) = code.get(i) else { return true };
                    i += 1;
                    match e {
                        0..=2 => st.stack.clear(),
                        6 => {
                            self.seac(st);
                            return true;
                        }
                        7 => {
                            st.sbx = st.arg(0);
                            st.wx = st.arg(2);
                            (st.x, st.y) = (st.sbx, st.arg(1));
                            st.stack.clear();
                        }
                        12 => {
                            let (b, a) = (st.stack.pop(), st.stack.pop());
                            let (a, b) = (a.unwrap_or(0.0), b.unwrap_or(1.0));
                            st.num(if b == 0.0 { 0.0 } else { a / b });
                        }
                        16 => st.othersubr(),
                        17 => {
                            if let Some(v) = st.ps.pop() {
                                st.num(v);
                            }
                        }
                        33 => {
                            (st.x, st.y) = (st.arg(0), st.arg(1));
                            st.stack.clear();
                        }
                        _ => st.stack.clear(),
                    }
                }
                _ => st.stack.clear(),
            }
        }
        false
    }

    /// Composes base + accent glyphs (StandardEncoding names); the accent is shifted by `sbx + adx - asb`, `ady`.
    fn seac(&self, st: &mut State) {
        st.close();
        if st.seac_depth > 0 {
            return;
        }
        let (asb, adx, ady) = (st.arg(0), st.arg(1), st.arg(2));
        let part = |v: f64| {
            let name = std_name(u8::try_from(v as i64).ok()?)?;
            self.glyph_at(name, 1)
        };
        if let Some(g) = part(st.arg(3)) {
            st.extend(g.ops.into_iter());
        }
        if let Some(g) = part(st.arg(4)) {
            let (dx, dy) = (st.sbx + adx - asb, ady);
            st.extend(g.ops.into_iter().map(|op| shift(op, dx, dy)));
        }
        st.stack.clear();
    }
}

enum Flex {
    Off,
    On,
}

struct State {
    ops: Vec<PathOp>,
    stack: Vec<f64>,
    ps: Vec<f64>,
    x: f64,
    y: f64,
    sbx: f64,
    wx: f64,
    flex: Flex,
    flex_pts: Vec<(f64, f64)>,
    open: bool,
    steps: usize,
    seac_depth: u8,
}

impl State {
    fn new(seac_depth: u8) -> State {
        State {
            ops: Vec::new(),
            stack: Vec::new(),
            ps: Vec::new(),
            x: 0.0,
            y: 0.0,
            sbx: 0.0,
            wx: 0.0,
            flex: Flex::Off,
            flex_pts: Vec::new(),
            open: false,
            steps: 0,
            seac_depth,
        }
    }

    fn arg(&self, i: usize) -> f64 {
        self.stack.get(i).copied().unwrap_or(0.0)
    }

    fn full(&self) -> bool {
        self.ops.len() >= MAX_PATH_OPS
    }

    fn num(&mut self, v: f64) {
        if self.stack.len() < MAX_STACK {
            self.stack.push(v);
        }
    }

    fn push(&mut self, op: PathOp) {
        if !self.full() {
            self.ops.push(op);
        }
    }

    fn extend(&mut self, ops: impl Iterator<Item = PathOp>) {
        for op in ops {
            self.push(op);
        }
    }

    fn close(&mut self) {
        if self.open {
            self.push(PathOp::Close);
            self.open = false;
        }
    }

    fn ensure_open(&mut self) {
        if !self.open {
            self.push(PathOp::Move(self.x, self.y));
            self.open = true;
        }
    }

    fn move_to(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        if matches!(self.flex, Flex::Off) {
            self.close();
            self.push(PathOp::Move(self.x, self.y));
            self.open = true;
        }
        self.stack.clear();
    }

    fn line_to(&mut self, dx: f64, dy: f64) {
        self.ensure_open();
        self.x += dx;
        self.y += dy;
        self.push(PathOp::Line(self.x, self.y));
        self.stack.clear();
    }

    /// Cubic from relative deltas `[dx1 dy1 dx2 dy2 dx3 dy3]`.
    fn curve_rel(&mut self, d: [f64; 6]) {
        self.ensure_open();
        let (x1, y1) = (self.x + d[0], self.y + d[1]);
        let (x2, y2) = (x1 + d[2], y1 + d[3]);
        let (x3, y3) = (x2 + d[4], y2 + d[5]);
        self.push(PathOp::Cubic(x1, y1, x2, y2, x3, y3));
        (self.x, self.y) = (x3, y3);
        self.stack.clear();
    }

    /// `arg1 .. argn n othersubr# callothersubr`: flex (0-2), hint replacement (3), others just move args.
    fn othersubr(&mut self) {
        let (Some(no), Some(n)) = (self.stack.pop(), self.stack.pop()) else {
            self.stack.clear();
            return;
        };
        let n = (n as i64).clamp(0, self.stack.len() as i64) as usize;
        let args = self.stack.split_off(self.stack.len() - n);
        match no as i64 {
            0 => {
                // args: flex height, x, y. Points 1..=6 are two curves; point 0 is the reference.
                if self.flex_pts.len() >= 7 {
                    let p = |i: usize| self.flex_pts.get(i).copied().unwrap_or((self.x, self.y));
                    let (a, b, c, d, e, f) = (p(1), p(2), p(3), p(4), p(5), p(6));
                    self.ensure_open();
                    self.push(PathOp::Cubic(a.0, a.1, b.0, b.1, c.0, c.1));
                    self.push(PathOp::Cubic(d.0, d.1, e.0, e.1, f.0, f.1));
                    (self.x, self.y) = f;
                }
                self.flex = Flex::Off;
                self.flex_pts.clear();
                let g = |i: usize| args.get(i).copied().unwrap_or(0.0);
                self.ps.clear();
                self.ps.push(g(2));
                self.ps.push(g(1));
            }
            1 => {
                self.flex = Flex::On;
                self.flex_pts.clear();
            }
            2 => {
                if self.flex_pts.len() < 8 {
                    self.flex_pts.push((self.x, self.y));
                }
            }
            3 => {
                self.ps.clear();
                self.ps.push(args.first().copied().unwrap_or(3.0));
            }
            _ => {
                self.ps.clear();
                self.ps.extend(args.iter().rev());
            }
        }
    }
}

fn shift(op: PathOp, dx: f64, dy: f64) -> PathOp {
    match op {
        PathOp::Move(x, y) => PathOp::Move(x + dx, y + dy),
        PathOp::Line(x, y) => PathOp::Line(x + dx, y + dy),
        PathOp::Cubic(a, b, c, d, e, f) => PathOp::Cubic(a + dx, b + dy, c + dx, d + dy, e + dx, f + dy),
        PathOp::Close => PathOp::Close,
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(b: u8) -> bool {
    matches!(b, b'[' | b']' | b'{' | b'}' | b'(' | b')' | b'<' | b'>' | b'/' | b'%')
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn int(t: &[u8]) -> Option<i64> {
    std::str::from_utf8(t).ok()?.parse().ok()
}

/// PostScript-ish tokenizer: names keep their `/`, brackets are single tokens, `%` comments are skipped.
fn token<'a>(d: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    loop {
        let b = *d.get(*pos)?;
        if is_ws(b) {
            *pos += 1;
        } else if b == b'%' {
            while d.get(*pos).is_some_and(|&c| c != b'\n' && c != b'\r') {
                *pos += 1;
            }
        } else {
            break;
        }
    }
    let start = *pos;
    let b = *d.get(start)?;
    *pos += 1;
    if !matches!(b, b'[' | b']' | b'{' | b'}' | b'(' | b')' | b'<' | b'>') {
        while d.get(*pos).is_some_and(|&c| !is_ws(c) && !is_delim(c)) {
            *pos += 1;
        }
    }
    d.get(start..*pos)
}

fn decrypt(data: &[u8], mut r: u16, skip: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().saturating_sub(skip));
    for (i, &c) in data.iter().enumerate() {
        let p = c ^ (r >> 8) as u8;
        r = u16::from(c).wrapping_add(r).wrapping_mul(52845).wrapping_add(22719);
        if i >= skip {
            out.push(p);
        }
    }
    out
}

fn unhex(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len() / 2);
    let mut hi: Option<u8> = None;
    for &c in d {
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ if is_ws(c) => continue,
            _ => break,
        };
        match hi.take() {
            Some(h) => out.push(h << 4 | v),
            None => hi = Some(v),
        }
    }
    out
}

/// Concatenates the ASCII/binary segments of a PFB; stops at the EOF segment or truncation.
fn unwrap_pfb(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while d.get(pos) == Some(&0x80) {
        let Some(&ty) = d.get(pos + 1) else { break };
        if ty == 3 {
            break;
        }
        let Some(Ok(len)) = d.get(pos + 2..pos + 6).map(<[u8; 4]>::try_from) else {
            break;
        };
        let start = pos + 6;
        let end = start.saturating_add(u32::from_le_bytes(len) as usize).min(d.len());
        out.extend_from_slice(d.get(start..end).unwrap_or(&[]));
        pos = end;
    }
    out
}

fn parse_matrix(clear: &[u8]) -> Option<[f64; 6]> {
    let mut pos = find(clear, b"/FontMatrix")? + 11;
    let open = token(clear, &mut pos)?;
    if open != b"[" && open != b"{" {
        return None;
    }
    let mut m = [0.0; 6];
    for v in &mut m {
        let t = token(clear, &mut pos)?;
        *v = std::str::from_utf8(t).ok()?.parse::<f64>().ok().filter(|v| v.is_finite())?;
    }
    Some(m)
}

fn parse_encoding(clear: &[u8]) -> Vec<Option<String>> {
    let mut enc = vec![None; 256];
    let Some(at) = find(clear, b"/Encoding") else { return enc };
    let mut pos = at + 9;
    if token(clear, &mut pos) == Some(b"StandardEncoding") {
        for (code, slot) in (0..=255u8).zip(enc.iter_mut()) {
            *slot = std_name(code).map(str::to_owned);
        }
        return enc;
    }
    for _ in 0..4096 {
        match token(clear, &mut pos) {
            Some(b"dup") => {
                let code = token(clear, &mut pos).and_then(int).and_then(|c| usize::try_from(c).ok());
                let name = token(clear, &mut pos).and_then(|n| n.strip_prefix(b"/"));
                if let (Some(slot), Some(name)) = (code.and_then(|c| enc.get_mut(c)), name) {
                    *slot = Some(String::from_utf8_lossy(name).into_owned());
                }
            }
            Some(b"def" | b"readonly") | None => break,
            Some(_) => {}
        }
    }
    enc
}

/// Adobe StandardEncoding as runs of (first code, names); gaps are undefined.
const STANDARD: &[(u8, &str)] = &[
    (
        32,
        "space exclam quotedbl numbersign dollar percent ampersand quoteright parenleft parenright asterisk plus comma hyphen period slash zero one two three four five six seven eight nine colon semicolon less equal greater question at A B C D E F G H I J K L M N O P Q R S T U V W X Y Z bracketleft backslash bracketright asciicircum underscore quoteleft a b c d e f g h i j k l m n o p q r s t u v w x y z braceleft bar braceright asciitilde",
    ),
    (161, "exclamdown cent sterling fraction yen florin section currency quotesingle quotedblleft guillemotleft guilsinglleft guilsinglright fi fl"),
    (177, "endash dagger daggerdbl periodcentered"),
    (182, "paragraph bullet quotesinglbase quotedblbase quotedblright guillemotright ellipsis perthousand"),
    (191, "questiondown"),
    (193, "grave acute circumflex tilde macron breve dotaccent dieresis"),
    (202, "ring cedilla"),
    (205, "hungarumlaut ogonek caron emdash"),
    (225, "AE"),
    (227, "ordfeminine"),
    (232, "Lslash Oslash OE ordmasculine"),
    (241, "ae"),
    (245, "dotlessi"),
    (248, "lslash oslash oe germandbls"),
];

fn std_name(code: u8) -> Option<&'static str> {
    STANDARD.iter().find_map(|&(start, names)| names.split(' ').nth(usize::from(code.checked_sub(start)?)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
pub(super) mod tests {
    use super::PathOp::{Close, Cubic, Line, Move};
    use super::*;

    fn encrypt(plain: &[u8], mut r: u16, lead: &[u8]) -> Vec<u8> {
        lead.iter()
            .chain(plain)
            .map(|&p| {
                let c = p ^ (r >> 8) as u8;
                r = u16::from(c).wrapping_add(r).wrapping_mul(52845).wrapping_add(22719);
                c
            })
            .collect()
    }

    fn num(v: i32) -> Vec<u8> {
        match v {
            -107..=107 => vec![(v + 139) as u8],
            108..=1131 => vec![(247 + (v - 108) / 256) as u8, ((v - 108) % 256) as u8],
            -1131..=-108 => vec![(251 + (-v - 108) / 256) as u8, ((-v - 108) % 256) as u8],
            _ => [vec![255], v.to_be_bytes().to_vec()].concat(),
        }
    }

    fn asm(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .flat_map(|t| match t.parse::<i32>() {
                Ok(v) => num(v),
                Err(_) => match t {
                    "hstem" => vec![1],
                    "rlineto" => vec![5],
                    "hlineto" => vec![6],
                    "vlineto" => vec![7],
                    "rrcurveto" => vec![8],
                    "closepath" => vec![9],
                    "callsubr" => vec![10],
                    "return" => vec![11],
                    "hsbw" => vec![13],
                    "endchar" => vec![14],
                    "rmoveto" => vec![21],
                    "vhcurveto" => vec![30],
                    "hvcurveto" => vec![31],
                    "seac" => vec![12, 6],
                    "div" => vec![12, 12],
                    "callothersubr" => vec![12, 16],
                    "pop" => vec![12, 17],
                    "setcurrentpoint" => vec![12, 33],
                    _ => panic!("bad op {t}"),
                },
            })
            .collect()
    }

    fn private_text() -> Vec<u8> {
        let subrs = [
            "3 0 callothersubr pop pop setcurrentpoint return",
            "0 1 callothersubr return",
            "0 2 callothersubr return",
            "return",
            "50 hlineto 50 vlineto return",
            "5 callsubr return",
        ];
        let glyphs = [
            (".notdef", "0 250 hsbw endchar"),
            ("A", "50 600 hsbw 0 0 rmoveto 100 hlineto 100 vlineto -100 hlineto closepath endchar"),
            ("sub", "0 500 hsbw 10 20 rmoveto 4 callsubr closepath endchar"),
            ("curve", "0 500 hsbw 0 0 rmoveto 10 20 30 40 50 60 rrcurveto 10 20 30 40 vhcurveto 10 20 30 40 hvcurveto closepath endchar"),
            (
                "flex",
                "0 500 hsbw 0 0 rmoveto 1 callsubr 50 0 rmoveto 2 callsubr -40 0 rmoveto 2 callsubr 10 5 rmoveto 2 callsubr \
                 10 0 rmoveto 2 callsubr 10 0 rmoveto 2 callsubr 10 -5 rmoveto 2 callsubr 10 0 rmoveto 2 callsubr \
                 50 60 0 0 callsubr 40 hlineto closepath endchar",
            ),
            ("acute", "20 300 hsbw 0 0 rmoveto 10 hlineto 10 vlineto closepath endchar"),
            ("Aacute", "50 600 hsbw 20 100 150 65 194 seac"),
            ("loop", "0 500 hsbw 4 4 callsubr 5 callsubr 1000 2 div 3 hstem endchar"),
        ];
        let mut out = b"dup /Private 8 dict dup begin\n/RD {string currentfile exch readstring pop} executeonly def\n\
            /ND {noaccess def} executeonly def\n/NP {noaccess put} executeonly def\n/Subrs 6 array\n"
            .to_vec();
        for (i, s) in subrs.iter().enumerate() {
            let cs = encrypt(&asm(s), 4330, &[0, 0, 0, 0]);
            out.extend(format!("dup {i} {} RD ", cs.len()).bytes());
            out.extend(cs);
            out.extend(b" NP\n");
        }
        out.extend(b"ND\n2 index /CharStrings 8 dict dup begin\n");
        for (name, s) in glyphs {
            let cs = encrypt(&asm(s), 4330, &[1, 2, 3, 4]);
            out.extend(format!("/{name} {} -| ", cs.len()).bytes());
            out.extend(cs);
            out.extend(b" |-\n");
        }
        out.extend(b"end end readonly put noaccess put\ndup /FontName get exch definefont pop\nmark currentfile closefile\n");
        out
    }

    fn clear_text(std_enc: bool) -> Vec<u8> {
        let enc = if std_enc {
            "/Encoding StandardEncoding def\n"
        } else {
            "/Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\ndup 65 /A put\ndup 66 /sub put\nreadonly def\n"
        };
        format!("%!PS-AdobeFont-1.0: Test 1.0\n/FontMatrix [0.002 0 0 0.002 0 0] readonly def\n{enc}currentdict end\ncurrentfile eexec\n")
            .into_bytes()
    }

    fn trailer() -> Vec<u8> {
        let mut t = vec![b'\n'];
        for _ in 0..8 {
            t.extend([b'0'; 64]);
            t.push(b'\n');
        }
        t.extend(b"cleartomark\n");
        t
    }

    pub(in crate::pdf_markup::page) fn raw_font(std_enc: bool, hex: bool) -> Vec<u8> {
        let mut out = clear_text(std_enc);
        let enc = encrypt(&private_text(), 55665, b"xyzw");
        if hex {
            for line in enc.chunks(32) {
                out.extend(line.iter().flat_map(|b| format!("{b:02X}").into_bytes()));
                out.push(b'\n');
            }
        } else {
            out.extend(enc);
        }
        out.extend(trailer());
        out
    }

    fn pfb(std_enc: bool) -> Vec<u8> {
        let seg = |ty: u8, d: &[u8]| [vec![0x80, ty], (d.len() as u32).to_le_bytes().to_vec(), d.to_vec()].concat();
        [seg(1, &clear_text(std_enc)), seg(2, &encrypt(&private_text(), 55665, &[1, 2, 3, 4])), seg(1, &trailer()), vec![0x80, 3]].concat()
    }

    fn check(f: &Type1) {
        assert_eq!(f.font_matrix(), [0.002, 0.0, 0.0, 0.002, 0.0, 0.0]);
        assert_eq!(f.builtin_encoding(65), Some("A"));
        assert_eq!(f.builtin_encoding(66), Some("sub"));
        assert_eq!(f.builtin_encoding(67), None);

        let g = f.glyph("A").unwrap();
        assert_eq!(g.advance, 600.0);
        assert_eq!(g.ops, [Move(50.0, 0.0), Line(150.0, 0.0), Line(150.0, 100.0), Line(50.0, 100.0), Close]);

        let g = f.glyph("sub").unwrap();
        assert_eq!(g.advance, 500.0);
        assert_eq!(g.ops, [Move(10.0, 20.0), Line(60.0, 20.0), Line(60.0, 70.0), Close]);

        let g = f.glyph("curve").unwrap();
        assert_eq!(
            g.ops,
            [
                Move(0.0, 0.0),
                Cubic(10.0, 20.0, 40.0, 60.0, 90.0, 120.0),
                Cubic(90.0, 130.0, 110.0, 160.0, 150.0, 160.0),
                Cubic(160.0, 160.0, 180.0, 190.0, 180.0, 230.0),
                Close
            ]
        );

        let g = f.glyph("flex").unwrap();
        assert_eq!(g.ops, [Move(0.0, 0.0), Cubic(10.0, 0.0, 20.0, 5.0, 30.0, 5.0), Cubic(40.0, 5.0, 50.0, 0.0, 60.0, 0.0), Line(100.0, 0.0), Close]);

        let g = f.glyph("Aacute").unwrap();
        assert_eq!(g.advance, 600.0);
        assert_eq!(
            g.ops,
            [
                Move(50.0, 0.0),
                Line(150.0, 0.0),
                Line(150.0, 100.0),
                Line(50.0, 100.0),
                Close,
                Move(150.0, 150.0),
                Line(160.0, 150.0),
                Line(160.0, 160.0),
                Close
            ]
        );

        // Self-recursive subr hits the depth cap but still yields a glyph.
        assert!(f.glyph("loop").is_some());
        assert!(f.glyph("missing").is_none());
    }

    #[test]
    fn raw_binary() {
        check(&Type1::parse(&raw_font(false, false)).unwrap());
    }

    #[test]
    fn hex_eexec() {
        check(&Type1::parse(&raw_font(false, true)).unwrap());
    }

    #[test]
    fn pfb_framing() {
        check(&Type1::parse(&pfb(false)).unwrap());
    }

    #[test]
    fn standard_encoding() {
        let f = Type1::parse(&raw_font(true, false)).unwrap();
        assert_eq!(f.builtin_encoding(65), Some("A"));
        assert_eq!(f.builtin_encoding(251), Some("germandbls"));
        assert_eq!(f.builtin_encoding(194), Some("acute"));
        assert_eq!(f.builtin_encoding(127), None);
        assert_eq!(f.builtin_encoding(255), None);
        assert_eq!(f.builtin_encoding(126), Some("asciitilde"));
        assert_eq!(f.builtin_encoding(232), Some("Lslash"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Type1::parse(b"").is_none());
        assert!(Type1::parse(b"hello world").is_none());
        assert!(Type1::parse(&[0x80, 1, 255, 255, 255, 255]).is_none());
    }

    fn touch(d: &[u8]) {
        if let Some(f) = Type1::parse(d) {
            let _ = (f.font_matrix(), f.builtin_encoding(65));
            for n in ["A", "sub", "curve", "flex", "Aacute", "loop", ".notdef", "acute"] {
                let _ = f.glyph(n);
            }
        }
    }

    #[test]
    fn hostile_truncation_and_flips() {
        let start = std::time::Instant::now();
        for font in [raw_font(false, false), raw_font(false, true), pfb(false)] {
            for len in 0..font.len() {
                touch(&font[..len]);
            }
            let mut d = font.clone();
            for i in 0..d.len() {
                for x in [0xFFu8, 0x01, 0x80] {
                    let orig = d[i];
                    d[i] ^= x;
                    touch(&d);
                    d[i] = orig;
                }
            }
        }
        assert!(start.elapsed().as_secs() < 60);
    }
}
