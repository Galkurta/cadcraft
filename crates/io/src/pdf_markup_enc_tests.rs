//! Encryption, the stream filters, a rebuilt xref, crop boxes, rich text and text encodings, on
//! hand-built files. The encrypted fixtures' keys and `/U` values were computed by an independent
//! implementation of the published algorithms; the files are sealed here with our own helpers.

use md5::{Digest, Md5};

use super::*;
use crate::pdf_markup::crypt::{cbc, hash_r56, rc4};

// ---------------------------------------------------------------- known answers

#[test]
fn primitives_match_published_vectors() {
    // RC4 (the classic "Key"/"Plaintext" vector).
    assert_eq!(rc4(b"Key", b"Plaintext"), unhex("bbf316e8d940af0ad3"));
    // AES-128 and AES-256 single blocks (FIPS-197 appendix C), zero IV.
    let pt = unhex("00112233445566778899aabbccddeeff");
    let k128 = unhex("000102030405060708090a0b0c0d0e0f");
    assert_eq!(cbc(&k128, &[0; 16], &pt, false).unwrap(), unhex("69c4e0d86a7b0430d8cdb78070b4c55a"));
    let k256 = unhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
    assert_eq!(cbc(&k256, &[0; 16], &pt, false).unwrap(), unhex("8ea2b7ca516745bfeafc49904b496089"));
    assert_eq!(cbc(&k256, &[0; 16], &unhex("8ea2b7ca516745bfeafc49904b496089"), true).unwrap(), pt);
    // The revision 6 hash (values from an independent implementation).
    assert_eq!(hex(&hash_r56(6, b"", b"01234567", b"").unwrap()), "bcc3b3d111aac6352a01169078d572a62dfd00c1838e88fb96d667edafe575f1");
    assert_eq!(hex(&hash_r56(6, b"abc", b"87654321", &[7; 48]).unwrap()), "90e51626d3badc95076ed98a190dacb71a63de3a2e5e0afe649a1f863b62302e");
    assert_eq!(hex(&hash_r56(5, b"", b"", b"").unwrap()), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    // Bad key sizes and short IVs are `None`, not a panic.
    assert!(cbc(&[0; 15], &[0; 16], &pt, false).is_none());
    assert!(cbc(&k128, &[0; 3], &pt, false).is_none());
}

#[test]
fn lzw_decodes_the_documented_example_and_round_trips() {
    // The example of the PDF file format description: 45 45 45 45 45 65 45 45 45 66.
    let want = [45, 45, 45, 45, 45, 65, 45, 45, 45, 66];
    assert_eq!(filters::lzw(&unhex("800b6050220c0c8501"), true, 100).unwrap(), want);
    // A pseudo-random mix of runs and noise, long enough to widen the codes to 12 bits and to
    // force a clear code, with both settings of /EarlyChange.
    let mut x = 12345u32;
    let data: Vec<u8> = (0..30_000)
        .map(|i| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            if (i / 50) % 3 == 0 { (x >> 24) as u8 } else { (x >> 29) as u8 }
        })
        .collect();
    for early in [true, false] {
        assert_eq!(filters::lzw(&lzw_enc(&data, early), early, 1 << 20).unwrap(), data, "early change {early}");
    }
    // The wrong /EarlyChange garbles a stream that long: it must not silently pass as right.
    assert_ne!(filters::lzw(&lzw_enc(&data, true), false, 1 << 20), Some(data.clone()));
    // The output limit is enforced; a missing end-of-data code is tolerated; a code that is not
    // in the table is an error.
    assert!(filters::lzw(&lzw_enc(&data, true), true, 1000).is_none());
    assert_eq!(filters::lzw(&unhex("800b6050220c0c85"), true, 100).unwrap(), want);
    assert!(filters::lzw(&unhex("80 40 80"), true, 100).is_none());
}

#[test]
fn ascii_filters_and_run_length() {
    assert_eq!(filters::ascii_hex(b"48 65 6c6C 6f7>ignored"), b"Hellop");
    assert_eq!(filters::ascii_hex(b"4>"), [0x40]);
    // The example text "Man " is 9jqo^.
    assert_eq!(filters::ascii85(b"<~9jqo^~>").unwrap(), b"Man ");
    assert_eq!(filters::ascii85(b"9j qo^z\n!!~>").unwrap(), b"Man \0\0\0\0\0");
    assert_eq!(filters::ascii85(b"").unwrap(), b"");
    assert!(filters::ascii85(b"9jqo^\x01").is_none());
    assert!(filters::ascii85(b"u!!!!").is_none(), "above 2^32");
    assert!(filters::ascii85(b"9").is_none(), "a lone digit");
    assert_eq!(filters::run_length(&[2, b'a', b'b', b'c', 254, b'x', 128, 9, 9], 100).unwrap(), b"abcxxx");
    assert!(filters::run_length(&[5, b'a'], 100).is_none(), "a literal run cut short");
    assert_eq!(filters::run_length(&[129, 0, 129, 0], 256).unwrap().len(), 256);
    assert!(filters::run_length(&[129, 0, 129, 0], 255).is_none());
    assert!(filters::run_length(&[129, 0, 129, 0, 129, 0], 600).unwrap().len() == 384);
}

