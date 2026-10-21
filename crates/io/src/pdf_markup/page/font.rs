//! Fonts of a page: simple fonts (Type 1, TrueType, Type 3), Type 0 composite fonts with their
//! CMaps, the embedded programs (TrueType / OpenType / CFF through skrifa, Type 1 through
//! `type1`) and, for fonts that are not embedded, our own fonts (`cadcraft_fonts`) with the
//! widths of `/Widths`. A glyph becomes a path in em units (1 = the font size).

use std::collections::HashMap;

use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::TableProvider;
use skrifa::raw::tables::cmap::PlatformId;
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tiny_skia::{FillRule, Path, PathBuilder};

use super::cff;
use super::cmap::CMap;
use super::enc::{Base, Encoding, base_char};
use super::type1::{PathOp, Type1};
use crate::pdf_markup::{Dict, Obj, Pdf};

const MAX_GLYPH_NAMES: usize = 70_000;
const MAX_W_ENTRIES: usize = 100_000;
/// The cap height of the substitute fonts' em (Helvetica's, about).
const CAP: f64 = 0.72;

pub(super) struct Font {
    pub id: usize,
    pub src: Src,
    pub enc: Encoding,
    /// Simple fonts: the width of every code in 1/1000 em.
    pub widths: Vec<Option<f64>>,
    pub missing: f64,
    pub composite: bool,
    pub vertical: bool,
    pub cmap: Option<CMap>,
    pub to_uni: Option<CMap>,
    pub cid_w: HashMap<u32, f64>,
    pub dw: f64,
    /// `/DW2`: the vertical origin's y and the vertical advance.
    pub dw2: (f64, f64),
    pub symbolic: bool,
    pub oblique: f64,
    pub bold: bool,
    pub t3: Option<Type3>,
}

pub(super) struct Type3 {
    pub matrix: [f64; 6],
    pub procs: Dict,
    pub resources: Option<Dict>,
}

pub(super) enum Src {
    Sfnt(Sfnt),
    T1(Type1),
    Std(cadcraft_fonts::TextFont),
    None,
}

pub(super) struct Sfnt {
    data: Vec<u8>,
    upem: f64,
    names: HashMap<String, u32>,
    cff: Option<cff::Cff>,
    /// `/CIDToGIDMap` stream of a CIDFontType2 (identity when `None`).
    cid_to_gid: Option<Vec<u8>>,
    truetype: bool,
}

/// One character of a shown string.
pub(super) struct Ch {
    pub code: u32,
    /// The glyph cache key (the CID of a composite font, else the code).
    pub key: u32,
    pub n: usize,
    /// Its width in em, when the font dictionary gives one.
    pub w: Option<f64>,
}

/// A glyph: a path to fill (rule `rule`) and/or a path to stroke, in em units.
pub(super) struct Shape {
    pub fill: Option<Path>,
    pub rule: FillRule,
    pub line: Option<Path>,
    /// The glyph's own advance in em.
    pub adv: f64,
}

struct Pen {
    pb: PathBuilder,
    s: f32,
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb.move_to(x * self.s, y * self.s);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb.line_to(x * self.s, y * self.s);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.pb.quad_to(cx0 * self.s, cy0 * self.s, x * self.s, y * self.s);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.pb.cubic_to(cx0 * self.s, cy0 * self.s, cx1 * self.s, cy1 * self.s, x * self.s, y * self.s);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

fn cmap_lookup(font: &FontRef, platform: u16, encoding: u16, cp: u32) -> Option<u32> {
    let cmap = font.cmap().ok()?;
    let pid = match platform {
        1 => PlatformId::Macintosh,
        3 => PlatformId::Windows,
        _ => return None,
    };
    let rec = cmap.encoding_records().iter().find(|r| r.platform_id() == pid && r.encoding_id() == encoding)?;
    let sub = rec.subtable(cmap.offset_data()).ok()?;
    sub.map_codepoint(cp).map(|g| g.to_u32()).filter(|g| *g != 0)
}

impl Sfnt {
    fn new(bytes: Vec<u8>) -> Option<Sfnt> {
        let sfnt = matches!(bytes.get(..4), Some(b"OTTO" | b"true" | b"ttcf" | [0, 1, 0, 0]));
        let data = if sfnt {
            bytes
        } else {
            let info = cff::parse(&bytes)?;
            cff::wrap(&bytes, info.n_glyphs)
        };
        let font = FontRef::new(&data).ok()?;
        let upem = f64::from(font.head().ok()?.units_per_em()).max(1.0);
        let mut names = HashMap::new();
        for (gid, name) in font.glyph_names().iter().take(MAX_GLYPH_NAMES) {
            names.entry(name.as_str().to_string()).or_insert(gid.to_u32());
        }
        let cff = font.data_for_tag(skrifa::raw::types::Tag::new(b"CFF ")).and_then(|d| cff::parse(d.as_bytes()));
        let truetype = font.data_for_tag(skrifa::raw::types::Tag::new(b"glyf")).is_some();
        Some(Sfnt { data, upem, names, cff, cid_to_gid: None, truetype })
    }

