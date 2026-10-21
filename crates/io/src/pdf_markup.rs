//! A minimal PDF reader for review markups: the annotations of one page as markup JSON
//! (normalized page coordinates `[u, v]`, origin top left, v down), so MARKUPIMPORT can turn a
//! reviewer's comments into drawing objects, and the decoded content of a page for the plotting
//! interpreter.
//!
//! Our own parser (clean-room, from the public PDF file structure): classic xref tables and xref
//! streams (with predictors), object streams, the filters Flate, LZW, ASCIIHex, ASCII85 and
//! RunLength (chains included), the standard security handler (RC4 and AES, user password; see
//! `crypt`), a damaged or missing xref rebuilt by scanning for `N G obj`, the page tree with
//! inherited `/MediaBox`, `/CropBox` and `/Rotate`, and the annotation types of a review:
//! FreeText, Text (sticky note), Square, Circle, Line, PolyLine, Polygon (a cloud with `/BE /S /C`
//! or `/IT /PolygonCloud`) and Ink; `/RC` rich text is read as plain text. Drawing a page is the
//! job of the child module `page`, which reads resources through this parser. The file is hostile input: every count, depth, array and inflated size is capped,
//! objects are read through `get` and `Option`, and a damaged file is an error or a page with
//! fewer markups, never a panic.
//!
//! Not read (ponytail: add when a real file needs it): the owner password, public-key handlers,
//! and appearance streams (the image filters stop `decode`; `page` decodes DCT itself).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use crate::{IoError, Result};

mod crypt;
mod filters;
pub(crate) mod page;
mod rich;

use crypt::Crypt;

const MAX_DEPTH: usize = 32;
const MAX_ARRAY: usize = 100_000;
/// Most object loads (file objects and object-stream members) in one read.
const MAX_LOADS: usize = 200_000;
/// Largest stream after inflating, and the most inflated bytes in one read.
const MAX_INFLATE: usize = 32 << 20;
const MAX_INFLATE_TOTAL: usize = 128 << 20;
const MAX_XREF_SECTIONS: usize = 256;
const MAX_XREF_ENTRIES: usize = 2_000_000;
const MAX_PAGE_NODES: usize = 100_000;
const MAX_ANNOTS: usize = 10_000;
/// Most coordinates in one annotation.
const MAX_COORDS: usize = 20_000;
/// A sticky note's text height, in points.
const NOTE_PT: f64 = 12.0;

type Dict = BTreeMap<String, Obj>;

#[derive(Clone, Debug)]
enum Obj {
    Null,
    Bool(bool),
    Num(f64),
    Name(String),
    Str(Vec<u8>),
    Arr(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
    /// A stream: its dictionary, the byte range of its raw data in the file and its object id
    /// (number, generation; the key of its encryption).
    Stream(Dict, usize, usize, (u32, u16)),
}

/// The markups of one page, in the page's displayed (rotated) size in points.
#[derive(Clone, Debug)]
pub struct PdfMarkups {
    pub width: f64,
    pub height: f64,
    pub page_count: usize,
    pub markups: Vec<Value>,
    /// What was skipped (annotation types that are not review markups).
    pub notes: Vec<String>,
}

/// The size in pixels of a PNG or JPEG image, from its header alone.
pub fn image_size(b: &[u8]) -> Option<(u32, u32)> {
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i.checked_add(4)?)?.try_into().ok()?));
    let be16 = |i: usize| Some(u32::from(u16::from_be_bytes(b.get(i..i.checked_add(2)?)?.try_into().ok()?)));
    if b.starts_with(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]) {
        return Some((be32(16)?, be32(20)?)).filter(|(w, h)| *w > 0 && *h > 0);
    }
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2usize;
    while i.checked_add(4)? <= b.len() {
        if *b.get(i)? != 0xFF {
            i += 1;
            continue;
        }
        let m = *b.get(i + 1)?;
        if m == 0xFF {
            i += 1;
            continue;
        }
        if m == 0x01 || m == 0xD8 || (0xD0..=0xD7).contains(&m) {
            i += 2;
            continue;
        }
        // Start-of-frame markers carry the size (not DHT, JPG, DAC).
        if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            return Some((be16(i + 7)?, be16(i + 5)?)).filter(|(w, h)| *w > 0 && *h > 0);
        }
        i = i.checked_add(2)?.checked_add(be16(i + 2)? as usize)?;
    }
    None
}

/// The annotations of page `page` (1-based) of a PDF file as markup JSON. `password` is the user
/// password of an encrypted file (empty for none).
pub fn pdf_markups(bytes: &[u8], page: usize, password: &str) -> Result<PdfMarkups> {
    let (mut pdf, pdict, total) = locate(bytes, page, password)?;
    let pg = pdf.page_geometry(&pdict).ok_or_else(|| IoError::Format("the page has no usable /MediaBox".into()))?;
    let mut markups = Vec::new();
    let mut notes = Vec::new();
    if let Obj::Arr(annots) = pdf.val(&pdict, "Annots") {
        for r in annots.iter().take(MAX_ANNOTS) {
            if let Obj::Dict(a) = pdf.resolve(r) {
                pdf.annot(&a, &pg, &mut markups, &mut notes);
            }
        }
    }
    notes.sort();
    notes.dedup();
    Ok(PdfMarkups { width: pg.dw, height: pg.dh, page_count: total, markups, notes })
}

/// One page's drawing instructions and geometry.
#[derive(Clone, Debug)]
pub struct PdfPageContent {
    pub page_count: usize,
    /// The visible box in default user space: the `/CropBox` inside the `/MediaBox` (the
    /// `/MediaBox` when there is none): lower left corner and size, in points.
    pub x0: f64,
    pub y0: f64,
    pub w: f64,
    pub h: f64,
    /// The page's rotation: 0, 90, 180 or 270 degrees clockwise.
    pub rotate: i64,
    /// The page's content streams, decoded and joined by a line feed (a stream with an image
    /// filter or damaged data is left out).
    pub content: Vec<u8>,
}