#[test]
fn predictors() {
    // TIFF 2 on RGB rows of two pixels: each sample adds the one before it.
    let out = filters::predict(vec![1, 2, 3, 1, 1, 1, 9, 9, 9, 1, 1, 1], 2, 3, 8, 2, 1000).unwrap();
    assert_eq!(out, [1, 2, 3, 2, 3, 4, 9, 9, 9, 10, 10, 10]);
    assert!(filters::predict(vec![0; 4], 2, 1, 16, 2, 1000).is_none(), "TIFF at 16 bits");
    assert!(filters::predict(vec![0; 4], 5, 1, 8, 2, 1000).is_none(), "unknown predictor");
    assert_eq!(filters::predict(vec![7, 7], 1, 1, 8, 2, 1000).unwrap(), [7, 7]);
    // PNG Sub and Paeth rows (colors 1, 8 bits).
    assert_eq!(filters::predict(vec![1, 1, 1, 1, 1], 11, 1, 8, 4, 1000).unwrap(), [1, 2, 3, 4]);
    assert_eq!(filters::predict(vec![4, 5, 5, 5, 5], 15, 1, 8, 4, 1000).unwrap(), [5, 10, 15, 20]);
    assert!(filters::predict(vec![9, 1, 1], 12, 1, 8, 2, 1000).is_none(), "bad row filter");
    assert!(filters::predict(vec![0], 12, 1, 8, 0, 1000).is_none());
    assert!(filters::predict(vec![0], 12, 1 << 40, 1 << 40, 1 << 40, 1000).is_none());
}

// ---------------------------------------------------------------- encoders for the fixtures

fn unhex(s: &str) -> Vec<u8> {
    let d: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
    d.chunks(2).map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap()).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex_enc(d: &[u8]) -> Vec<u8> {
    format!("{}>", hex(d)).into_bytes()
}

fn a85_enc(d: &[u8]) -> Vec<u8> {
    let mut out = String::from("<~");
    for chunk in d.chunks(4) {
        let v = chunk.iter().enumerate().fold(0u32, |a, (i, b)| a | (u32::from(*b) << (24 - 8 * i)));
        if chunk.len() == 4 && v == 0 {
            out.push('z');
            continue;
        }
        let (mut digits, mut x) = ([0u8; 5], v);
        for i in (0..5).rev() {
            digits[i] = (x % 85) as u8 + b'!';
            x /= 85;
        }
        out.extend(digits.iter().take(chunk.len() + 1).map(|d| *d as char));
    }
    out.push_str("~>");
    out.into_bytes()
}

fn rl_enc(d: &[u8]) -> Vec<u8> {
    let (mut out, mut i) = (Vec::new(), 0);
    while i < d.len() {
        let b = d[i];
        let run = d[i..].iter().take(128).take_while(|x| **x == b).count();
        if run >= 3 {
            out.extend([(257 - run) as u8, b]);
            i += run;
            continue;
        }
        let start = i;
        i += run;
        while i < d.len() && i - start < 128 {
            let c = d[i];
            if d[i..].iter().take(3).take_while(|x| **x == c).count() >= 3 {
                break;
            }
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend(&d[start..i]);
    }
    out.push(128);
    out
}

fn lzw_enc(d: &[u8], early: bool) -> Vec<u8> {
    use std::collections::HashMap;
    let e = usize::from(early);
    // The decoder is one table entry behind the encoder, so the code width follows the number
    // of codes sent, less one.
    let width = |sent: usize| match 258 + sent.saturating_sub(1) + e {
        2048.. => 12,
        1024.. => 11,
        512.. => 10,
        _ => 9,
    };
    let (mut codes, mut sent, mut next) = (vec![(256u32, 9u32)], 0usize, 258u32);
    let mut table: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut w: Vec<u8> = Vec::new();
    for &c in d {
        let mut wc = w.clone();
        wc.push(c);
        if wc.len() == 1 || table.contains_key(&wc) {
            w = wc;
            continue;
        }
        codes.push((if w.len() == 1 { u32::from(w[0]) } else { table[&w] }, width(sent)));
        sent += 1;
        table.insert(wc, next);
        next += 1;
        w = vec![c];
        if sent > 3800 {
            codes.push((256, width(sent)));
            (sent, next) = (0, 258);
            table.clear();
        }
    }
    if !w.is_empty() {
        codes.push((if w.len() == 1 { u32::from(w[0]) } else { table[&w] }, width(sent)));
        sent += 1;
    }
    codes.push((257, width(sent)));
    let (mut out, mut acc, mut have) = (Vec::new(), 0u64, 0u32);
    for (code, bits) in codes {
        acc = (acc << bits) | u64::from(code);
        have += bits;
        while have >= 8 {
            out.push((acc >> (have - 8)) as u8);
            have -= 8;
            acc &= (1 << have) - 1;
        }
    }
    if have > 0 {
        out.push((acc << (8 - have)) as u8);
    }
    out
}

// ---------------------------------------------------------------- filters in files

#[test]
fn every_filter_and_chain_reads_xref_streams_and_object_streams() {
    let lzw1 = |d: &[u8]| lzw_enc(d, true);
    let lzw0 = |d: &[u8]| lzw_enc(d, false);
    let a85_lzw = |d: &[u8]| a85_enc(&lzw_enc(d, true));
    let hex_rl_flate = |d: &[u8]| hex_enc(&rl_enc(&flate(d)));
    let a85_flate_rl = |d: &[u8]| a85_enc(&flate(&rl_enc(d)));
    let up = "/DecodeParms << /Predictor 12 /Columns 4 >>";
    let specs = [
        Spec { filter: "/LZWDecode", xparms: up, sparms: "", enc: &lzw1, predict: true, crypt: None },
        Spec {
            filter: "/LZW",
            xparms: "/DecodeParms << /Predictor 12 /Columns 4 /EarlyChange 0 >>",
            sparms: "/DecodeParms << /EarlyChange 0 >>",
            enc: &lzw0,
            predict: true,
            crypt: None,
        },
        Spec { filter: "/ASCIIHexDecode", xparms: "", sparms: "", enc: &hex_enc, predict: false, crypt: None },
        Spec { filter: "/ASCII85Decode", xparms: "", sparms: "", enc: &a85_enc, predict: false, crypt: None },
        Spec { filter: "/RunLengthDecode", xparms: "", sparms: "", enc: &rl_enc, predict: false, crypt: None },
        Spec {
            filter: "[/ASCII85Decode /LZWDecode]",
            xparms: "/DecodeParms [null << /Predictor 12 /Columns 4 >>]",
            sparms: "",
            enc: &a85_lzw,
            predict: true,
            crypt: None,
        },
        // Abbreviated names, and a parameter array shorter than the filter array.
        Spec {
            filter: "[/AHx /RL /Fl]",
            xparms: "/DecodeParms [null null << /Predictor 12 /Columns 4 >>]",
            sparms: "",
            enc: &hex_rl_flate,
            predict: true,
            crypt: None,
        },
        Spec { filter: "[/A85 /Fl /RL]", xparms: "", sparms: "", enc: &a85_flate_rl, predict: false, crypt: None },
    ];
    for (i, s) in specs.iter().enumerate() {
        check(&pdf_markups(&compressed_with(&OBJS, s), 1).unwrap_or_else(|e| panic!("spec {i}: {e}")));
    }
}

#[test]
fn image_filters_are_skipped_not_fatal_in_content() {
    // A page whose first content stream is a JPEG (an image filter) and second is readable.
    let o = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Count 1 /Kids [3 0 R] /MediaBox [0 0 200 100] >>",
        "<< /Type /Page /Parent 2 0 R /Contents [4 0 R 5 0 R] >>",
        "<< /Filter /DCTDecode /Length 3 >>\nstream\nabc\nendstream",
        "<< /Filter /ASCIIHexDecode /Length 11 >>\nstream\n30203020 6d>\nendstream",
    ];
    let c = pdf_page_content(&classic(&o), 1, "").unwrap();
    assert_eq!(c.content, b"0 0 m");
}

