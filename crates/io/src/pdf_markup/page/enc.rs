//! Simple-font encodings: WinAnsi, MacRoman and Standard code → character tables, glyph names ↔
//! Unicode for the Latin repertoire (plus `uniXXXX` / `uXXXX`), and the `/Encoding` dictionary
//! with its `/Differences`.

use crate::pdf_markup::{Dict, Obj, Pdf};

/// Glyph names outside plain ASCII letters/digits, with their Unicode character.
static NAMES: &[(&str, u32)] = &[
    ("space", 0x20),
    ("exclam", 0x21),
    ("quotedbl", 0x22),
    ("numbersign", 0x23),
    ("dollar", 0x24),
    ("percent", 0x25),
    ("ampersand", 0x26),
    ("quotesingle", 0x27),
    ("parenleft", 0x28),
    ("parenright", 0x29),
    ("asterisk", 0x2A),
    ("plus", 0x2B),
    ("comma", 0x2C),
    ("hyphen", 0x2D),
    ("period", 0x2E),
    ("slash", 0x2F),
    ("zero", 0x30),
    ("one", 0x31),
    ("two", 0x32),
    ("three", 0x33),
    ("four", 0x34),
    ("five", 0x35),
    ("six", 0x36),
    ("seven", 0x37),
    ("eight", 0x38),
    ("nine", 0x39),
    ("colon", 0x3A),
    ("semicolon", 0x3B),
    ("less", 0x3C),
    ("equal", 0x3D),
    ("greater", 0x3E),
    ("question", 0x3F),
    ("at", 0x40),
    ("bracketleft", 0x5B),
    ("backslash", 0x5C),
    ("bracketright", 0x5D),
    ("asciicircum", 0x5E),
    ("underscore", 0x5F),
    ("grave", 0x60),
    ("braceleft", 0x7B),
    ("bar", 0x7C),
    ("braceright", 0x7D),
    ("asciitilde", 0x7E),
    ("exclamdown", 0xA1),
    ("cent", 0xA2),
    ("sterling", 0xA3),
    ("currency", 0xA4),
    ("yen", 0xA5),
    ("brokenbar", 0xA6),
    ("section", 0xA7),
    ("dieresis", 0xA8),
    ("copyright", 0xA9),
    ("ordfeminine", 0xAA),
    ("guillemotleft", 0xAB),
    ("logicalnot", 0xAC),
    ("registered", 0xAE),
    ("macron", 0xAF),
    ("degree", 0xB0),
    ("plusminus", 0xB1),
    ("twosuperior", 0xB2),
    ("threesuperior", 0xB3),
    ("acute", 0xB4),
    ("mu", 0xB5),
    ("paragraph", 0xB6),
    ("periodcentered", 0xB7),
    ("cedilla", 0xB8),
    ("onesuperior", 0xB9),
    ("ordmasculine", 0xBA),
    ("guillemotright", 0xBB),
    ("onequarter", 0xBC),
    ("onehalf", 0xBD),
    ("threequarters", 0xBE),
    ("questiondown", 0xBF),
    ("Agrave", 0xC0),
    ("Aacute", 0xC1),
    ("Acircumflex", 0xC2),
    ("Atilde", 0xC3),
    ("Adieresis", 0xC4),
    ("Aring", 0xC5),
    ("AE", 0xC6),
    ("Ccedilla", 0xC7),
    ("Egrave", 0xC8),
    ("Eacute", 0xC9),
    ("Ecircumflex", 0xCA),
    ("Edieresis", 0xCB),
    ("Igrave", 0xCC),
    ("Iacute", 0xCD),
    ("Icircumflex", 0xCE),
    ("Idieresis", 0xCF),
    ("Eth", 0xD0),
    ("Ntilde", 0xD1),
    ("Ograve", 0xD2),
    ("Oacute", 0xD3),
    ("Ocircumflex", 0xD4),
    ("Otilde", 0xD5),
    ("Odieresis", 0xD6),
    ("multiply", 0xD7),
    ("Oslash", 0xD8),
    ("Ugrave", 0xD9),
    ("Uacute", 0xDA),
    ("Ucircumflex", 0xDB),
    ("Udieresis", 0xDC),
    ("Yacute", 0xDD),
    ("Thorn", 0xDE),
    ("germandbls", 0xDF),
    ("agrave", 0xE0),
    ("aacute", 0xE1),
    ("acircumflex", 0xE2),
    ("atilde", 0xE3),
    ("adieresis", 0xE4),
    ("aring", 0xE5),
    ("ae", 0xE6),
    ("ccedilla", 0xE7),
    ("egrave", 0xE8),
    ("eacute", 0xE9),
    ("ecircumflex", 0xEA),
    ("edieresis", 0xEB),
    ("igrave", 0xEC),
    ("iacute", 0xED),
    ("icircumflex", 0xEE),
    ("idieresis", 0xEF),
    ("eth", 0xF0),
    ("ntilde", 0xF1),
    ("ograve", 0xF2),
    ("oacute", 0xF3),
    ("ocircumflex", 0xF4),
    ("otilde", 0xF5),
    ("odieresis", 0xF6),
    ("divide", 0xF7),
    ("oslash", 0xF8),
    ("ugrave", 0xF9),
    ("uacute", 0xFA),
    ("ucircumflex", 0xFB),
    ("udieresis", 0xFC),
    ("yacute", 0xFD),
    ("thorn", 0xFE),
    ("ydieresis", 0xFF),
    ("OE", 0x152),
    ("oe", 0x153),
    ("Scaron", 0x160),
    ("scaron", 0x161),
    ("Ydieresis", 0x178),
    ("Zcaron", 0x17D),
    ("zcaron", 0x17E),
    ("florin", 0x192),
    ("circumflex", 0x2C6),
    ("tilde", 0x2DC),
    ("endash", 0x2013),
    ("emdash", 0x2014),
    ("quoteleft", 0x2018),
    ("quoteright", 0x2019),
    ("quotesinglbase", 0x201A),
    ("quotedblleft", 0x201C),
    ("quotedblright", 0x201D),
    ("quotedblbase", 0x201E),
    ("dagger", 0x2020),
    ("daggerdbl", 0x2021),
    ("bullet", 0x2022),
    ("ellipsis", 0x2026),
    ("perthousand", 0x2030),
    ("guilsinglleft", 0x2039),
    ("guilsinglright", 0x203A),
    ("Euro", 0x20AC),
    ("trademark", 0x2122),
    ("fi", 0xFB01),
    ("fl", 0xFB02),
    ("dotlessi", 0x131),
    ("Lslash", 0x141),
    ("lslash", 0x142),
    ("fraction", 0x2044),
    ("ring", 0x2DA),
    ("caron", 0x2C7),
    ("breve", 0x2D8),
    ("dotaccent", 0x2D9),
    ("hungarumlaut", 0x2DD),
    ("ogonek", 0x2DB),
    ("minus", 0x2212),
    ("nbspace", 0xA0),
    ("sfthyphen", 0xAD),
    ("middot", 0xB7),
    ("mu1", 0xB5),
];

