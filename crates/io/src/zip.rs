//! A minimal ZIP reader for package files (DWFx / OPC and DWF 6): the central directory, stored
//! and deflated entries. The archive is hostile input: entry counts, sizes and offsets are
//! checked, every inflate is capped, and ZIP64 and encrypted entries are refused with an error.

use miniz_oxide::inflate::decompress_to_vec_with_limit;

const MAX_ENTRIES: usize = 100_000;
/// Most bytes inflated from one entry unless the caller asks for less.
pub(crate) const MAX_ENTRY_BYTES: usize = 256 << 20;

struct Entry {
    name: String,
    method: u16,
    csize: usize,
    usize_: usize,
    local: usize,
}

pub(crate) struct Zip<'a> {
    data: &'a [u8],
    entries: Vec<Entry>,
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// Whether the 4-byte signature `sig` is at `at`.
fn sig_at(d: &[u8], at: usize, sig: &[u8; 4]) -> bool {
    at.checked_add(4).and_then(|e| d.get(at..e)) == Some(sig.as_slice())
}

/// Whether `bytes` start like a ZIP archive (a local file header, or an empty archive).
pub(crate) fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06")
}

/// The part name as OPC compares it: no leading slash, ASCII case folded.
fn fold(name: &str) -> String {
    name.trim_start_matches('/').replace('\\', "/").to_ascii_lowercase()
}

impl<'a> Zip<'a> {
    pub(crate) fn open(data: &'a [u8]) -> Result<Self, String> {
        let bad = || "the package is not a valid ZIP archive".to_string();
        // The end-of-central-directory record is within the last 64 KiB + 22 bytes.
        let from = data.len().saturating_sub(22 + 65535);
        let eocd = (from..=data.len().saturating_sub(22)).rev().find(|&i| sig_at(data, i, b"PK\x05\x06")).ok_or_else(bad)?;
        let count = usize::from(u16_at(data, eocd + 10).ok_or_else(bad)?);
        let cd_offset = u32_at(data, eocd + 16).ok_or_else(bad)?;
        if count == 0xFFFF || cd_offset == 0xFFFF_FFFF {
            return Err("ZIP64 packages are not supported".into());
        }
        let mut at = cd_offset as usize;
        let mut entries = Vec::new();
        for _ in 0..count.min(MAX_ENTRIES) {
            if !sig_at(data, at, b"PK\x01\x02") {
                return Err(bad());
            }
            let flags = u16_at(data, at + 8).ok_or_else(bad)?;
            let method = u16_at(data, at + 10).ok_or_else(bad)?;
            let csize = u32_at(data, at + 20).ok_or_else(bad)?;
            let usize_ = u32_at(data, at + 24).ok_or_else(bad)?;
            let (n, m, k) = (
                usize::from(u16_at(data, at + 28).ok_or_else(bad)?),
                usize::from(u16_at(data, at + 30).ok_or_else(bad)?),
                usize::from(u16_at(data, at + 32).ok_or_else(bad)?),
            );
            let local = u32_at(data, at + 42).ok_or_else(bad)?;
            let name = at.checked_add(46).and_then(|a| data.get(a..a.checked_add(n)?)).ok_or_else(bad)?;
            if flags & 1 != 0 {
                return Err("encrypted packages are not supported".into());
            }
            if csize == 0xFFFF_FFFF || usize_ == 0xFFFF_FFFF {
                return Err("ZIP64 packages are not supported".into());
            }
            entries.push(Entry {
                name: String::from_utf8_lossy(name).into_owned(),
                method,
                csize: csize as usize,
                usize_: usize_ as usize,
                local: local as usize,
            });
            at = at.checked_add(46 + n + m + k).ok_or_else(bad)?;
        }
        Ok(Zip { data, entries })
    }

    /// Every entry name, as stored.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    fn find(&self, name: &str) -> Option<&Entry> {
        let want = fold(name);
        self.entries.iter().find(|e| fold(&e.name) == want)
    }

    /// The bytes of entry `name` (matched the OPC way), inflating at most `limit` bytes.
    pub(crate) fn read(&self, name: &str, limit: usize) -> Result<Vec<u8>, String> {
        let e = self.find(name).ok_or_else(|| format!("the package has no part `{name}`"))?;
        let bad = || format!("part `{name}` is damaged");
        if e.usize_ > limit {
            return Err(format!("part `{name}` is too large"));
        }
        if !sig_at(self.data, e.local, b"PK\x03\x04") {
            return Err(bad());
        }
        let n = usize::from(u16_at(self.data, e.local + 26).ok_or_else(bad)?);
        let m = usize::from(u16_at(self.data, e.local + 28).ok_or_else(bad)?);
        let start = e.local.checked_add(30 + n + m).ok_or_else(bad)?;
        let raw = self.data.get(start..start.checked_add(e.csize).ok_or_else(bad)?).ok_or_else(bad)?;
        match e.method {
            0 => Ok(raw.to_vec()),
            8 => decompress_to_vec_with_limit(raw, limit).map_err(|_| bad()),
            m => Err(format!("part `{name}` uses unsupported compression method {m}")),
        }
    }
}

/// A ZIP archive of `entries` (name, bytes), deflated when `deflate`: fixtures for tests.
#[cfg(test)]
pub(crate) fn build(entries: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let (method, body) = if deflate { (8u16, miniz_oxide::deflate::compress_to_vec(data, 6)) } else { (0u16, data.to_vec()) };
        let off = out.len() as u32;
        out.extend(b"PK\x03\x04");
        out.extend([20, 0, 0, 0]);
        out.extend(method.to_le_bytes());
        out.extend([0u8; 8]);
        out.extend((body.len() as u32).to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend([0, 0]);
        out.extend(name.as_bytes());
        out.extend(&body);
        central.extend(b"PK\x01\x02");
        central.extend([20, 0, 20, 0, 0, 0]);
        central.extend(method.to_le_bytes());
        central.extend([0u8; 8]);
        central.extend((body.len() as u32).to_le_bytes());
        central.extend((data.len() as u32).to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend([0u8; 12]);
        central.extend(off.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let cd_off = out.len() as u32;
    out.extend(&central);
    out.extend(b"PK\x05\x06\0\0\0\0");
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(cd_off.to_le_bytes());
    out.extend([0, 0]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_stored_and_deflated_entries_case_insensitively() {
        for deflate in [false, true] {
            let z = build(&[("A/b.xml", b"<a/>"), ("c.txt", b"hello hello hello hello")], deflate);
            let zip = Zip::open(&z).unwrap();
            assert_eq!(zip.read("/a/B.XML", 100).unwrap(), b"<a/>");
            assert_eq!(zip.read("c.txt", 100).unwrap(), b"hello hello hello hello");
            assert!(zip.read("c.txt", 5).is_err());
            assert!(zip.read("none", 5).is_err());
        }
    }

    #[test]
    fn hostile_archives_do_not_panic() {
        let z = build(&[("a.xml", b"<a/><a/><a/>"), ("b.txt", b"0123456789")], true);
        for n in 0..z.len() {
            if let Ok(zip) = Zip::open(&z[..n]) {
                let names: Vec<String> = zip.names().map(str::to_string).collect();
                for name in names {
                    let _ = zip.read(&name, 1000);
                }
            }
        }
        for i in 0..z.len() {
            let mut b = z.clone();
            b[i] ^= 0xFF;
            if let Ok(zip) = Zip::open(&b) {
                let names: Vec<String> = zip.names().map(str::to_string).collect();
                for name in names {
                    let _ = zip.read(&name, 1000);
                }
            }
        }
    }
}