// ---------------------------------------------------------------- encryption

struct Fixture {
    v: i64,
    r: i64,
    len: i64,
    p: i64,
    o: &'static str,
    u: &'static str,
    ue: &'static str,
    key: &'static str,
    id: &'static str,
    pw: &'static str,
}

const RC4_40: Fixture = Fixture {
    v: 1,
    r: 2,
    len: 40,
    p: -4,
    o: "030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dc",
    u: "4eed1d10463a176a373d5bef76374d30d4d37c2cd8fd59b7f56bcf014c910d7e",
    ue: "",
    key: "32a1beef16",
    id: "6465666768696a6b6c6d6e6f70717273",
    pw: "",
};
const RC4_128: Fixture = Fixture {
    v: 2,
    r: 3,
    len: 128,
    p: -3904,
    o: "030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dc",
    u: "cfb038019d32e7381497ee031178443f00000000000000000000000000000000",
    ue: "",
    key: "96be17f98a7b84ab9235fd08e9b0e1d6",
    id: "6465666768696a6b6c6d6e6f70717273",
    pw: "",
};
const RC4_128_PW: Fixture = Fixture {
    v: 2,
    r: 3,
    len: 128,
    p: -3904,
    o: "030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dc",
    u: "042986b940dee97a1e03c4bd38a5a54200000000000000000000000000000000",
    ue: "",
    key: "97208d7b3fcca9d7b8eaec012b4470c2",
    id: "6465666768696a6b6c6d6e6f70717273",
    pw: "secret",
};
const AES_128: Fixture = Fixture {
    v: 4,
    r: 4,
    len: 128,
    p: -3904,
    o: "030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dc",
    u: "cfb038019d32e7381497ee031178443f00000000000000000000000000000000",
    ue: "",
    key: "96be17f98a7b84ab9235fd08e9b0e1d6",
    id: "6465666768696a6b6c6d6e6f70717273",
    pw: "",
};
const AES_256_R5: Fixture = Fixture {
    v: 5,
    r: 5,
    len: 256,
    p: -3904,
    o: "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    u: "133c8eb86cf813474ade739d5d133087e2026f56aaf366284dd1a25d98d4469076616c69646174656b657973616c7421",
    ue: "1c66e90702ad5a8ec89aba0c8c46490619557e034d72bcc5d03a316e35f64f9d",
    key: "05121f2c394653606d7a8794a1aebbc8d5e2effc091623303d4a5764717e8b98",
    id: "6465666768696a6b6c6d6e6f70717273",
    pw: "",
};
const AES_256_R6: Fixture = Fixture {
    v: 5,
    r: 6,
    len: 256,
    p: -3904,
    o: "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    u: "936af0029db0e0a9d1c4382fd0c4a3ac26e9e12c15a9d8f3fcdeeea0d2c335fa76616c69646174656b657973616c7421",
    ue: "c67cc8cbe4e107d57051c8f178ec922746667488c3a05d0c45f85411978c5af2",
    key: "05121f2c394653606d7a8794a1aebbc8d5e2effc091623303d4a5764717e8b98",
    id: "6465666768696a6b6c6d6e6f70717273",
    pw: "",
};
const AES_256_R6_PW: Fixture = Fixture {
    v: 5,
    r: 6,
    len: 256,
    p: -3904,
    o: "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    u: "ad837fb5d71d1c1be9032cc17d6b593c0a359c77a23def3de7b63f88ce66495476616c69646174656b657973616c7421",
    ue: "e502a9488d472325a3f885c42485e2647b5a3e637d8d1c4c8de932ee5b41430a",
    key: "05121f2c394653606d7a8794a1aebbc8d5e2effc091623303d4a5764717e8b98",
    id: "6465666768696a6b6c6d6e6f70717273",
    pw: "open sesame",
};

const EMPTY_PASSWORD: [&Fixture; 5] = [&RC4_40, &RC4_128, &AES_128, &AES_256_R5, &AES_256_R6];

