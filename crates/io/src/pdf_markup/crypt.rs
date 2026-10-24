//! The standard security handler (user password): RC4 40-128 bit (V1/V2, revisions 2-4), AES-128
//! (V4 `AESV2`) and AES-256 (V5, revisions 5 and 6), written from the public PDF file structure.
//! Public-key handlers are refused. The owner password is not tried (ponytail: add when a real
//! file needs it; the user password opens the markups).

use std::collections::BTreeMap;

use aes::cipher::{BlockDecrypt, BlockEncrypt, BlockSizeUser, KeyInit, consts::U16, generic_array::GenericArray};
use aes::{Aes128, Aes256};
use md5::Md5;
use sha2::{Digest, Sha256, Sha384, Sha512};

use super::{Dict, Obj, Pdf};
use crate::{IoError, Result};

/// The padding string of the standard handler's key derivation.
const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08, 0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80,
    0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Method {
    Identity,
    Rc4,
    Aes128,
    Aes256,
}

pub(super) struct Crypt {
    key: Vec<u8>,
    stm: Method,
    strf: Method,
    /// The named crypt filters (`/CF`), for streams with their own `/Crypt` filter.
    cf: BTreeMap<String, Method>,
}

/// RC4 (an empty key leaves the data alone).
pub(super) fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return data.to_vec();
    }
    // Indexed by `u8` values only: no index can leave the 256 entries.
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for (i, k) in (0..=255u8).zip(key.iter().cycle()) {
        j = j.wrapping_add(s[usize::from(i)]).wrapping_add(*k);
        s.swap(usize::from(i), usize::from(j));
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|b| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[usize::from(i)]);
            s.swap(usize::from(i), usize::from(j));
            b ^ s[usize::from(s[usize::from(i)].wrapping_add(s[usize::from(j)]))]
        })
        .collect()
}

/// AES-CBC without padding over whole blocks (`None` for a bad key size or a short IV).
pub(super) fn cbc(key: &[u8], iv: &[u8], data: &[u8], decrypt: bool) -> Option<Vec<u8>> {
    match key.len() {
        16 => cbc_with(&Aes128::new_from_slice(key).ok()?, iv, data, decrypt),
        32 => cbc_with(&Aes256::new_from_slice(key).ok()?, iv, data, decrypt),
        _ => None,
    }
}

fn cbc_with<C: BlockEncrypt + BlockDecrypt + BlockSizeUser<BlockSize = U16>>(c: &C, iv: &[u8], data: &[u8], decrypt: bool) -> Option<Vec<u8>> {
    let mut prev = GenericArray::<u8, U16>::default();
    prev.copy_from_slice(iv.get(..16)?);
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.as_chunks::<16>().0 {
        let mut b = GenericArray::<u8, U16>::from(*chunk);
        if decrypt {
            let ct = b;
            c.decrypt_block(&mut b);
            b.iter_mut().zip(prev.iter()).for_each(|(x, y)| *x ^= y);
            prev = ct;
        } else {
            b.iter_mut().zip(prev.iter()).for_each(|(x, y)| *x ^= y);
            c.encrypt_block(&mut b);
            prev = b;
        }
        out.extend_from_slice(&b);
    }
    Some(out)
}

/// A string or stream encrypted with AES-CBC: 16 bytes of IV, then PKCS#5-padded blocks.
fn aes_open(key: &[u8], data: &[u8]) -> Vec<u8> {
    let (Some(iv), Some(body)) = (data.get(..16), data.get(16..)) else { return Vec::new() };
    let body = body.get(..body.len() / 16 * 16).unwrap_or_default();
    let mut out = cbc(key, iv, body, true).unwrap_or_default();
    if let Some(&n) = out.last()
        && (1..=16).contains(&n)
        && usize::from(n) <= out.len()
    {
        out.truncate(out.len() - usize::from(n));
    }
    out
}