    fn outline(&self, gid: u32) -> Option<Path> {
        let font = FontRef::new(&self.data).ok()?;
        let glyph = font.outline_glyphs().get(GlyphId::new(gid))?;
        let mut pen = Pen { pb: PathBuilder::new(), s: (1.0 / self.upem) as f32 };
        glyph.draw(DrawSettings::unhinted(Size::unscaled(), LocationRef::default()), &mut pen).ok()?;
        pen.pb.finish()
    }

    fn advance(&self, gid: u32) -> Option<f64> {
        let font = FontRef::new(&self.data).ok()?;
        let w = font.glyph_metrics(Size::unscaled(), LocationRef::default()).advance_width(GlyphId::new(gid))?;
        Some(f64::from(w) / self.upem)
    }

    /// The glyph of a simple font's `code`, by the rules of ISO 32000 §9.6.5 (names first for
    /// CFF programs, the Unicode and symbolic cmaps first for TrueType).
    fn simple_gid(&self, f: &Font, code: u8) -> Option<u32> {
        let font = FontRef::new(&self.data).ok()?;
        let name = f.enc.names.get(usize::from(code)).and_then(|n| n.as_deref());
        let ch = f.enc.chars.get(usize::from(code)).copied().flatten();
        let by_name = || name.and_then(|n| self.names.get(n).copied()).filter(|g| *g != 0);
        let by_unicode = || ch.and_then(|c| font.charmap().map(c)).map(|g| g.to_u32()).filter(|g| *g != 0);
        let by_mac =
            || ch.and_then(|c| (0..=255u8).find(|k| base_char(Base::Mac, *k) == Some(c))).and_then(|k| cmap_lookup(&font, 1, 0, u32::from(k)));
        let symbolic = || {
            let c = u32::from(code);
            cmap_lookup(&font, 3, 0, c).or_else(|| cmap_lookup(&font, 3, 0, 0xF000 | c)).or_else(|| cmap_lookup(&font, 1, 0, c))
        };
        if self.truetype {
            if f.symbolic || !f.enc.explicit {
                symbolic().or_else(by_unicode).or_else(by_name)
            } else {
                by_unicode().or_else(by_mac).or_else(by_name).or_else(symbolic)
            }
        } else {
            by_name().or_else(by_unicode).or_else(symbolic)
        }
    }