/// A string or stream of object `id`, encrypted with the fixture's file key (a fixed IV for AES).
fn seal(fx: &Fixture, id: u32, data: &[u8]) -> Vec<u8> {
    let key = unhex(fx.key);
    let aes = fx.v >= 4;
    let k = if fx.v == 5 {
        key
    } else {
        let mut m = Md5::new();
        m.update(&key);
        m.update([id as u8, (id >> 8) as u8, (id >> 16) as u8, 0, 0]);
        if aes {
            m.update(b"sAlT");
        }
        m.finalize()[..(key.len() + 5).min(16)].to_vec()
    };
    if !aes {
        return rc4(&k, data);
    }
    let mut padded = data.to_vec();
    let pad = 16 - data.len() % 16;
    padded.extend(std::iter::repeat_n(pad as u8, pad));
    let iv = [9u8; 16];
    let mut out = iv.to_vec();
    out.extend(cbc(&k, &iv, &padded, false).unwrap());
    out
}

/// The `/Encrypt` dictionary; `stmf` is the name of the stream crypt filter.
fn encrypt_dict(fx: &Fixture, stmf: &str) -> String {
    let mut d = format!("<< /Filter /Standard /V {} /R {} /Length {} /P {} /O <{}> /U <{}>", fx.v, fx.r, fx.len, fx.p, fx.o, fx.u);
    if fx.v >= 4 {
        let (cfm, n) = if fx.v == 5 { ("AESV3", 32) } else { ("AESV2", 16) };
        d += &format!(" /CF << /StdCF << /CFM /{cfm} /Length {n} /AuthEvent /DocOpen >> >> /StmF {stmf} /StrF /StdCF");
    }
    if fx.v == 5 {
        d += &format!(" /UE <{}> /OE <{}> /Perms <{}>", fx.ue, hex(&[0; 32]), hex(&[0; 16]));
    }
    d + " >>"
}

/// `(text)` strings of an object, encrypted as hex strings (the fixtures use no escapes).
fn seal_strings(obj: &str, id: u32, fx: &Fixture) -> String {
    let (mut out, mut it) = (String::new(), obj.chars());
    while let Some(c) = it.next() {
        if c != '(' {
            out.push(c);
            continue;
        }
        let text: String = it.by_ref().take_while(|c| *c != ')').collect();
        out += &format!("<{}>", hex(&seal(fx, id, text.as_bytes())));
    }
    out
}

fn trailer_extra(fx: &Fixture, encrypt: &str) -> String {
    format!("/Encrypt {encrypt} /ID [<{0}> <{0}>]", fx.id)
}

/// A classic file with the strings sealed and the `/Encrypt` dictionary as an indirect object.
fn classic_enc(objs: &[&str], fx: &Fixture) -> Vec<u8> {
    let mut all: Vec<String> = objs.iter().enumerate().map(|(i, o)| seal_strings(o, i as u32 + 1, fx)).collect();
    all.push(encrypt_dict(fx, "/StdCF"));
    let refs: Vec<&str> = all.iter().map(String::as_str).collect();
    classic_extra(&refs, &trailer_extra(fx, &format!("{} 0 R", all.len())))
}

/// An object-stream file: the object stream sealed, `/Encrypt` direct in the xref stream.
fn compressed_enc(objs: &[&str], fx: &Fixture, stmf: &str, seal_streams: bool) -> Vec<u8> {
    let sealer = |id: u32, d: &[u8]| if seal_streams { seal(fx, id, d) } else { d.to_vec() };
    compressed_with(objs, &Spec { crypt: Some((&sealer, trailer_extra(fx, &encrypt_dict(fx, stmf)))), ..FLATE_UP })
}

#[test]
fn encrypted_files_with_an_empty_password_read_like_the_plain_one() {
    let plain = pdf_markups(&classic(&OBJS), 1).unwrap().markups;
    for fx in EMPTY_PASSWORD {
        let m = pdf_markups(&classic_enc(&OBJS, fx), 1).unwrap_or_else(|e| panic!("classic V{} R{}: {e}", fx.v, fx.r));
        assert_eq!(m.markups, plain, "classic V{} R{}", fx.v, fx.r);
        check(&m);
        let m = pdf_markups(&compressed_enc(&OBJS, fx, "/StdCF", true), 1).unwrap_or_else(|e| panic!("compressed V{} R{}: {e}", fx.v, fx.r));
        assert_eq!(m.markups, plain, "compressed V{} R{}", fx.v, fx.r);
    }
}

#[test]
fn the_ciphertext_is_really_unreadable_without_decrypting() {
    // Guards the fixtures: were the strings left plain, the tests above would prove nothing.
    for fx in EMPTY_PASSWORD {
        let f = classic_enc(&OBJS, fx);
        assert!(!String::from_utf8_lossy(&f).contains("Check this"));
    }
    assert!(String::from_utf8_lossy(&classic(&OBJS)).contains("Check this"));
}

#[test]
fn passwords_are_checked() {
    for fx in [&RC4_128_PW, &AES_256_R6_PW] {
        let file = classic_enc(&OBJS, fx);
        for wrong in ["", "nope", "secre", "secret ", "OPEN SESAME"] {
            if wrong == fx.pw {
                continue;
            }
            let e = crate::pdf_markup::pdf_markups(&file, 1, wrong).unwrap_err().to_string();
            assert!(e.contains("password"), "{e}");
        }
        check(&crate::pdf_markup::pdf_markups(&file, 1, fx.pw).unwrap());
        // A wrong password is not retried as a damaged file, nor does it leak content.
        let e = crate::pdf_markup::pdf_markups(&compressed_enc(&OBJS, fx, "/StdCF", true), 1, "x").unwrap_err().to_string();
        assert!(e.contains("password"), "{e}");
    }
    // A password given for a file that has none is ignored.
    check(&crate::pdf_markup::pdf_markups(&classic(&OBJS), 1, "anything").unwrap());
    // A very long wrong password is just wrong.
    assert!(crate::pdf_markup::pdf_markups(&classic_enc(&OBJS, &AES_256_R6), 1, &"x".repeat(5000)).is_err());
}