/// The password hash of revisions 5 (SHA-256) and 6 (the iterated hash).
pub(super) fn hash_r56(r: i64, pw: &[u8], salt: &[u8], udata: &[u8]) -> Option<Vec<u8>> {
    let mut seed = Sha256::new();
    seed.update(pw);
    seed.update(salt);
    seed.update(udata);
    let mut k = seed.finalize().to_vec();
    if r < 6 {
        return Some(k);
    }
    let mut i = 0u32;
    loop {
        let unit: Vec<u8> = [pw, &k, udata].concat();
        let k1 = unit.repeat(64);
        let e = cbc(k.get(..16)?, k.get(16..32)?, &k1, false)?;
        let m = e.get(..16)?.iter().map(|b| u32::from(*b)).sum::<u32>() % 3;
        k = match m {
            0 => Sha256::digest(&e).to_vec(),
            1 => Sha384::digest(&e).to_vec(),
            _ => Sha512::digest(&e).to_vec(),
        };
        i += 1;
        if i >= 64 && e.last().is_some_and(|l| u32::from(*l) + 32 <= i) {
            break;
        }
    }
    k.truncate(32);
    Some(k)
}

impl Crypt {
    fn obj_key(&self, m: Method, id: (u32, u16)) -> Vec<u8> {
        if m == Method::Aes256 {
            return self.key.clone();
        }
        let mut h = Md5::new();
        h.update(&self.key);
        h.update([id.0 as u8, (id.0 >> 8) as u8, (id.0 >> 16) as u8, id.1 as u8, (id.1 >> 8) as u8]);
        if m == Method::Aes128 {
            h.update(b"sAlT");
        }
        let mut k = h.finalize().to_vec();
        k.truncate((self.key.len() + 5).min(16));
        k
    }

    pub(super) fn open(&self, m: Method, id: (u32, u16), data: &[u8]) -> Vec<u8> {
        match m {
            Method::Identity => data.to_vec(),
            Method::Rc4 => rc4(&self.obj_key(m, id), data),
            Method::Aes128 | Method::Aes256 => aes_open(&self.obj_key(m, id), data),
        }
    }

    /// Decrypts every string of an object read from the file (not of an object-stream member).
    pub(super) fn strings(&self, o: &mut Obj, id: (u32, u16)) {
        match o {
            Obj::Str(s) => *s = self.open(self.strf, id, s),
            Obj::Arr(v) => v.iter_mut().for_each(|e| self.strings(e, id)),
            Obj::Dict(d) | Obj::Stream(d, ..) => d.values_mut().for_each(|e| self.strings(e, id)),
            _ => {}
        }
    }

    /// How a stream with these (resolved) filters and parameters is encrypted.
    pub(super) fn stream_method(&self, filters: &[Obj], parms: &[Obj]) -> Method {
        if matches!(filters.first(), Some(Obj::Name(n)) if n == "Crypt") {
            let name = match parms.first() {
                Some(Obj::Dict(p)) => match p.get("Name") {
                    Some(Obj::Name(n)) => n.as_str(),
                    _ => "Identity",
                },
                _ => "Identity",
            };
            return self.cf.get(name).copied().unwrap_or(Method::Identity);
        }
        self.stm
    }
}

fn err(m: &str) -> IoError {
    IoError::Format(m.into())
}