const CP1252_HIGH: [u32; 32] = [
    0x20AC, 0, 0x201A, 0x192, 0x201E, 0x2026, 0x2020, 0x2021, 0x2C6, 0x2030, 0x160, 0x2039, 0x152, 0, 0x17D, 0, 0, 0x2018, 0x2019, 0x201C, 0x201D,
    0x2022, 0x2013, 0x2014, 0x2DC, 0x2122, 0x161, 0x203A, 0x153, 0, 0x17E, 0x178,
];

/// MacRoman codes 0x80..=0xFF.
const MAC_HIGH: &str =
    "ÄÅÇÉÑÖÜáàâäãåçéèêëíìîïñóòôöõúùûü†°¢£§•¶ß®©™´¨≠ÆØ∞±≤≥¥µ∂∑∏π∫ªºΩæø¿¡¬√ƒ≈∆«»…\u{A0}ÀÃÕŒœ–—“”‘’÷◊ÿŸ⁄€‹›ﬁﬂ‡·‚„‰ÂÊÁËÈÍÎÏÌÓÔ\u{F8FF}ÒÚÛÙıˆ˜¯˘˙˚¸˝˛ˇ";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Base {
    Win,
    Mac,
    Standard,
}

/// The character of `code` in a base encoding.
pub(super) fn base_char(b: Base, code: u8) -> Option<char> {
    let c = u32::from(code);
    if (0x20..0x7F).contains(&c) {
        let c = match (b, c) {
            (Base::Standard, 0x27) => 0x2019,
            (Base::Standard, 0x60) => 0x2018,
            _ => c,
        };
        return char::from_u32(c);
    }
    let u = match b {
        Base::Win => match c {
            0x80..=0x9F => *CP1252_HIGH.get((c - 0x80) as usize)?,
            0xA0..=0xFF => c,
            _ => return None,
        },
        Base::Mac => return MAC_HIGH.chars().nth(c.checked_sub(0x80)? as usize),
        Base::Standard => match c {
            0xA1..=0xA3 => c,
            0xA4 => 0x2044,
            0xA5 => 0xA5,
            0xA6 => 0x192,
            0xA7 => 0xA7,
            0xA8 => 0xA4,
            0xA9 => 0x27,
            0xAA => 0x201C,
            0xAB => 0xAB,
            0xAC => 0x2039,
            0xAD => 0x203A,
            0xAE => 0xFB01,
            0xAF => 0xFB02,
            0xB1 => 0x2013,
            0xB2 => 0x2020,
            0xB3 => 0x2021,
            0xB4 => 0xB7,
            0xB6 => 0xB6,
            0xB7 => 0x2022,
            0xB8 => 0x201A,
            0xB9 => 0x201E,
            0xBA => 0x201D,
            0xBB => 0xBB,
            0xBC => 0x2026,
            0xBD => 0x2030,
            0xBF => 0xBF,
            0xC1 => 0x60,
            0xC2 => 0xB4,
            0xC3 => 0x2C6,
            0xC4 => 0x2DC,
            0xC5 => 0xAF,
            0xC6 => 0x2D8,
            0xC7 => 0x2D9,
            0xC8 => 0xA8,
            0xCA => 0x2DA,
            0xCB => 0xB8,
            0xCD => 0x2DD,
            0xCE => 0x2DB,
            0xCF => 0x2C7,
            0xD0 => 0x2014,
            0xE1 => 0xC6,
            0xE3 => 0xAA,
            0xE8 => 0x141,
            0xE9 => 0xD8,
            0xEA => 0x152,
            0xEB => 0xBA,
            0xF1 => 0xE6,
            0xF5 => 0x131,
            0xF8 => 0x142,
            0xF9 => 0xF8,
            0xFA => 0x153,
            0xFB => 0xDF,
            _ => return None,
        },
    };
    char::from_u32(u)
}