#[test]
fn identity_crypt_filters_leave_streams_alone() {
    // StmF /Identity: the object stream is stored plain, the strings are still encrypted.
    check(&pdf_markups(&compressed_enc(&OBJS, &AES_128, "/Identity", false), 1).unwrap());
    // A stream that names the /Identity crypt filter itself, in a file whose default encrypts.
    let id_chain = Spec {
        filter: "[/Crypt /FlateDecode]",
        sparms: "/DecodeParms [<< /Type /CryptFilterDecodeParms /Name /Identity >> null]",
        xparms: "/DecodeParms [null << /Predictor 12 /Columns 4 >>]",
        enc: &flate,
        predict: true,
        crypt: Some((&|_, d: &[u8]| d.to_vec(), trailer_extra(&AES_128, &encrypt_dict(&AES_128, "/StdCF")))),
    };
    check(&pdf_markups(&compressed_with(&OBJS, &id_chain), 1).unwrap());
    // The same stream sealed with the default filter would not read through /Identity.
    let sealed =
        Spec { crypt: Some((&|id, d: &[u8]| seal(&AES_128, id, d), trailer_extra(&AES_128, &encrypt_dict(&AES_128, "/StdCF")))), ..id_chain };
    assert!(pdf_markups(&compressed_with(&OBJS, &sealed), 1).is_err());
}

#[test]
fn public_key_and_unknown_handlers_are_refused() {
    let file = |dict: &str| classic_extra(&OBJS, &format!("/Encrypt << {dict} >>"));
    let e = pdf_markups(&file("/Filter /Adobe.PubSec /SubFilter /adbe.pkcs7.s5 /V 4 /CF << >>"), 1).unwrap_err().to_string();
    assert!(e.contains("public-key"), "{e}");
    let e = pdf_markups(&file("/V 2 /R 3"), 1).unwrap_err().to_string();
    assert!(e.contains("public-key"), "no /Filter: {e}");
    for bad in [
        "/Filter /Standard /V 3 /R 3 /O <00> /U <00>",
        "/Filter /Standard /V 9 /R 3",
        "/Filter /Standard /V 4 /R 9 /O <00> /U <00>",
        "/Filter /Standard /V 1 /R 2",
        "/Filter /Standard /V 2 /R 3 /O <0011> /U <0011>",
        "/Filter /Standard /V 5 /R 6 /U <00> /UE <00>",
        "/Filter /Standard /V 5 /R 4 /U <00> /UE <00>",
        "/Filter /Standard /V 4 /R 4 /CF << /X << /CFM /Weird >> >> /O <00> /U <00>",
        "/Filter /Standard /V 2 /R 3 /Length 99999999999999999999 /P 99999999999999 /O <00> /U <00>",
    ] {
        assert!(pdf_markups(&file(bad), 1).is_err(), "{bad}");
    }
}

// ---------------------------------------------------------------- a rebuilt xref

/// The first place `needle` is in `f`.
fn at(f: &[u8], needle: &str) -> usize {
    f.windows(needle.len()).position(|w| w == needle.as_bytes()).unwrap()
}

/// `f` without the bytes from `from` up to (not including) `to`.
fn cut(f: &[u8], from: &str, to: &str) -> Vec<u8> {
    let a = at(f, from);
    let b = a + at(&f[a..], to);
    [&f[..a], &f[b..]].concat()
}

/// `f` with its `startxref` offset replaced by one that leads nowhere useful.
fn bad_startxref(f: &[u8]) -> Vec<u8> {
    let i = f.windows(10).rposition(|w| w == b"startxref\n").unwrap();
    [&f[..i], b"startxref\n3\n%%EOF"].concat()
}

#[test]
fn a_missing_or_wrong_xref_is_rebuilt() {
    let file = classic(&OBJS);
    let text = String::from_utf8_lossy(&file).into_owned();
    // The xref table deleted (startxref now points into the trailer).
    check(&pdf_markups(&cut(&file, "xref\n0", "trailer"), 1).unwrap());
    // Everything from the xref table on deleted: no trailer, the catalog is found by scanning.
    check(&pdf_markups(&file[..text.find("xref\n0").unwrap()], 1).unwrap());
    // startxref pointing nowhere, and one pointing past the end.
    check(&pdf_markups(text.replace("startxref\n", "startxref\n9").as_bytes(), 1).unwrap());
    check(&pdf_markups(text.replace("startxref\n", "startxref\n99999").as_bytes(), 1).unwrap());
    // Every offset shifted by an extra line after the header.
    let shifted = text.replacen("%PDF-1.4\n", "%PDF-1.4\n%shifted by a comment line\n", 1);
    check(&pdf_markups(shifted.as_bytes(), 1).unwrap());
    // Junk before the header does not move the offsets (they count from the header).
    check(&pdf_markups(&[b"junk\n\n".as_slice(), &file].concat(), 1).unwrap());
}