    fn cid_gid(&self, cid: u32) -> Option<u32> {
        if let Some(m) = &self.cid_to_gid {
            let i = (cid as usize).checked_mul(2)?;
            return Some(u32::from(u16::from_be_bytes([*m.get(i)?, *m.get(i + 1)?])));
        }
        match &self.cff {
            Some(c) if c.cid_keyed => c.cid_to_gid.get(&cid).copied(),
            _ => Some(cid),
        }
    }
}

fn pick_text_font(base: &str) -> cadcraft_fonts::TextFont {
    let name = base.split_once('+').map_or(base, |(_, n)| n);
    let family = name.split([',', '-']).next().unwrap_or(name);
    let lower = family.to_ascii_lowercase();
    let alias = if lower.contains("helvetica") || lower.contains("arial") {
        Some("Arial")
    } else if lower.contains("times") {
        Some("Times New Roman")
    } else if lower.contains("courier") {
        Some("Courier New")
    } else {
        None
    };
    for cand in [Some(family), alias].into_iter().flatten() {
        if let Some(f) = cadcraft_fonts::ttf::find(cand) {
            return cadcraft_fonts::TextFont::Outline(f);
        }
    }
    cadcraft_fonts::TextFont::resolve(family)
}

fn font_stream(pdf: &mut Pdf, desc: &Dict) -> Option<(Vec<u8>, bool)> {
    for (k, t1) in [("FontFile2", false), ("FontFile3", false), ("FontFile", true)] {
        if let Obj::Stream(d, a, b, id) = pdf.val(desc, k)
            && let Some(data) = pdf.decode(&d, a, b, id)
        {
            return Some((data, t1));
        }
    }
    None
}

fn names_of(name: &str) -> (bool, bool) {
    let l = name.to_ascii_lowercase();
    (l.contains("italic") || l.contains("oblique"), l.contains("bold") || l.contains("black") || l.contains("heavy"))
}

impl Font {
    fn blank(id: usize) -> Font {
        Font {
            id,
            src: Src::None,
            enc: Encoding::from_base(Base::Standard),
            widths: vec![None; 256],
            missing: 0.0,
            composite: false,
            vertical: false,
            cmap: None,
            to_uni: None,
            cid_w: HashMap::new(),
            dw: 1000.0,
            dw2: (880.0, -1000.0),
            symbolic: false,
            oblique: 0.0,
            bold: false,
            t3: None,
        }
    }

    /// Loads a font dictionary.
    pub(super) fn load(pdf: &mut Pdf, fd: &Dict, id: usize) -> Option<Font> {
        let sub = match pdf.val(fd, "Subtype") {
            Obj::Name(n) => n,
            _ => return None,
        };
        let base = match pdf.val(fd, "BaseFont") {
            Obj::Name(n) => n,
            _ => String::new(),
        };
        let mut f = Font::blank(id);
        let (italic, bold) = names_of(&base);
        f.oblique = if italic { 0.2 } else { 0.0 };
        f.bold = bold;
        match sub.as_str() {
            "Type0" => f.load_type0(pdf, fd, &base)?,
            "Type3" => f.load_type3(pdf, fd)?,
            _ => f.load_simple(pdf, fd, &base),
        }
        if let Obj::Stream(d, a, b, id) = pdf.val(fd, "ToUnicode")
            && let Some(data) = pdf.decode(&d, a, b, id)
        {
            f.to_uni = Some(CMap::parse(&data));
        }
        Some(f)
    }

    fn load_simple(&mut self, pdf: &mut Pdf, fd: &Dict, base: &str) {
        let desc = match pdf.val(fd, "FontDescriptor") {
            Obj::Dict(d) => d,
            _ => Dict::new(),
        };
        let flags = pdf.num_of(&desc, "Flags").unwrap_or(0.0) as i64;
        self.symbolic = flags & 4 != 0 && flags & 32 == 0;
        if flags & 64 != 0 {
            self.oblique = 0.2;
        }
        self.missing = pdf.num_of(&desc, "MissingWidth").unwrap_or(0.0);
        let first = pdf.num_of(fd, "FirstChar").unwrap_or(0.0).max(0.0) as usize;
        if let Obj::Arr(ws) = pdf.val(fd, "Widths") {
            for (i, w) in ws.iter().enumerate().take(256) {
                if let (Obj::Num(w), Some(slot)) = (pdf.resolve(w), self.widths.get_mut(first + i)) {
                    *slot = Some(w);
                }
            }
        }
        let embedded = font_stream(pdf, &desc);
        match embedded {
            Some((data, true)) => {
                if let Some(t1) = Type1::parse(&data) {
                    self.enc = Encoding::load(pdf, fd, Some(&|c| t1.builtin_encoding(c).map(str::to_string)));
                    self.src = Src::T1(t1);
                    return;
                }
            }
            Some((data, false)) => {
                if let Some(s) = Sfnt::new(data) {
                    self.enc = Encoding::load(pdf, fd, None);
                    self.src = Src::Sfnt(s);
                    return;
                }
            }
            None => {}
        }
        self.enc = Encoding::load(pdf, fd, None);
        self.src = Src::Std(pick_text_font(base));
    }