impl Pdf<'_> {
    /// Sets up decryption from `/Encrypt` (nothing to do for a plain file); `false` when the
    /// encryption dictionary cannot be read. Run before any other object is read; the
    /// encryption dictionary itself is never decrypted.
    pub(super) fn init_crypt(&mut self, password: &str) -> Result<bool> {
        let Some(e) = self.trailer.get("Encrypt").cloned() else { return Ok(true) };
        // An unreadable dictionary is `Ok(false)`: the xref may be what is wrong.
        let Obj::Dict(e) = self.resolve(&e) else { return Ok(false) };
        if !matches!(self.val(&e, "Filter"), Obj::Name(n) if n == "Standard") {
            return Err(err("PDF files with a public-key security handler are not supported"));
        }
        let int = |me: &mut Self, k: &str, def: i64| me.num_of(&e, k).filter(|n| n.abs() < 1e12).map_or(def, |n| n as i64);
        let (v, r) = (int(self, "V", 0), int(self, "R", 0));
        if !matches!(v, 1 | 2 | 4 | 5) || !(2..=6).contains(&r) {
            return Err(err("the PDF file uses an unsupported encryption version"));
        }
        let (o, u) = (self.string(&e, "O"), self.string(&e, "U"));
        let id0 = match self.trailer.get("ID").cloned().map(|i| self.resolve(&i)) {
            Some(Obj::Arr(a)) => match a.first().map(|f| self.resolve(f)) {
                Some(Obj::Str(s)) => s,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let (mut stm, mut strf, mut cf) = (Method::Rc4, Method::Rc4, BTreeMap::new());
        if v >= 4 {
            let table = match self.val(&e, "CF") {
                Obj::Dict(d) => d,
                _ => Dict::new(),
            };
            for (name, f) in &table {
                let Obj::Dict(f) = self.resolve(f) else { continue };
                let m = match self.val(&f, "CFM") {
                    Obj::Name(n) if n == "V2" => Method::Rc4,
                    Obj::Name(n) if n == "AESV2" => Method::Aes128,
                    Obj::Name(n) if n == "AESV3" => Method::Aes256,
                    Obj::Name(n) if n == "None" => Method::Identity,
                    _ => return Err(err("the PDF file uses an unsupported crypt filter")),
                };
                cf.insert(name.clone(), m);
            }
            let named = |me: &mut Self, k: &str| match me.val(&e, k) {
                Obj::Name(n) => cf.get(&n).copied().unwrap_or(Method::Identity),
                _ => Method::Identity,
            };
            (stm, strf) = (named(self, "StmF"), named(self, "StrF"));
        }
        let key = if v == 5 {
            if r < 5 {
                return Err(err("the PDF file uses an unsupported encryption revision"));
            }
            let pw = password.as_bytes().get(..password.len().min(127)).unwrap_or_default();
            let ue = self.string(&e, "UE");
            key_r56(r, pw, &u, &ue)?
        } else {
            let bits = int(self, "Length", 40);
            let len = if v == 1 {
                5
            } else if v == 4 {
                if (40..=128).contains(&bits) { (bits / 8) as usize } else { 16 }
            } else {
                (bits / 8).clamp(5, 16) as usize
            };
            let enc_meta = !matches!(e.get("EncryptMetadata"), Some(Obj::Bool(false)));
            let p = int(self, "P", 0) as u32;
            let mut padded: Vec<u8> = password.chars().filter_map(|c| u8::try_from(u32::from(c)).ok()).take(32).collect();
            padded.extend(PAD.iter().take(32 - padded.len()));
            key_r234(&padded, &o, &u, p, &id0, r, len, enc_meta)?
        };
        self.crypt = Some(Crypt { key, stm, strf, cf });
        Ok(true)
    }
}

fn key_r56(r: i64, pw: &[u8], u: &[u8], ue: &[u8]) -> Result<Vec<u8>> {
    let (Some(hash), Some(vsalt), Some(ksalt), Some(ue)) = (u.get(..32), u.get(32..40), u.get(40..48), ue.get(..32)) else {
        return Err(err("the PDF file's encryption dictionary is damaged"));
    };
    if hash_r56(r, pw, vsalt, &[]).as_deref() != Some(hash) {
        return Err(wrong());
    }
    let ik = hash_r56(r, pw, ksalt, &[]).ok_or_else(|| err("the PDF file's encryption dictionary is damaged"))?;
    cbc(&ik, &[0; 16], ue, true).ok_or_else(|| err("the PDF file's encryption dictionary is damaged"))
}

fn wrong() -> IoError {
    IoError::Format("the PDF file is password protected and the password is missing or wrong".into())
}

/// The file key of revisions 2-4 for a user password (already padded), checked against `/U`.
#[allow(clippy::too_many_arguments)]
fn key_r234(padded: &[u8], o: &[u8], u: &[u8], p: u32, id0: &[u8], r: i64, len: usize, enc_meta: bool) -> Result<Vec<u8>> {
    let o = o.get(..32).ok_or_else(|| err("the PDF file's encryption dictionary is damaged"))?;
    let mut h = Md5::new();
    h.update(padded);
    h.update(o);
    h.update(p.to_le_bytes());
    h.update(id0);
    if r >= 4 && !enc_meta {
        h.update([0xFF; 4]);
    }
    let mut d = h.finalize().to_vec();
    if r >= 3 {
        for _ in 0..50 {
            d = Md5::digest(d.get(..len).unwrap_or_default()).to_vec();
        }
    }
    d.truncate(len);
    let ok = if r == 2 {
        u.get(..32) == Some(rc4(&d, &PAD).as_slice())
    } else {
        let mut x = rc4(&d, &Md5::new().chain_update(PAD).chain_update(id0).finalize());
        for i in 1..=19u8 {
            let k: Vec<u8> = d.iter().map(|b| b ^ i).collect();
            x = rc4(&k, &x);
        }
        u.get(..16).is_some_and(|u| u == x.as_slice())
    };
    if ok { Ok(d) } else { Err(wrong()) }
}