#[test]
fn a_rebuilt_xref_prefers_the_later_object_and_the_last_trailer() {
    let file = classic(&OBJS);
    let text = String::from_utf8_lossy(&file).into_owned();
    // An update appended without an xref: the FreeText annotation becomes a plain rectangle.
    let updated =
        [&file[..text.find("xref\n0").unwrap()], b"4 0 obj\n<< /Type /Annot /Subtype /Square /Rect [10 10 60 40] >>\nendobj\n".as_slice()].concat();
    let m = pdf_markups(&updated, 1).unwrap();
    let kinds: Vec<&str> = m.markups.iter().map(|v| v["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["rect", "cloud", "text", "polyline", "leader"]);
    // Two trailers: the last one wins over an earlier one that names a missing root.
    let head = &file[..text.find("xref\n0").unwrap()];
    let (bogus, good) = (b"trailer\n<< /Root 7 0 R >>\n".as_slice(), b"trailer\n<< /Root 1 0 R >>\n".as_slice());
    check(&pdf_markups(&[head, bogus, good].concat(), 1).unwrap());
    assert!(pdf_markups(&[head, good, bogus].concat(), 1).is_err());
}

#[test]
fn a_rebuilt_xref_finds_object_streams_and_a_catalog_inside_one() {
    let file = compressed(&OBJS);
    // The xref stream and startxref deleted: no trailer at all, the catalog sits in the object
    // stream.
    check(&pdf_markups(&cut(&file, "9 0 obj\n<< /Type /XRef", "%%EOF"), 1).unwrap());
    // The xref stream kept but the offset in startxref wrong.
    check(&pdf_markups(&bad_startxref(&file), 1).unwrap());
}

#[test]
fn encrypted_files_with_a_damaged_xref_are_rebuilt_and_decrypted() {
    for fx in EMPTY_PASSWORD {
        let file = classic_enc(&OBJS, fx);
        check(&pdf_markups(&cut(&file, "xref\n0", "trailer"), 1).unwrap_or_else(|e| panic!("V{} R{}: {e}", fx.v, fx.r)));
    }
    let file = compressed_enc(&OBJS, &AES_256_R6, "/StdCF", true);
    check(&pdf_markups(&bad_startxref(&file), 1).unwrap());
    // A wrong password is still a password error, not "damaged".
    let e = crate::pdf_markup::pdf_markups(&cut(&classic_enc(&OBJS, &RC4_128_PW), "xref\n0", "trailer"), 1, "bad").unwrap_err().to_string();
    assert!(e.contains("password"), "{e}");
}

#[test]
fn nothing_to_rebuild_from_is_an_error() {
    assert!(pdf_markups(b"%PDF-1.4\n1 0 obj\n<< /A 1 >>\nendobj\n", 1).is_err());
    assert!(pdf_markups(b"%PDF-1.4\n", 1).is_err());
    assert!(pdf_markups(b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 9 0 R >> endobj", 1).is_err());
    // The page that does not exist is reported the same way after a rebuild.
    let file = classic(&OBJS);
    let text = String::from_utf8_lossy(&file).into_owned();
    let e = pdf_markups(&file[..text.find("xref\n0").unwrap()], 5).unwrap_err().to_string();
    assert!(e.contains("1 page"), "{e}");
}

// ---------------------------------------------------------------- crop box

fn page_with(page_extra: &str, pages_extra: &str) -> Vec<u8> {
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Count 1 /Kids [3 0 R] /MediaBox [0 0 200 100] {pages_extra} >>"),
        format!("<< /Type /Page /Parent 2 0 R /Annots [4 0 R] {page_extra} >>"),
        "<< /Type /Annot /Subtype /FreeText /Rect [30 40 80 60] /Contents (Hi) >>".to_string(),
    ];
    classic(&objs.iter().map(String::as_str).collect::<Vec<_>>())
}

#[test]
fn the_crop_box_is_the_page() {
    // The crop box [10 20 110 70] is 100 x 50; the note's top left (30, 60) is 20 right of and
    // 10 below its top left.
    for f in [page_with("/CropBox [10 20 110 70]", ""), page_with("", "/CropBox [10 20 110 70]"), page_with("/CropBox [110 70 10 20]", "")] {
        let m = pdf_markups(&f, 1).unwrap();
        assert_eq!((m.width, m.height), (100.0, 50.0));
        near(&m.markups[0]["at"], &[0.2, 0.2]);
        let c = pdf_page_content(&f, 1, "").unwrap();
        assert_eq!((c.x0, c.y0, c.w, c.h, c.rotate), (10.0, 20.0, 100.0, 50.0, 0));
    }
    // The page's own /CropBox wins over an inherited one.
    let m = pdf_markups(&page_with("/CropBox [0 0 100 100]", "/CropBox [10 20 110 70]"), 1).unwrap();
    assert_eq!((m.width, m.height), (100.0, 100.0));
    // The crop box is cut to the media box; one that misses it or is empty or damaged is ignored.
    let m = pdf_markups(&page_with("/CropBox [100 50 300 300]", ""), 1).unwrap();
    assert_eq!((m.width, m.height), (100.0, 50.0));
    for crop in [
        "/CropBox [-50 -50 300 300]",
        "/CropBox [500 500 600 600]",
        "/CropBox [5 5 5 50]",
        "/CropBox [1 2 3]",
        "/CropBox (x)",
        "/CropBox [0 0 0.0000001 1]",
    ] {
        let m = pdf_markups(&page_with(crop, ""), 1).unwrap();
        assert_eq!((m.width, m.height), (200.0, 100.0), "{crop}");
    }
    // The turn applies to the cropped page.
    let m = pdf_markups(&page_with("/CropBox [10 20 110 70] /Rotate 90", ""), 1).unwrap();
    assert_eq!((m.width, m.height), (50.0, 100.0));
}

// ---------------------------------------------------------------- page content

#[test]
fn page_content_is_decoded_and_joined() {
    let o = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Count 1 /Kids [3 0 R] /MediaBox [0 0 200 100] /Rotate 270 >>",
        "<< /Type /Page /Parent 2 0 R /Contents [4 0 R 5 0 R 9 0 R] >>",
        "<< /Length 7 >>\nstream\n0 0 m S\nendstream",
        "<< /Filter /ASCIIHexDecode /Length 11 >>\nstream\n312031206c53>\nendstream",
    ];
    let c = pdf_page_content(&classic(&o), 1, "").unwrap();
    assert_eq!(c.content, b"0 0 m S\n1 1 lS");
    assert_eq!((c.page_count, c.x0, c.y0, c.w, c.h, c.rotate), (1, 0.0, 0.0, 200.0, 100.0, 270));
    // A single stream, no stream, and a bad page.
    let one = [o[0], o[1], "<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>", o[3]];
    assert_eq!(pdf_page_content(&classic(&one), 1, "").unwrap().content, b"0 0 m S");
    let none = [o[0], o[1], "<< /Type /Page /Parent 2 0 R >>"];
    assert!(pdf_page_content(&classic(&none), 1, "").unwrap().content.is_empty());
    assert!(pdf_page_content(&classic(&none), 2, "").is_err());
    assert!(pdf_page_content(&classic(&none), 0, "").is_err());
    assert!(pdf_page_content(b"nope", 1, "").is_err());
}

#[test]
fn encrypted_page_content_is_decrypted() {
    // A page's content stream is a direct stream (never in an object stream): seal it by hand.
    let fx = &AES_128;
    let body = seal(fx, 4, b"0 0 m 5 5 l S");
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Count 1 /Kids [3 0 R] /MediaBox [0 0 200 100] >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec(),
        [format!("<< /Length {} >>\nstream\n", body.len()).as_bytes(), &body, b"\nendstream"].concat(),
    ];
    objs.push(encrypt_dict(fx, "/StdCF").into_bytes());
    let mut f = b"%PDF-1.4\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(f.len());
        f.extend(format!("{} 0 obj\n", i + 1).bytes());
        f.extend(o);
        f.extend(b"\nendobj\n");
    }
    let xref = f.len();
    f.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
    for o in offs {
        f.extend(format!("{o:010} 00000 n \n").bytes());
    }
    f.extend(format!("trailer\n<< /Size 6 /Root 1 0 R {} >>\nstartxref\n{xref}\n%%EOF\n", trailer_extra(fx, "5 0 R")).bytes());
    assert_eq!(pdf_page_content(&f, 1, "").unwrap().content, b"0 0 m 5 5 l S");
}