    fn load_type3(&mut self, pdf: &mut Pdf, fd: &Dict) -> Option<()> {
        let m = pdf.nums_of(fd, "FontMatrix").filter(|m| m.len() == 6)?;
        let matrix = [m[0], m[1], m[2], m[3], m[4], m[5]];
        let Obj::Dict(procs) = pdf.val(fd, "CharProcs") else { return None };
        let resources = match pdf.val(fd, "Resources") {
            Obj::Dict(d) => Some(d),
            _ => None,
        };
        self.enc = Encoding::load(pdf, fd, None);
        let first = pdf.num_of(fd, "FirstChar").unwrap_or(0.0).max(0.0) as usize;
        if let Obj::Arr(ws) = pdf.val(fd, "Widths") {
            for (i, w) in ws.iter().enumerate().take(256) {
                // Glyph space → text space: widths are scaled by the font matrix right here.
                if let (Obj::Num(w), Some(slot)) = (pdf.resolve(w), self.widths.get_mut(first + i)) {
                    *slot = Some(w * matrix[0] * 1000.0);
                }
            }
        }
        self.t3 = Some(Type3 { matrix, procs, resources });
        Some(())
    }

    fn load_type0(&mut self, pdf: &mut Pdf, fd: &Dict, base: &str) -> Option<()> {
        self.composite = true;
        let Obj::Arr(kids) = pdf.val(fd, "DescendantFonts") else { return None };
        let Obj::Dict(cid) = pdf.resolve(kids.first()?) else { return None };
        // The character map.
        let cmap = match pdf.val(fd, "Encoding") {
            Obj::Name(n) => CMap::identity(n.ends_with("-V")),
            Obj::Stream(d, a, b, id) => {
                let data = pdf.decode(&d, a, b, id)?;
                let mut m = CMap::parse(&data);
                if matches!(pdf.val(&d, "CMapName"), Obj::Name(n) if n.starts_with("Identity")) {
                    m.identity = true;
                }
                m
            }
            _ => CMap::identity(false),
        };
        self.vertical = cmap.vertical;
        self.cmap = Some(cmap);
        self.dw = pdf.num_of(&cid, "DW").unwrap_or(1000.0);
        if let Some(v) = pdf.nums_of(&cid, "DW2").filter(|v| v.len() == 2) {
            self.dw2 = (v[0], v[1]);
        }
        if let Obj::Arr(w) = pdf.val(&cid, "W") {
            self.read_w(pdf, &w);
        }
        let desc = match pdf.val(&cid, "FontDescriptor") {
            Obj::Dict(d) => d,
            _ => Dict::new(),
        };
        if let Some((data, false)) = font_stream(pdf, &desc)
            && let Some(mut s) = Sfnt::new(data)
        {
            if let Obj::Stream(d, a, b, id) = pdf.val(&cid, "CIDToGIDMap") {
                s.cid_to_gid = pdf.decode(&d, a, b, id);
            }
            self.src = Src::Sfnt(s);
            return Some(());
        }
        self.src = Src::Std(pick_text_font(base));
        Some(())
    }

    fn read_w(&mut self, pdf: &mut Pdf, w: &[Obj]) {
        let mut i = 0;
        while i < w.len() && self.cid_w.len() < MAX_W_ENTRIES {
            let Obj::Num(c) = pdf.resolve(&w[i]) else { break };
            match w.get(i + 1).map(|o| pdf.resolve(o)) {
                Some(Obj::Arr(list)) => {
                    for (k, v) in list.iter().enumerate().take(MAX_W_ENTRIES) {
                        if let Obj::Num(v) = pdf.resolve(v) {
                            self.cid_w.insert(c as u32 + k as u32, v);
                        }
                    }
                    i += 2;
                }
                Some(Obj::Num(c2)) => {
                    let Some(Obj::Num(v)) = w.get(i + 2).map(|o| pdf.resolve(o)) else { break };
                    let (lo, hi) = (c.max(0.0) as u32, (c2.max(0.0) as u32).min(c.max(0.0) as u32 + 65_535));
                    for cid in lo..=hi {
                        self.cid_w.insert(cid, v);
                    }
                    i += 3;
                }
                _ => break,
            }
        }
    }