/// The character a glyph name stands for.
pub(super) fn name_char(name: &str) -> Option<char> {
    let mut it = name.chars();
    if let (Some(c), None) = (it.next(), it.next())
        && c.is_ascii_alphabetic()
    {
        return Some(c);
    }
    if let Some((_, u)) = NAMES.iter().find(|(n, _)| *n == name) {
        return char::from_u32(*u);
    }
    let hex = |s: &str| u32::from_str_radix(s, 16).ok().filter(|_| s.bytes().all(|b| b.is_ascii_hexdigit()));
    if let Some(h) = name.strip_prefix("uni").filter(|h| h.len() >= 4) {
        return hex(h.get(..4)?).and_then(char::from_u32);
    }
    if let Some(h) = name.strip_prefix('u').filter(|h| (4..=6).contains(&h.len())) {
        return hex(h).and_then(char::from_u32);
    }
    None
}

/// The glyph name of a character (the reverse of [`name_char`], for ASCII letters the letter).
pub(super) fn char_name(c: char) -> Option<String> {
    if c.is_ascii_alphabetic() {
        return Some(c.to_string());
    }
    NAMES.iter().find(|(_, u)| *u == c as u32).map(|(n, _)| (*n).to_string())
}

/// A simple font's encoding: the glyph name and character of every code.
#[derive(Clone)]
pub(super) struct Encoding {
    pub names: Vec<Option<String>>,
    pub chars: Vec<Option<char>>,
    /// Whether the font dictionary named an encoding or listed differences.
    pub explicit: bool,
}