// ---------------------------------------------------------------- rich text and strings

fn free_text(extra: &str) -> Vec<u8> {
    let o = format!("<< /Type /Annot /Subtype /FreeText /Rect [20 60 120 90] {extra} >>");
    let mut objs = OBJS;
    objs[2] = "<< /Type /Page /Parent 2 0 R /Annots [4 0 R] >>";
    classic(&[objs[0], objs[1], objs[2], &o])
}

fn text_of(extra: &str) -> Option<String> {
    pdf_markups(&free_text(extra), 1).unwrap().markups.first().map(|m| m["text"].as_str().unwrap().to_string())
}

#[test]
fn rich_text_alone_gives_the_text() {
    let rc = "(<?xml version=\"1.0\"?><body xmlns=\"http://www.w3.org/1999/xhtml\" style=\"font:12pt Helv\"><p dir=\"ltr\"><span style=\"color:#FF0000\">Line one &amp; two</span></p>\
              <p>Second&#33;&#x21; &lt;ok&gt;<br/>third</p></body>)";
    assert_eq!(text_of(&format!("/RC {rc}")).unwrap(), "Line one & two\nSecond!! <ok>\nthird");
    // As a stream (UTF-8), and as a stream with a byte order mark (UTF-16BE).
    let s = "<body><p>caf\u{e9} \u{2713}</p></body>";
    let mut o = OBJS;
    o[2] = "<< /Type /Page /Parent 2 0 R /Annots [4 0 R] >>";
    let utf8 = format!("<< /Length {} >>\nstream\n{s}\nendstream", s.len());
    let f = classic(&[o[0], o[1], o[2], "<< /Type /Annot /Subtype /FreeText /Rect [20 60 120 90] /RC 5 0 R >>", &utf8]);
    assert_eq!(pdf_markups(&f, 1).unwrap().markups[0]["text"], "caf\u{e9} \u{2713}");
    let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain(s.encode_utf16().flat_map(u16::to_be_bytes)).collect();
    let mut f =
        classic(&[o[0], o[1], o[2], "<< /Type /Annot /Subtype /FreeText /Rect [20 60 120 90] /RC 5 0 R >>", "<< /Length 1 >>\nstream\nX\nendstream"]);
    let t = String::from_utf8_lossy(&f).into_owned();
    let i = t.find("X\nendstream").unwrap();
    f.splice(i..i + 1, be.iter().copied());
    // The declared /Length is wrong now; the reader falls back to the `endstream` keyword.
    assert_eq!(pdf_markups(&f, 1).unwrap().markups[0]["text"], "caf\u{e9} \u{2713}");
    // Rich text with no readable text gives no markup.
    assert_eq!(text_of("/RC (<body><p> </p></body>)"), None);
}

#[test]
fn contents_is_preferred_when_it_agrees_with_the_rich_text() {
    // Same words: /Contents keeps its own line breaks and spacing.
    assert_eq!(text_of("/Contents (Hello,\\nthere  world) /RC (<body><p>Hello, there world</p></body>)").unwrap(), "Hello,\nthere  world");
    // Different words: the rich text is what a viewer shows.
    assert_eq!(text_of("/Contents (stale text) /RC (<body><p>fresh text</p></body>)").unwrap(), "fresh text");
    // An empty or tag-only rich text leaves /Contents.
    assert_eq!(text_of("/Contents (kept) /RC (<body><p></p></body>)").unwrap(), "kept");
    assert_eq!(text_of("/Contents (kept) /RC ()").unwrap(), "kept");
    assert_eq!(text_of("/Contents (kept)").unwrap(), "kept");
}

#[test]
fn rich_text_conversion_is_lenient() {
    let p = rich::plain;
    assert_eq!(p("plain"), "plain");
    assert_eq!(p("<p>a</p><p>b</p>"), "a\nb");
    assert_eq!(p("<div>a<br>b<br />  c</div>"), "a\nb\nc");
    assert_eq!(p("<p>  many   spaces\n and\tlines </p>"), "many spaces and lines");
    assert_eq!(p("a &amp;amp; b &nbsp;c"), "a &amp; b c");
    assert_eq!(p("&#65;&#x42;&#X43; &bogus; &#xZZ; &#1114112; &amp"), "ABC &bogus; &#xZZ; &#1114112; &amp");
    assert_eq!(p("<p>open<"), "open");
    assert_eq!(p("<p>x</p"), "x");
    assert_eq!(p("<!-- c --><p>y</p>"), "y");
    assert_eq!(p(""), "");
    assert_eq!(p("<p></p><p> </p>"), "");
    assert_eq!(p("<ul><li>one</li><li>two</li></ul>"), "one\ntwo");
    assert_eq!(p("<P>Upper</P><BR>"), "Upper");
}