    /// The characters of a shown string.
    pub(super) fn chars(&self, s: &[u8]) -> Vec<Ch> {
        if let Some(m) = &self.cmap {
            // Without embedded glyphs the code stands for a character via /ToUnicode.
            let by_code = !matches!(self.src, Src::Sfnt(_));
            return m
                .decode(s)
                .into_iter()
                .map(|(code, cid, n)| Ch {
                    code,
                    key: if by_code { code } else { cid },
                    n,
                    w: Some(self.cid_w.get(&cid).copied().unwrap_or(self.dw) / 1000.0),
                })
                .collect();
        }
        s.iter()
            .map(|b| {
                let w = self.widths.get(usize::from(*b)).copied().flatten().or(if self.widths.iter().any(Option::is_some) {
                    Some(self.missing)
                } else {
                    None
                });
                Ch { code: u32::from(*b), key: u32::from(*b), n: 1, w: w.map(|w| w / 1000.0) }
            })
            .collect()
    }

    /// The glyph shape of a character.
    pub(super) fn shape(&self, ch: &Ch) -> Option<Shape> {
        match &self.src {
            Src::Sfnt(s) => {
                let gid = if self.composite { s.cid_gid(ch.key)? } else { s.simple_gid(self, u8::try_from(ch.code).ok()?)? };
                Some(Shape { fill: s.outline(gid), rule: FillRule::Winding, line: None, adv: s.advance(gid).unwrap_or(0.0) })
            }
            Src::T1(t) => {
                let name = self.enc.names.get(ch.code as usize)?.as_deref()?;
                let g = t.glyph(name)?;
                let m = t.font_matrix();
                let mut pb = PathBuilder::new();
                let p = |x: f64, y: f64| ((x * m[0] + y * m[2] + m[4]) as f32, (x * m[1] + y * m[3] + m[5]) as f32);
                for op in &g.ops {
                    match *op {
                        PathOp::Move(x, y) => {
                            let (x, y) = p(x, y);
                            pb.move_to(x, y);
                        }
                        PathOp::Line(x, y) => {
                            let (x, y) = p(x, y);
                            pb.line_to(x, y);
                        }
                        PathOp::Cubic(a, b, c, d, e, f) => {
                            let (a, b) = p(a, b);
                            let (c, d) = p(c, d);
                            let (e, f) = p(e, f);
                            pb.cubic_to(a, b, c, d, e, f);
                        }
                        PathOp::Close => pb.close(),
                    }
                }
                Some(Shape { fill: pb.finish(), rule: FillRule::Winding, line: None, adv: g.advance * m[0] })
            }
            Src::Std(tf) => {
                let c = if self.composite {
                    self.to_uni.as_ref()?.unicode.get(&ch.code)?.chars().next()?
                } else {
                    self.enc.chars.get(ch.code as usize).copied().flatten()?
                };
                Some(self.std_shape(tf, c))
            }
            Src::None => None,
        }
    }

    fn std_shape(&self, tf: &cadcraft_fonts::TextFont, c: char) -> Shape {
        let adv_of = |w: f64| if c == ' ' { 0.278 } else { w + 0.08 };
        if c.is_whitespace() {
            return Shape { fill: None, rule: FillRule::Winding, line: None, adv: 0.278 };
        }
        let sh = cadcraft_fonts::shape_line(tf, &c.to_string(), CAP, 1.0, self.oblique);
        let (mut fill, mut line) = (PathBuilder::new(), PathBuilder::new());
        for contour in sh.glyphs.iter().flatten() {
            for (i, v) in contour.iter().enumerate() {
                if i == 0 {
                    fill.move_to(v.x as f32, v.y as f32);
                } else {
                    fill.line_to(v.x as f32, v.y as f32);
                }
            }
            fill.close();
        }
        for stroke in &sh.strokes {
            for (i, v) in stroke.iter().enumerate() {
                if i == 0 {
                    line.move_to(v.x as f32, v.y as f32);
                } else {
                    line.line_to(v.x as f32, v.y as f32);
                }
            }
        }
        Shape { fill: fill.finish(), rule: FillRule::EvenOdd, line: line.finish(), adv: adv_of(sh.width) }
    }
}