/// The decoded content of page `page` (1-based); `password` as for [`pdf_markups`].
pub fn pdf_page_content(bytes: &[u8], page: usize, password: &str) -> Result<PdfPageContent> {
    let (mut pdf, pdict, page_count) = locate(bytes, page, password)?;
    let pg = pdf.page_geometry(&pdict).ok_or_else(|| IoError::Format("the page has no usable /MediaBox".into()))?;
    let content = page_streams(&mut pdf, &pdict);
    Ok(PdfPageContent { page_count, x0: pg.x0, y0: pg.y0, w: pg.w, h: pg.h, rotate: pg.rot, content })
}

/// A page's content streams, decoded and joined by a line feed.
fn page_streams(pdf: &mut Pdf, pdict: &Dict) -> Vec<u8> {
    let parts = match pdf.val(pdict, "Contents") {
        Obj::Arr(v) => v.into_iter().take(MAX_ANNOTS).collect(),
        Obj::Null => Vec::new(),
        one => vec![one],
    };
    let mut content = Vec::new();
    for p in &parts {
        if let Obj::Stream(d, a, b, id) = pdf.resolve(p)
            && let Some(data) = pdf.decode(&d, a, b, id)
        {
            if !content.is_empty() {
                content.push(b'\n');
            }
            content.extend(data);
        }
    }
    content
}