impl Encoding {
    pub(super) fn from_base(b: Base) -> Self {
        let chars: Vec<Option<char>> = (0..=255u8).map(|c| base_char(b, c)).collect();
        let names = chars.iter().map(|c| c.and_then(char_name)).collect();
        Encoding { names, chars, explicit: false }
    }

    /// The encoding of a font dictionary. `builtin` is the font program's own encoding (a Type 1
    /// font's) used when the dictionary names no base; `std_default` the base then (Standard).
    pub(super) fn load(pdf: &mut Pdf, font: &Dict, builtin: Option<&dyn Fn(u8) -> Option<String>>) -> Encoding {
        let enc = pdf.val(font, "Encoding");
        let (base_name, diffs) = match &enc {
            Obj::Name(n) => (Some(n.clone()), None),
            Obj::Dict(d) => {
                let b = match pdf.val(d, "BaseEncoding") {
                    Obj::Name(n) => Some(n),
                    _ => None,
                };
                let diffs = match pdf.val(d, "Differences") {
                    Obj::Arr(a) => Some(a),
                    _ => None,
                };
                (b, diffs)
            }
            _ => (None, None),
        };
        let mut e = match base_name.as_deref() {
            Some("WinAnsiEncoding") => Encoding::from_base(Base::Win),
            Some("MacRomanEncoding") => Encoding::from_base(Base::Mac),
            Some("StandardEncoding") => Encoding::from_base(Base::Standard),
            _ => match builtin {
                Some(f) => {
                    let names: Vec<Option<String>> = (0..=255u8).map(f).collect();
                    let chars = names.iter().map(|n| n.as_deref().and_then(name_char)).collect();
                    Encoding { names, chars, explicit: false }
                }
                None => Encoding::from_base(Base::Standard),
            },
        };
        e.explicit = base_name.is_some() || diffs.is_some();
        if let Some(diffs) = diffs {
            let mut code = 0usize;
            for o in diffs.iter().take(4096) {
                match pdf.resolve(o) {
                    Obj::Num(n) if n >= 0.0 => code = n as usize,
                    Obj::Name(n) => {
                        if let (Some(a), Some(b)) = (e.names.get_mut(code), e.chars.get_mut(code)) {
                            *b = name_char(&n);
                            *a = Some(n);
                        }
                        code += 1;
                    }
                    _ => {}
                }
            }
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_agree_with_the_standards() {
        assert_eq!(MAC_HIGH.chars().count(), 128);
        assert_eq!(base_char(Base::Win, 0x80), Some('€'));
        assert_eq!(base_char(Base::Win, 0xE9), Some('é'));
        assert_eq!(base_char(Base::Mac, 0x8E), Some('é'));
        assert_eq!(base_char(Base::Standard, 0x27), Some('\u{2019}'));
        assert_eq!(base_char(Base::Standard, 0xAE), Some('\u{FB01}'));
        assert_eq!(name_char("eacute"), Some('é'));
        assert_eq!(name_char("uni20AC"), Some('€'));
        assert_eq!(name_char("A"), Some('A'));
        assert_eq!(char_name('€').as_deref(), Some("Euro"));
        assert_eq!(name_char("nonsense"), None);
    }
}