#[test]
fn text_strings_decode_per_their_encoding() {
    // UTF-16BE with a byte order mark, including a surrogate pair (U+1D11E).
    assert_eq!(pdf_text(&unhex("FEFF 041F 0440 0438 0432 0456 0442 D834 DD1E")), "\u{41f}\u{440}\u{438}\u{432}\u{456}\u{442}\u{1d11e}");
    // A language escape (U+001B en U+001B) is not text; a lone half surrogate and an odd byte
    // are replaced or dropped.
    assert_eq!(pdf_text(&unhex("FEFF 001B 0065 006E 001B 0041")), "A");
    assert_eq!(pdf_text(&unhex("FEFF D834 0041")), "\u{fffd}A");
    assert_eq!(pdf_text(&unhex("FEFF 0041 00")), "A");
    assert_eq!(pdf_text(&unhex("FEFF")), "");
    // UTF-8 with a byte order mark.
    assert_eq!(pdf_text(&unhex("EFBBBF C3A9")), "\u{e9}");
    // PDFDocEncoding: the bullet and the euro, a spacing accent, Latin-1 above 0xA0, ASCII.
    assert_eq!(pdf_text(&unhex("80 A0 18 E9 41 83 8D 9F")), "\u{2022}\u{20ac}\u{2d8}\u{e9}A\u{2026}\u{201c}\u{fffd}");
    // Line ends become line feeds.
    assert_eq!(pdf_text(b"a\r\nb\rc"), "a\nb\nc");
    // Through an annotation: a hex string in UTF-16BE.
    assert_eq!(text_of("/Contents <FEFF 0048 0069 2713>").unwrap(), "Hi\u{2713}");
}

// ---------------------------------------------------------------- hostile input

/// Every truncation, and a few byte flips at every position (every `stride`-th for the slow
/// ones): never a panic.
fn fuzz(file: &[u8], stride: usize) {
    for n in (0..file.len()).step_by(stride) {
        let _ = crate::pdf_markup::pdf_markups(&file[..n], 1, "");
    }
    for i in (0..file.len()).step_by(stride) {
        for x in [0xFFu8, 0x20, 0x01] {
            let mut f = file.to_vec();
            f[i] ^= x;
            let _ = crate::pdf_markup::pdf_markups(&f, 1, "");
            let _ = pdf_page_content(&f, 1, "");
        }
    }
}

#[test]
fn hostile_new_fixtures_never_panic() {
    let lzw = |d: &[u8]| lzw_enc(d, true);
    let chain = |d: &[u8]| a85_enc(&rl_enc(&hex_enc(d)));
    let lzw_spec =
        Spec { filter: "/LZWDecode", xparms: "/DecodeParms << /Predictor 12 /Columns 4 >>", sparms: "", enc: &lzw, predict: true, crypt: None };
    let chain_spec =
        Spec { filter: "[/ASCII85Decode /RunLengthDecode /ASCIIHexDecode]", xparms: "", sparms: "", enc: &chain, predict: false, crypt: None };
    let mut files = vec![compressed_with(&OBJS, &lzw_spec), compressed_with(&OBJS, &chain_spec)];
    for fx in [&RC4_128, &AES_128, &AES_256_R5] {
        files.push(classic_enc(&OBJS, fx));
        files.push(compressed_enc(&OBJS, fx, "/StdCF", true));
    }
    files.push(cut(&classic(&OBJS), "xref\n0", "trailer"));
    files.push(cut(&compressed(&OBJS), "9 0 obj\n<< /Type /XRef", "%%EOF"));
    files.push(page_with("/CropBox [10 20 110 70]", ""));
    files.push(free_text("/RC (<body><p>a &amp; b</p></body>) /Contents <FEFF0041>"));
    for f in &files {
        fuzz(f, 1);
    }
    // The R6 hash is slow: fewer positions.
    fuzz(&classic_enc(&OBJS, &AES_256_R6), 7);
    fuzz(&compressed_enc(&OBJS, &AES_256_R6, "/StdCF", true), 7);
}

#[test]
fn hostile_filter_data_never_panics() {
    let mut x = 99u32;
    let mut noise = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 24) as u8
            })
            .collect()
    };
    for n in 0..300 {
        let d = noise(n);
        let _ = filters::lzw(&d, true, 1 << 16);
        let _ = filters::lzw(&d, false, 1 << 16);
        let _ = filters::ascii85(&d);
        let _ = filters::ascii_hex(&d);
        let _ = filters::run_length(&d, 1 << 16);
        let _ = filters::predict(d.clone(), 12, 3, 8, 5, 1 << 16);
        let _ = filters::predict(d.clone(), 2, 3, 8, 5, 1 << 16);
        let _ = cbc(&[1; 16], &[0; 16], &d, true);
        let _ = rc4(&d, &d);
    }
    // A run-length bomb and an LZW bomb stop at the limit.
    assert!(filters::run_length(&[129, 0].repeat(100_000), 1000).is_none());
    let zeros = lzw_enc(&vec![0u8; 100_000], true);
    assert!(filters::lzw(&zeros, true, 5000).is_none());
    assert_eq!(filters::lzw(&zeros, true, 100_000).unwrap().len(), 100_000);
    // A bomb through a whole file is refused.
    let rl = |_: &[u8]| rl_enc(&vec![0u8; 40 << 20]);
    let bomb = Spec { filter: "/RunLengthDecode", xparms: "", sparms: "", enc: &rl, predict: false, crypt: None };
    assert!(pdf_markups(&compressed_with(&OBJS, &bomb), 1).is_err());
}