/// Opens the file and finds page `page` (with the inherited attributes merged in): through the
/// xref, else, when that is missing or leads nowhere, through a rebuilt one.
fn locate<'a>(bytes: &'a [u8], page: usize, password: &str) -> Result<(Pdf<'a>, Dict, usize)> {
    let damaged = || IoError::Format("the PDF file is damaged or uses a feature the markup reader does not support".into());
    let head = bytes.get(..bytes.len().min(1024)).unwrap_or_default();
    let start = find(head, b"%PDF-", 0).ok_or_else(|| IoError::Format("not a PDF file".into()))?;
    let data = bytes.get(start..).ok_or_else(damaged)?;
    if page == 0 {
        return Err(IoError::Format("pages are numbered from 1".into()));
    }
    let missing = |total: usize| IoError::Format(format!("page {page} does not exist: the file has {total} page(s)"));
    let mut pdf = Pdf::new(data);
    if pdf.load_xref().is_some() && pdf.init_crypt(password)? {
        match pdf.find_page(page) {
            Some((Some(d), total)) => return Ok((pdf, d, total)),
            Some((None, total)) => return Err(missing(total)),
            None => {}
        }
    }
    let mut pdf = Pdf::new(data);
    let objstms = pdf.rebuild_xref();
    if !pdf.init_crypt(password)? {
        return Err(damaged());
    }
    for s in objstms {
        pdf.load_objstm(s, true);
    }
    if !pdf.trailer.contains_key("Root") {
        // The catalog may sit in an object stream.
        let cat = pdf
            .cache
            .iter()
            .filter(|(_, o)| matches!(o, Obj::Dict(d) if matches!(d.get("Type"), Some(Obj::Name(t)) if t == "Catalog")))
            .map(|(n, _)| *n)
            .min();
        pdf.trailer.insert("Root".into(), Obj::Ref(cat.ok_or_else(damaged)?));
    }
    match pdf.find_page(page).ok_or_else(damaged)? {
        (Some(d), total) => Ok((pdf, d, total)),
        (None, total) => Err(missing(total)),
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

fn find(h: &[u8], n: &[u8], from: usize) -> Option<usize> {
    if n.is_empty() {
        return None;
    }
    h.get(from..)?.windows(n.len()).position(|w| w == n).map(|i| i + from)
}

struct Lex<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Lex<'a> {
    fn peek(&self) -> Option<u8> {
        self.d.get(self.p).copied()
    }
    fn starts(&self, s: &[u8]) -> bool {
        self.d.get(self.p..).is_some_and(|r| r.starts_with(s))
    }
    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if is_ws(b) {
                self.p += 1;
            } else if b == b'%' {
                while self.peek().is_some_and(|b| b != 10 && b != 13) {
                    self.p += 1;
                }
            } else {
                break;
            }
        }
    }
    fn word(&mut self) -> &'a [u8] {
        let s = self.p;
        while self.peek().is_some_and(|b| !is_ws(b) && !is_delim(b)) {
            self.p += 1;
        }
        self.d.get(s..self.p).unwrap_or_default()
    }
    fn uint(&mut self) -> Option<u64> {
        let s = self.p;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.p += 1;
        }
        std::str::from_utf8(self.d.get(s..self.p)?).ok()?.parse().ok()
    }
    fn name(&mut self) -> String {
        let mut out = Vec::new();
        while let Some(b) = self.peek() {
            if is_ws(b) || is_delim(b) {
                break;
            }
            self.p += 1;
            let hex = |i: usize| self.d.get(i).and_then(|c| (*c as char).to_digit(16));
            if b == b'#'
                && let (Some(h), Some(l)) = (hex(self.p), hex(self.p + 1))
            {
                out.push((h * 16 + l) as u8);
                self.p += 2;
            } else {
                out.push(b);
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    fn number(&mut self) -> Option<Obj> {
        let s = self.p;
        while self.peek().is_some_and(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.')) {
            self.p += 1;
        }
        let t = std::str::from_utf8(self.d.get(s..self.p)?).ok()?;
        if t.bytes().all(|b| b.is_ascii_digit())
            && let Ok(n) = t.parse::<u32>()
        {
            // `n g R` is a reference.
            let save = self.p;
            self.skip_ws();
            if self.uint().is_some() {
                self.skip_ws();
                if self.peek() == Some(b'R') && self.d.get(self.p + 1).is_none_or(|b| is_ws(*b) || is_delim(*b)) {
                    self.p += 1;
                    return Some(Obj::Ref(n));
                }
            }
            self.p = save;
        }
        t.parse::<f64>().ok().filter(|v| v.is_finite()).map(Obj::Num)
    }
    fn literal(&mut self) -> Option<Vec<u8>> {
        self.p += 1;
        let (mut out, mut depth) = (Vec::new(), 1usize);
        loop {
            let b = self.peek()?;
            self.p += 1;
            match b {
                b'(' => {
                    depth += 1;
                    out.push(b);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(out);
                    }
                    out.push(b);
                }
                b'\\' => {
                    let e = self.peek()?;
                    self.p += 1;
                    match e {
                        b'n' => out.push(10),
                        b'r' => out.push(13),
                        b't' => out.push(9),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'0'..=b'7' => {
                            let mut v = u32::from(e - b'0');
                            for _ in 0..2 {
                                match self.peek() {
                                    Some(d @ b'0'..=b'7') => {
                                        v = v * 8 + u32::from(d - b'0');
                                        self.p += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        13 => {
                            if self.peek() == Some(10) {
                                self.p += 1;
                            }
                        }
                        10 => {}
                        other => out.push(other),
                    }
                }
                _ => out.push(b),
            }
        }
    }
    fn hex_string(&mut self) -> Option<Vec<u8>> {
        self.p += 1;
        let (mut out, mut hi) = (Vec::new(), None);
        loop {
            let b = self.peek()?;
            self.p += 1;
            if b == b'>' {
                out.extend(hi.map(|h: u32| (h * 16) as u8));
                return Some(out);
            }
            if let Some(d) = (b as char).to_digit(16) {
                match hi.take() {
                    Some(h) => out.push((h * 16 + d) as u8),
                    None => hi = Some(d),
                }
            }
        }
    }
    fn obj(&mut self, depth: usize) -> Option<Obj> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_ws();
        match self.peek()? {
            b'/' => {
                self.p += 1;
                Some(Obj::Name(self.name()))
            }
            b'(' => self.literal().map(Obj::Str),
            b'<' if self.starts(b"<<") => {
                self.p += 2;
                let mut d = Dict::new();
                loop {
                    self.skip_ws();
                    if self.starts(b">>") {
                        self.p += 2;
                        return Some(Obj::Dict(d));
                    }
                    if self.peek()? != b'/' || d.len() >= MAX_ARRAY {
                        return None;
                    }
                    self.p += 1;
                    let k = self.name();
                    let v = self.obj(depth + 1)?;
                    d.insert(k, v);
                }
            }
            b'<' => self.hex_string().map(Obj::Str),
            b'[' => {
                self.p += 1;
                let mut v = Vec::new();
                loop {
                    self.skip_ws();
                    if self.peek()? == b']' {
                        self.p += 1;
                        return Some(Obj::Arr(v));
                    }
                    if v.len() >= MAX_ARRAY {
                        return None;
                    }
                    v.push(self.obj(depth + 1)?);
                }
            }
            b'0'..=b'9' | b'+' | b'-' | b'.' => self.number(),
            _ => match self.word() {
                b"true" => Some(Obj::Bool(true)),
                b"false" => Some(Obj::Bool(false)),
                b"null" => Some(Obj::Null),
                _ => None,
            },
        }
    }
}

/// The indirect object at byte `off`: its number, generation and value (a stream's data stays in
/// the file).
fn indirect(data: &[u8], off: usize) -> Option<(u32, u16, Obj)> {
    let mut l = Lex { d: data, p: off };
    l.skip_ws();
    let n = u32::try_from(l.uint()?).ok()?;
    l.skip_ws();
    let g = u16::try_from(l.uint()?).unwrap_or(0);
    l.skip_ws();
    if l.word() != b"obj" {
        return None;
    }
    let o = l.obj(0)?;
    let Obj::Dict(d) = o else { return Some((n, g, o)) };
    l.skip_ws();
    if !l.starts(b"stream") {
        return Some((n, g, Obj::Dict(d)));
    }
    l.p += 6;
    if l.starts(b"\r\n") {
        l.p += 2;
    } else if matches!(l.peek(), Some(10 | 13)) {
        l.p += 1;
    }
    let start = l.p;
    let declared = match d.get("Length") {
        Some(Obj::Num(n)) if *n >= 0.0 => Some(*n as usize),
        _ => None,
    };
    let end = declared
        .and_then(|len| start.checked_add(len))
        .filter(|end| {
            let mut t = Lex { d: data, p: *end };
            t.skip_ws();
            t.starts(b"endstream")
        })
        .or_else(|| {
            let e = find(data, b"endstream", start)?;
            let mut e2 = e;
            if e2 > start && data.get(e2 - 1) == Some(&10) {
                e2 -= 1;
            }
            if e2 > start && data.get(e2 - 1) == Some(&13) {
                e2 -= 1;
            }
            Some(e2)
        })?;
    Some((n, g, Obj::Stream(d, start, end, (n, g))))
}

#[derive(Clone, Copy)]
enum Entry {
    Free,
    At(usize),
    /// In the object stream with this number.
    In(u32),
}

struct Pdf<'a> {
    data: &'a [u8],
    xref: HashMap<u32, Entry>,
    trailer: Dict,
    cache: HashMap<u32, Obj>,
    objstms: HashSet<u32>,
    loads: usize,
    inflated: usize,
    crypt: Option<Crypt>,
}

/// A page's media box and rotation, with the transform to normalized displayed coordinates.
struct Page {
    x0: f64,
    y0: f64,
    w: f64,
    h: f64,
    rot: i64,
    /// Displayed size (swapped for a quarter turn).
    dw: f64,
    dh: f64,
}

impl Page {
    /// A point of default user space as `[u, v]`, origin top left of the displayed page.
    fn uv(&self, x: f64, y: f64) -> [f64; 2] {
        let (u, v) = ((x - self.x0) / self.w, (self.y0 + self.h - y) / self.h);
        match self.rot {
            90 => [1.0 - v, u],
            180 => [1.0 - u, 1.0 - v],
            270 => [v, 1.0 - u],
            _ => [u, v],
        }
    }
}

fn min_uv(pts: &[[f64; 2]]) -> [f64; 2] {
    pts.iter().fold([f64::INFINITY; 2], |m, p| [m[0].min(p[0]), m[1].min(p[1])])
}

fn text_markup(text: &str, at: [f64; 2], height: f64, color: Option<&str>) -> Value {
    let mut v = json!({ "kind": "text", "text": text, "at": at, "height": height });
    if let Some(c) = color {
        v["color"] = json!(c);
    }
    v
}

fn shape(kind: &str, pts: &[[f64; 2]], color: Option<&str>) -> Value {
    let mut v = json!({ "kind": kind, "points": pts });
    if let Some(c) = color {
        v["color"] = json!(c);
    }
    v
}

/// PDFDocEncoding's differences from Latin-1: 0x18-0x1F, then 0x80-0xA0 (0x9F is undefined).
const PDFDOC_18: [char; 8] = ['˘', 'ˇ', 'ˆ', '˙', '˝', '˛', '˚', '˜'];
const PDFDOC_80: [char; 33] = [
    '•', '†', '‡', '…', '—', '–', 'ƒ', '⁄', '‹', '›', '−', '‰', '„', '“', '”', '‘', '’', '‚', '™', 'ﬁ', 'ﬂ', 'Ł', 'Œ', 'Š', 'Ÿ', 'Ž', 'ı', 'ł', 'œ',
    'š', 'ž', '\u{FFFD}', '€',
];

fn pdfdoc(b: u8) -> char {
    let pick = |t: &[char], from: u8| t.get(usize::from(b - from)).copied().unwrap_or('\u{FFFD}');
    match b {
        0x18..=0x1F => pick(&PDFDOC_18, 0x18),
        0x80..=0xA0 => pick(&PDFDOC_80, 0x80),
        _ => char::from(b),
    }
}

/// PDF text string: UTF-16BE or UTF-8 with a byte order mark (language escapes dropped), else
/// PDFDocEncoding.
fn pdf_text(b: &[u8]) -> String {
    let s = if let Some(r) = b.strip_prefix(&[0xFE, 0xFF]) {
        let u: Vec<u16> = r.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
        drop_language(&String::from_utf16_lossy(&u))
    } else if let Some(r) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        drop_language(&String::from_utf8_lossy(r))
    } else {
        b.iter().map(|c| pdfdoc(*c)).collect()
    };
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Text between a pair of U+001B is a language tag, not text.
fn drop_language(s: &str) -> String {
    let mut tag = false;
    s.chars()
        .filter(|c| {
            if *c == '\u{1b}' {
                tag = !tag;
                return false;
            }
            !tag
        })
        .collect()
}

/// The font size of a default appearance string (`/Helv 12 Tf`).
fn font_size(da: &[u8]) -> f64 {
    let t: Vec<&[u8]> = da.split(|b| is_ws(*b)).filter(|t| !t.is_empty()).collect();
    t.iter()
        .position(|w| *w == b"Tf")
        .and_then(|i| t.get(i.checked_sub(1)?))
        .and_then(|w| std::str::from_utf8(w).ok()?.parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s > 0.0 && *s < 1000.0)
        .unwrap_or(NOTE_PT)
}

fn hex_color(c: &[f64]) -> Option<String> {
    let b = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let (r, g, bl) = match c {
        [g] => (*g, *g, *g),
        [r, g, b] => (*r, *g, *b),
        [c, m, y, k] => ((1.0 - c) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - y) * (1.0 - k)),
        _ => return None,
    };
    Some(format!("#{:02x}{:02x}{:02x}", b(r), b(g), b(bl)))
}

impl<'a> Pdf<'a> {
    fn new(data: &'a [u8]) -> Self {
        Pdf { data, xref: HashMap::new(), trailer: Dict::new(), cache: HashMap::new(), objstms: HashSet::new(), loads: 0, inflated: 0, crypt: None }
    }

    // ---- file structure ----

    fn load_xref(&mut self) -> Option<()> {
        let tail = self.data.len().saturating_sub(4096);
        let pos = self.data.get(tail..)?.windows(9).rposition(|w| w == b"startxref")? + tail;
        let mut l = Lex { d: self.data, p: pos + 9 };
        l.skip_ws();
        let first = usize::try_from(l.uint()?).ok()?;
        let (mut todo, mut seen, mut newest) = (vec![first], HashSet::new(), true);
        while let Some(off) = todo.pop() {
            if !seen.insert(off) || seen.len() > MAX_XREF_SECTIONS {
                continue;
            }
            let Some(t) = self.xref_section(off) else {
                if newest {
                    return None;
                }
                continue;
            };
            let num = |t: &Dict, k: &str| match t.get(k) {
                Some(Obj::Num(n)) if *n >= 0.0 => Some(*n as usize),
                _ => None,
            };
            // The older table is read after this one's hybrid stream.
            todo.extend(num(&t, "Prev"));
            todo.extend(num(&t, "XRefStm"));
            for (k, v) in t {
                self.trailer.entry(k).or_insert(v);
            }
            newest = false;
        }
        (!self.trailer.is_empty()).then_some(())
    }

    fn xref_section(&mut self, off: usize) -> Option<Dict> {
        let mut l = Lex { d: self.data, p: off };
        l.skip_ws();
        if l.starts(b"xref") {
            l.p += 4;
            loop {
                l.skip_ws();
                if l.starts(b"trailer") {
                    l.p += 7;
                    break;
                }
                let start = u32::try_from(l.uint()?).ok()?;
                l.skip_ws();
                let count = u32::try_from(l.uint()?).ok()?;
                for i in 0..count {
                    if self.xref.len() > MAX_XREF_ENTRIES {
                        return None;
                    }
                    l.skip_ws();
                    let o = usize::try_from(l.uint()?).ok()?;
                    l.skip_ws();
                    l.uint()?;
                    l.skip_ws();
                    let e = match l.word() {
                        b"n" if o > 0 => Entry::At(o),
                        b"n" | b"f" => Entry::Free,
                        _ => return None,
                    };
                    self.xref.entry(start.checked_add(i)?).or_insert(e);
                }
            }
            return match l.obj(0)? {
                Obj::Dict(d) => Some(d),
                _ => None,
            };
        }
        let (_, _, Obj::Stream(d, a, b, id)) = indirect(self.data, off)? else { return None };
        let data = self.decode(&d, a, b, id)?;
        let ints = |me: &mut Self, k: &str| -> Option<Vec<usize>> {
            let Obj::Arr(v) = me.val(&d, k) else { return None };
            v.iter()
                .map(|o| match me.resolve(o) {
                    Obj::Num(n) if n >= 0.0 => Some(n as usize),
                    _ => None,
                })
                .collect()
        };
        let w = ints(self, "W")?;
        let [w0, w1, w2] = w.as_slice() else { return None };
        if *w0 > 8 || *w1 > 8 || *w2 > 8 {
            return None;
        }
        let row = w0 + w1 + w2;
        let size = self.num_of(&d, "Size")? as usize;
        let index = ints(self, "Index").unwrap_or_else(|| vec![0, size]);
        let mut rows = data.chunks_exact(row.max(1));
        for [start, count] in index.as_chunks::<2>().0 {
            for i in 0..*count {
                let Some(r) = rows.next() else { break };
                if self.xref.len() > MAX_XREF_ENTRIES {
                    return None;
                }
                let field = |from: usize, n: usize| r.get(from..from + n).map_or(0u64, |s| s.iter().fold(0u64, |a, b| (a << 8) | u64::from(*b)));
                let t = if *w0 == 0 { 1 } else { field(0, *w0) };
                let f1 = field(*w0, *w1);
                let e = match t {
                    1 => Entry::At(usize::try_from(f1).ok()?),
                    2 => Entry::In(u32::try_from(f1).ok()?),
                    _ => Entry::Free,
                };
                self.xref.entry(u32::try_from(start.checked_add(i)?).ok()?).or_insert(e);
            }
        }
        Some(d)
    }

    // ---- objects ----

    fn get(&mut self, n: u32) -> Option<Obj> {
        if let Some(o) = self.cache.get(&n) {
            return Some(o.clone());
        }
        self.loads += 1;
        if self.loads > MAX_LOADS {
            return None;
        }
        match *self.xref.get(&n)? {
            Entry::At(off) => {
                let (m, g, mut o) = indirect(self.data, off)?;
                if m != n {
                    return None;
                }
                if let Some(c) = &self.crypt {
                    c.strings(&mut o, (n, g));
                }
                self.cache.insert(n, o.clone());
                Some(o)
            }
            Entry::In(s) => {
                self.load_objstm(s, false)?;
                self.cache.get(&n).cloned()
            }
            Entry::Free => None,
        }
    }

    /// Reads the members of object stream `s` into the cache: those the xref places in it, and,
    /// when `register` (a rebuilt xref), those it does not know yet.
    fn load_objstm(&mut self, s: u32, register: bool) -> Option<()> {
        if !self.objstms.insert(s) {
            return Some(());
        }
        // An object stream is a direct object; a chain of `In` entries would recurse without bound.
        if !matches!(self.xref.get(&s), Some(Entry::At(_))) {
            return None;
        }
        let Obj::Stream(d, a, b, id) = self.get(s)? else { return None };
        let count = self.num_of(&d, "N")? as usize;
        let first = self.num_of(&d, "First")? as usize;
        let data = self.decode(&d, a, b, id)?;
        let mut head = Lex { d: &data, p: 0 };
        for _ in 0..count.min(MAX_LOADS) {
            head.skip_ws();
            let num = u32::try_from(head.uint()?).ok()?;
            head.skip_ws();
            let off = first.checked_add(usize::try_from(head.uint()?).ok()?)?;
            self.loads += 1;
            if self.loads > MAX_LOADS {
                return None;
            }
            if register && !self.xref.contains_key(&num) {
                self.xref.insert(num, Entry::In(s));
            }
            if matches!(self.xref.get(&num), Some(Entry::In(x)) if *x == s)
                && !self.cache.contains_key(&num)
                && let Some(o) = (Lex { d: &data, p: off }).obj(0)
            {
                self.cache.insert(num, o);
            }
        }
        Some(())
    }

    /// Rebuilds the cross-reference of a file whose own is missing or wrong: every `N G obj` in
    /// the file (a later one replaces an earlier), the trailer entries of every `trailer` and
    /// xref stream (the last one wins), else the catalog. Returns the object streams found; the
    /// caller reads their members once decryption is set up.
    fn rebuild_xref(&mut self) -> Vec<u32> {
        let d = self.data;
        // The start of the run of bytes before `j` that satisfy `f`.
        let back = |mut j: usize, f: fn(&u8) -> bool| {
            while j > 0 && d.get(j - 1).is_some_and(f) {
                j -= 1;
            }
            j
        };
        let (mut at, mut found) = (0usize, Vec::new());
        while let Some(i) = find(d, b"obj", at) {
            at = i + 3;
            if found.len() >= MAX_LOADS || d.get(at).is_some_and(|b| !is_ws(*b) && !is_delim(*b)) {
                continue;
            }
            let g_end = back(i, |b| is_ws(*b));
            let g_start = back(g_end, u8::is_ascii_digit);
            let n_end = back(g_start, |b| is_ws(*b));
            let n_start = back(n_end, u8::is_ascii_digit);
            if g_end == i || g_start == g_end || n_end == g_start || n_start == n_end {
                continue;
            }
            if let Some(num) = d.get(n_start..n_end).and_then(|t| std::str::from_utf8(t).ok()?.parse::<u32>().ok()) {
                self.xref.insert(num, Entry::At(n_start));
                found.push((n_start, num));
            }
        }
        // Trailer dictionaries, newest (highest offset) first.
        let mut cands: Vec<(usize, Dict)> = Vec::new();
        let (mut t, mut objstms, mut catalog) = (0usize, Vec::new(), None);
        while let Some(i) = find(d, b"trailer", t) {
            t = i + 7;
            if cands.len() >= MAX_XREF_SECTIONS {
                break;
            }
            if let Some(Obj::Dict(td)) = (Lex { d, p: t }).obj(0) {
                cands.push((i, td));
            }
        }
        for &(off, num) in &found {
            let w = d.get(off..d.len().min(off + 200)).unwrap_or_default();
            if find(w, b"/ObjStm", 0).is_some() {
                objstms.push(num);
            } else if find(w, b"/XRef", 0).is_some() {
                if let Some((_, _, Obj::Stream(sd, ..))) = indirect(d, off)
                    && cands.len() < 2 * MAX_XREF_SECTIONS
                {
                    cands.push((off, sd));
                }
            } else if find(w, b"/Catalog", 0).is_some() {
                catalog = Some(num);
            }
        }
        cands.sort_by_key(|c| std::cmp::Reverse(c.0));
        for (_, td) in cands {
            for (k, v) in td {
                self.trailer.entry(k).or_insert(v);
            }
        }
        if !self.trailer.contains_key("Root")
            && let Some(c) = catalog
        {
            self.trailer.insert("Root".into(), Obj::Ref(c));
        }
        objstms
    }

    /// Follows references (a bounded number of hops); an unreadable one is `Null`.
    fn resolve(&mut self, o: &Obj) -> Obj {
        let mut cur = o.clone();
        for _ in 0..MAX_DEPTH {
            match cur {
                Obj::Ref(n) => cur = self.get(n).unwrap_or(Obj::Null),
                other => return other,
            }
        }
        Obj::Null
    }

    fn val(&mut self, d: &Dict, k: &str) -> Obj {
        d.get(k).map_or(Obj::Null, |o| self.resolve(o))
    }

    fn num_of(&mut self, d: &Dict, k: &str) -> Option<f64> {
        match self.val(d, k) {
            Obj::Num(n) => Some(n),
            _ => None,
        }
    }

    fn string(&mut self, d: &Dict, k: &str) -> Vec<u8> {
        match self.val(d, k) {
            Obj::Str(s) => s,
            _ => Vec::new(),
        }
    }

    fn nums(&mut self, o: &Obj) -> Option<Vec<f64>> {
        let Obj::Arr(v) = self.resolve(o) else { return None };
        if v.len() > MAX_COORDS {
            return None;
        }
        v.iter()
            .map(|e| match self.resolve(e) {
                Obj::Num(n) => Some(n),
                _ => None,
            })
            .collect()
    }

    fn nums_of(&mut self, d: &Dict, k: &str) -> Option<Vec<f64>> {
        let o = d.get(k)?;
        self.nums(o)
    }

    // ---- streams ----

    /// A stream's data: decrypted, then through its filters (`None` for a filter that is not
    /// read, such as an image filter, or for damaged data).
    fn decode(&mut self, d: &Dict, a: usize, b: usize, id: (u32, u16)) -> Option<Vec<u8>> {
        let raw = self.data.get(a..b)?.to_vec();
        match self.decode_raw(d, raw, Some(id))? {
            (data, None) => Some(data),
            _ => None,
        }
    }

    /// `cur` (a stream's raw data; `id` is its encryption key, `None` for inline data) through
    /// the filters of `d`, up to an image filter (DCT, CCITT, JBIG2, JPX): that one's name and
    /// parameters are returned with the data it would decode. `None` for damaged data or an
    /// unknown filter.
    fn decode_raw(&mut self, d: &Dict, mut cur: Vec<u8>, id: Option<(u32, u16)>) -> Option<(Vec<u8>, Option<(String, Dict)>)> {
        let list = |me: &mut Self, k: &str| -> Vec<Obj> {
            match me.val(d, k) {
                Obj::Null => Vec::new(),
                Obj::Arr(v) => v.iter().map(|o| me.resolve(o)).collect(),
                o => vec![o],
            }
        };
        let (filters, parms) = (list(self, "Filter"), list(self, "DecodeParms"));
        // The xref stream is never encrypted.
        if !matches!(d.get("Type"), Some(Obj::Name(t)) if t == "XRef")
            && let (Some(c), Some(id)) = (&self.crypt, id)
        {
            cur = c.open(c.stream_method(&filters, &parms), id, &cur);
        }
        for (i, f) in filters.iter().enumerate() {
            let Obj::Name(n) = f else { return None };
            let p = match parms.get(i) {
                Some(Obj::Dict(p)) => Some(p.clone()),
                _ => None,
            };
            if matches!(n.as_str(), "DCTDecode" | "DCT" | "CCITTFaxDecode" | "CCF" | "JBIG2Decode" | "JPXDecode") {
                return Some((cur, Some((n.clone(), p.unwrap_or_default()))));
            }
            let int = |me: &mut Self, k: &str, def: usize| {
                p.as_ref().and_then(|p| me.num_of(p, k)).filter(|n| *n >= 0.0 && *n < 1e9).map_or(def, |n| n as usize)
            };
            let predicted = matches!(n.as_str(), "FlateDecode" | "Fl" | "LZWDecode" | "LZW");
            cur = match n.as_str() {
                "FlateDecode" | "Fl" => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&cur, MAX_INFLATE).ok()?,
                "LZWDecode" | "LZW" => filters::lzw(&cur, int(self, "EarlyChange", 1) != 0, MAX_INFLATE)?,
                "ASCIIHexDecode" | "AHx" => filters::ascii_hex(&cur),
                "ASCII85Decode" | "A85" => filters::ascii85(&cur)?,
                "RunLengthDecode" | "RL" => filters::run_length(&cur, MAX_INFLATE)?,
                // Handled above (the decryption).
                "Crypt" => cur,
                _ => return None,
            };
            self.inflated = self.inflated.checked_add(cur.len())?;
            if cur.len() > MAX_INFLATE || self.inflated > MAX_INFLATE_TOTAL {
                return None;
            }
            if predicted && p.is_some() {
                let (pred, colors) = (int(self, "Predictor", 1), int(self, "Colors", 1));
                let (bpc, cols) = (int(self, "BitsPerComponent", 8), int(self, "Columns", 1));
                cur = filters::predict(cur, pred, colors, bpc, cols, MAX_INFLATE)?;
            }
        }
        Some((cur, None))
    }

    // ---- pages ----

    /// Page `target` (1-based) with the inherited `/MediaBox` and `/Rotate` merged in (`None` when
    /// there are fewer pages), and the page count.
    fn find_page(&mut self, target: usize) -> Option<(Option<Dict>, usize)> {
        let root = self.trailer.get("Root")?.clone();
        let Obj::Dict(root) = self.resolve(&root) else { return None };
        let pages = root.get("Pages")?.clone();
        let (mut stack, mut seen, mut nodes, mut idx) = (vec![(pages, Dict::new())], HashSet::new(), 0usize, 0usize);
        while let Some((node, inherited)) = stack.pop() {
            if let Obj::Ref(n) = &node
                && !seen.insert(*n)
            {
                continue;
            }
            nodes += 1;
            if nodes > MAX_PAGE_NODES {
                return None;
            }
            let Obj::Dict(mut d) = self.resolve(&node) else { continue };
            let mut inh = inherited;
            for k in ["MediaBox", "CropBox", "Rotate", "Resources"] {
                if let Some(v) = d.get(k) {
                    inh.insert(k.into(), v.clone());
                }
            }
            if let Obj::Arr(kids) = self.val(&d, "Kids") {
                stack.extend(kids.into_iter().rev().map(|k| (k, inh.clone())));
                continue;
            }
            idx += 1;
            if idx == target {
                for (k, v) in inh {
                    d.entry(k).or_insert(v);
                }
                let pages = root_pages(self, &root);
                let total = pages.and_then(|p| self.num_of(&p, "Count")).map_or(0, |c| c as usize).max(idx);
                return Some((Some(d), total));
            }
        }
        Some((None, idx))
    }

    /// A rectangle array as `[x0, y0, x1, y1]`, ordered.
    fn rect_of(&mut self, p: &Dict, k: &str) -> Option<[f64; 4]> {
        let b = self.nums_of(p, k)?;
        let [a, b, c, d] = b.as_slice() else { return None };
        Some([a.min(*c), b.min(*d), a.max(*c), b.max(*d)])
    }

    /// The visible box: `/CropBox` inside `/MediaBox` (the media box when the crop box is missing
    /// or does not overlap it).
    fn page_geometry(&mut self, p: &Dict) -> Option<Page> {
        let m = self.rect_of(p, "MediaBox")?;
        let usable =
            |r: &[f64; 4]| r.iter().all(|v| v.is_finite()) && r[2] - r[0] >= 1e-6 && r[3] - r[1] >= 1e-6 && r[2] - r[0] < 1e9 && r[3] - r[1] < 1e9;
        let crop = self.rect_of(p, "CropBox").map(|c| [c[0].max(m[0]), c[1].max(m[1]), c[2].min(m[2]), c[3].min(m[3])]).filter(usable);
        let r = crop.unwrap_or(m);
        if !usable(&r) {
            return None;
        }
        let (x0, y0, w, h) = (r[0], r[1], r[2] - r[0], r[3] - r[1]);
        let rot = (self.num_of(p, "Rotate").unwrap_or(0.0) as i64).rem_euclid(360);
        let (dw, dh) = if rot == 90 || rot == 270 { (h, w) } else { (w, h) };
        Some(Page { x0, y0, w, h, rot, dw, dh })
    }

    // ---- annotations ----

    fn color(&mut self, a: &Dict) -> Option<String> {
        hex_color(&self.nums_of(a, "C")?)
    }

    /// The comment of an annotation: `/Contents`, unless the rich text `/RC` (what a viewer
    /// shows) says something else once its markup is stripped and white space collapsed.
    fn annot_text(&mut self, a: &Dict) -> String {
        let contents = pdf_text(&self.string(a, "Contents"));
        let rc = match self.val(a, "RC") {
            Obj::Str(s) => pdf_text(&s),
            Obj::Stream(d, x, y, id) => match self.decode(&d, x, y, id) {
                Some(b) if b.starts_with(&[0xFE, 0xFF]) || b.starts_with(&[0xEF, 0xBB, 0xBF]) => pdf_text(&b),
                Some(b) => String::from_utf8_lossy(&b).into_owned(),
                None => String::new(),
            },
            _ => String::new(),
        };
        let rc = rich::plain(&rc);
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        if rc.is_empty() || norm(&rc) == norm(&contents) { contents } else { rc }
    }

    /// `/Rect` as `[x0, y0, x1, y1]`, ordered.
    fn rect(&mut self, a: &Dict) -> Option<[f64; 4]> {
        let r = self.nums_of(a, "Rect")?;
        let [x0, y0, x1, y1] = r.as_slice() else { return None };
        Some([x0.min(*x1), y0.min(*y1), x0.max(*x1), y0.max(*y1)])
    }

    /// The rectangle a Square or Circle annotation draws: `/Rect` inset by `/RD`.
    fn inset(&mut self, a: &Dict, r: [f64; 4]) -> [f64; 4] {
        if let Some(rd) = self.nums_of(a, "RD")
            && let [l, t, rt, b] = rd.as_slice()
        {
            let q = [r[0] + l, r[1] + b, r[2] - rt, r[3] - t];
            if q[0] < q[2] && q[1] < q[3] {
                return q;
            }
        }
        r
    }

    /// A cloud border: `/BE /S /C`, or `/IT /PolygonCloud`.
    fn is_cloud(&mut self, a: &Dict) -> bool {
        let be = match self.val(a, "BE") {
            Obj::Dict(d) => matches!(self.val(&d, "S"), Obj::Name(n) if n == "C"),
            _ => false,
        };
        be || matches!(self.val(a, "IT"), Obj::Name(n) if n == "PolygonCloud")
    }

    fn annot(&mut self, a: &Dict, pg: &Page, out: &mut Vec<Value>, notes: &mut Vec<String>) {
        let Obj::Name(sub) = self.val(a, "Subtype") else { return };
        let flags = self.num_of(a, "F").unwrap_or(0.0) as u32;
        // Hidden and NoView annotations are not part of the review.
        if flags & (2 | 32) != 0 {
            return;
        }
        let color = self.color(a);
        let c = color.as_deref();
        let text = self.annot_text(a);
        let note_h = NOTE_PT / pg.dh;
        let comment = |pts: &[[f64; 2]], out: &mut Vec<Value>| {
            if !text.is_empty() {
                out.push(text_markup(&text, min_uv(pts), note_h, c));
            }
        };
        match sub.as_str() {
            "FreeText" | "Text" => {
                let Some(r) = self.rect(a) else { return };
                if text.is_empty() {
                    return;
                }
                let size = if sub == "FreeText" { font_size(&self.string(a, "DA")) } else { NOTE_PT };
                let corners = [pg.uv(r[0], r[1]), pg.uv(r[2], r[3]), pg.uv(r[0], r[3]), pg.uv(r[2], r[1])];
                out.push(text_markup(&text, min_uv(&corners), size / pg.dh, c));
            }
            "Square" | "Circle" => {
                let Some(r) = self.rect(a) else { return };
                let r = self.inset(a, r);
                let (p0, p1) = (pg.uv(r[0], r[1]), pg.uv(r[2], r[3]));
                if sub == "Circle" {
                    out.push(shape("ellipse", &[p0, p1], c));
                } else if self.is_cloud(a) {
                    out.push(shape("cloud", &[pg.uv(r[0], r[3]), pg.uv(r[2], r[3]), pg.uv(r[2], r[1]), pg.uv(r[0], r[1])], c));
                } else {
                    out.push(shape("rect", &[p0, p1], c));
                }
                comment(&[p0, p1], out);
            }
            "Line" => {
                let Some(l) = self.nums_of(a, "L") else { return };
                let [x1, y1, x2, y2, ..] = l.as_slice() else { return };
                let (p1, p2) = (pg.uv(*x1, *y1), pg.uv(*x2, *y2));
                let ends: Vec<String> = match self.val(a, "LE") {
                    Obj::Arr(v) => v
                        .iter()
                        .map(|o| match self.resolve(o) {
                            Obj::Name(n) => n,
                            _ => "None".into(),
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let arrow = |i: usize| ends.get(i).is_some_and(|n| n != "None");
                if arrow(0) || arrow(1) || !text.is_empty() {
                    // The first point of a leader is its arrow head.
                    let pts = if arrow(1) && !arrow(0) { [p2, p1] } else { [p1, p2] };
                    let mut v = shape("leader", &pts, c);
                    if !text.is_empty() {
                        v["text"] = json!(text);
                    }
                    out.push(v);
                } else {
                    out.push(shape("polyline", &[p1, p2], c));
                }
            }
            "PolyLine" | "Polygon" => {
                let Some(v) = self.nums_of(a, "Vertices") else { return };
                let mut pts: Vec<[f64; 2]> = v.as_chunks::<2>().0.iter().map(|[x, y]| pg.uv(*x, *y)).collect();
                if pts.len() < 2 {
                    return;
                }
                if self.is_cloud(a) {
                    out.push(shape("cloud", &pts, c));
                } else {
                    if sub == "Polygon" {
                        pts.extend(pts.first().copied());
                    }
                    out.push(shape("polyline", &pts, c));
                }
                comment(&pts, out);
            }
            "Ink" => {
                let Obj::Arr(strokes) = self.val(a, "InkList") else { return };
                for s in strokes.iter().take(MAX_COORDS) {
                    let Some(v) = self.nums(s) else { continue };
                    let pts: Vec<[f64; 2]> = v.as_chunks::<2>().0.iter().map(|[x, y]| pg.uv(*x, *y)).collect();
                    if pts.len() >= 2 {
                        out.push(shape("polyline", &pts, c));
                    }
                }
            }
            "Popup" | "Link" | "Widget" => {}
            other => notes.push(format!("skipped a /{other} annotation")),
        }
    }
}

fn root_pages(pdf: &mut Pdf, root: &Dict) -> Option<Dict> {
    match pdf.val(root, "Pages") {
        Obj::Dict(d) => Some(d),
        _ => None,
    }
}

#[cfg(test)]
#[path = "pdf_markup_tests.rs"]
mod tests;
