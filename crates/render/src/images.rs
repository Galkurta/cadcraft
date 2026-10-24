//! Pictures for IMAGE entities: decoding with size caps and a cache.
//!
//! A picture is named by a path. Bytes the host handed over (the web build has no files, and
//! tests have none) are registered under a name with [`put_bytes`] and found first; otherwise
//! the path is read from disk (native builds). Decoded pictures are cached by path and checked against the file's modification time on every lookup, so an edited
//! file shows up without reopening the drawing. Every size is capped: a hostile file is an
//! `Err`, never a panic or an unbounded allocation.
//!
//! PNG and JPEG decode here.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, LazyLock, Mutex};

/// Largest picture file read.
pub const MAX_FILE_BYTES: u64 = 256 << 20;
/// Most pixels in a decoded picture.
pub const MAX_PIXELS: u64 = 50_000_000;
/// Most decoded bytes the cache keeps; the least recently used pictures go first.
const MAX_CACHE_BYTES: usize = 768 << 20;
/// Most bytes (and pictures) held for [`put_bytes`].
const MAX_EMBEDDED_BYTES: usize = 512 << 20;
const MAX_EMBEDDED: usize = 256;

/// A decoded picture: straight-alpha RGBA, top row first.
#[derive(Debug)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Raster {
    /// A raster of `width` × `height` pixels filled with `rgba`; `None` when it is empty or over [`MAX_PIXELS`].
    pub fn filled(width: u32, height: u32, rgba: [u8; 4]) -> Option<Raster> {
        let n = checked_len(width, height)?;
        Some(Raster { width, height, rgba: rgba.iter().copied().cycle().take(n).collect() })
    }
}

fn checked_len(w: u32, h: u32) -> Option<usize> {
    let px = u64::from(w) * u64::from(h);
    (px > 0 && px <= MAX_PIXELS).then_some(())?;
    usize::try_from(px.checked_mul(4)?).ok()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    modified: u128,
}

type Embedded = HashMap<String, Arc<Vec<u8>>>;

static EMBEDDED: LazyLock<Mutex<Embedded>> = LazyLock::new(Default::default);

fn embedded() -> &'static Mutex<Embedded> {
    &EMBEDDED
}

/// Register picture bytes under `name`: an IMAGE whose path is `name` then shows them without a
/// file (web builds, generated pictures). Replaces an earlier registration of the name.
pub fn put_bytes(name: &str, bytes: Vec<u8>) -> Result<(), String> {
    let mut e = embedded().lock().map_err(|_| "the picture store is unavailable".to_string())?;
    let held: usize = e.iter().filter(|(k, _)| k.as_str() != name).map(|(_, v)| v.len()).sum();
    let others = e.keys().filter(|k| k.as_str() != name).count();
    if held.saturating_add(bytes.len()) > MAX_EMBEDDED_BYTES || others >= MAX_EMBEDDED {
        return Err("too many picture bytes held in memory".into());
    }
    e.insert(name.to_string(), Arc::new(bytes));
    Ok(())
}

/// Paths that reach other computers (network shares, URLs) are never opened for a picture a
/// drawing names: a stranger's file must not make the viewer contact a stranger's server.
#[cfg(not(target_arch = "wasm32"))]
fn refused(path: &str) -> bool {
    let p = path.trim_start();
    p.starts_with("\\\\") || p.starts_with("//") || p.contains("://") || p.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("file:"))
}

fn file_stamp(_path: &str) -> Option<Stamp> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if refused(_path) {
            return None;
        }
        let m = std::fs::metadata(_path).ok()?;
        if !m.is_file() {
            return None;
        }
        let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        Some(Stamp { len: m.len(), modified })
    }
    #[cfg(target_arch = "wasm32")]
    None
}

/// Whether `path` names registered bytes or a readable file.
pub fn available(path: &str) -> bool {
    embedded().lock().is_ok_and(|e| e.contains_key(path)) || file_stamp(path).is_some()
}

/// The bytes behind `path` (registered bytes first, else the file), within the size cap.
pub fn read_bytes(path: &str) -> Result<Arc<Vec<u8>>, String> {
    source(path).map(|(b, _)| b)
}

/// A stamp that changes when the bytes behind `path` do, without reading a file.
fn stamp_of(path: &str) -> Result<Stamp, String> {
    if let Some(b) = embedded().lock().map_err(|_| "the picture store is unavailable".to_string())?.get(path) {
        return Ok(embedded_stamp(b));
    }
    file_checked(path)
}

fn embedded_stamp(b: &Arc<Vec<u8>>) -> Stamp {
    Stamp { len: b.len() as u64, modified: Arc::as_ptr(b) as usize as u128 }
}

/// The bytes behind `path` and a stamp that changes when they do.
fn source(path: &str) -> Result<(Arc<Vec<u8>>, Stamp), String> {
    if let Some(b) = embedded().lock().map_err(|_| "the picture store is unavailable".to_string())?.get(path) {
        return Ok((b.clone(), embedded_stamp(b)));
    }
    read_file(path)
}

/// The stamp of the file at `_path`, once it passes the network and size checks.
fn file_checked(_path: &str) -> Result<Stamp, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if refused(_path) {
            return Err(format!("{_path}: network paths are not opened"));
        }
        let stamp = file_stamp(_path).ok_or_else(|| format!("{_path}: file not found"))?;
        if stamp.len > MAX_FILE_BYTES {
            return Err(format!("{_path}: file too large"));
        }
        Ok(stamp)
    }
    #[cfg(target_arch = "wasm32")]
    Err(format!("{_path}: files are not available on the web; attach the picture's bytes"))
}

fn read_file(path: &str) -> Result<(Arc<Vec<u8>>, Stamp), String> {
    let stamp = file_checked(path)?;
    #[cfg(test)]
    tests::note_read(path);
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    Ok((Arc::new(b), stamp))
}

/// The size in pixels of a PNG or JPEG, from its header, within [`MAX_PIXELS`].
pub fn raster_dimensions(bytes: &[u8]) -> Result<(u32, u32), String> {
    let r = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let (w, h) = r.into_dimensions().map_err(|e| e.to_string())?;
    checked_len(w, h).ok_or_else(|| format!("the picture is {w}×{h} pixels (at most {MAX_PIXELS} pixels are read)"))?;
    Ok((w, h))
}

/// Decode PNG or JPEG bytes (size-checked before any pixel is allocated).
pub fn decode_raster(bytes: &[u8]) -> Result<Raster, String> {
    let (w, h) = raster_dimensions(bytes)?;
    let mut r = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_PIXELS * 8);
    r.limits(limits);
    let img = r.decode().map_err(|e| e.to_string())?.into_rgba8();
    if (img.width(), img.height()) != (w, h) {
        return Err("the picture's size is inconsistent".into());
    }
    Ok(Raster { width: w, height: h, rgba: img.into_raw() })
}

struct Entry {
    stamp: Stamp,
    raster: Arc<Raster>,
    used: u64,
}

#[derive(Default)]
struct Cache {
    pictures: HashMap<String, Entry>,
    /// Pictures that failed to decode, by the stamp they failed at (a broken file is not re-read every frame).
    failed: HashMap<String, (Stamp, String)>,
    sizes: HashMap<String, (Stamp, (f64, f64))>,
    clock: u64,
    bytes: usize,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

fn cache() -> &'static Mutex<Cache> {
    &CACHE
}

/// The size of the picture in pixels.
pub fn natural_size(path: &str) -> Result<(f64, f64), String> {
    let stamp = stamp_of(path)?;
    {
        let c = cache().lock().map_err(|_| "the picture cache is unavailable".to_string())?;
        if let Some((s, size)) = c.sizes.get(path)
            && *s == stamp
        {
            return Ok(*size);
        }
    }
    let (bytes, stamp) = source(path)?;
    let (w, h) = raster_dimensions(&bytes)?;
    let size = (f64::from(w), f64::from(h));
    let mut c = cache().lock().map_err(|_| "the picture cache is unavailable".to_string())?;
    if c.sizes.len() > 4096 {
        c.sizes.clear();
    }
    c.sizes.insert(path.to_string(), (stamp, size));
    Ok(size)
}

/// The decoded picture for `path`.
pub fn load(path: &str) -> Result<Arc<Raster>, String> {
    let stamp = stamp_of(path)?;
    let key = path.to_string();
    {
        let mut c = cache().lock().map_err(|_| "the picture cache is unavailable".to_string())?;
        c.clock += 1;
        let now = c.clock;
        if let Some(e) = c.pictures.get_mut(&key)
            && e.stamp == stamp
        {
            e.used = now;
            return Ok(e.raster.clone());
        }
        if let Some((s, why)) = c.failed.get(&key)
            && *s == stamp
        {
            return Err(why.clone());
        }
    }
    let (bytes, stamp) = source(path)?;
    // Decoded outside the lock: a big picture doesn't stall the others.
    let decoded = decode_raster(&bytes).and_then(|r| {
        checked_len(r.width, r.height).filter(|n| *n == r.rgba.len()).map(|_| r).ok_or_else(|| "the decoded picture is inconsistent".to_string())
    });
    let raster = match decoded {
        Ok(r) => r,
        Err(why) => {
            if let Ok(mut c) = cache().lock() {
                if c.failed.len() > 4096 {
                    c.failed.clear();
                }
                c.failed.insert(key, (stamp, why.clone()));
            }
            return Err(why);
        }
    };
    let raster = Arc::new(raster);
    let mut c = cache().lock().map_err(|_| "the picture cache is unavailable".to_string())?;
    c.clock += 1;
    let used = c.clock;
    if let Some(old) = c.pictures.insert(key, Entry { stamp, raster: raster.clone(), used }) {
        c.bytes = c.bytes.saturating_sub(old.raster.rgba.len());
    }
    c.bytes = c.bytes.saturating_add(raster.rgba.len());
    while c.bytes > MAX_CACHE_BYTES && c.pictures.len() > 1 {
        let Some(oldest) = c.pictures.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone()) else { break };
        if let Some(e) = c.pictures.remove(&oldest) {
            c.bytes = c.bytes.saturating_sub(e.raster.rgba.len());
        }
    }
    Ok(raster)
}

/// `r` faded towards `bg` by `fade` percent (0 = unchanged, 100 = all background).
pub fn faded(r: &Raster, fade: u8, bg: cadcraft_color::Rgb) -> Raster {
    let f = u32::from(fade.min(100));
    let mix = |c: u8, b: u8| ((u32::from(c) * (100 - f) + u32::from(b) * f + 50) / 100) as u8;
    let mut rgba = r.rgba.clone();
    for [red, green, blue, _] in rgba.as_chunks_mut::<4>().0 {
        *red = mix(*red, bg.0);
        *green = mix(*green, bg.1);
        *blue = mix(*blue, bg.2);
    }
    Raster { width: r.width, height: r.height, rgba }
}

/// `r` as a `data:image/png;base64,…` URI (for SVG); `None` when it can't be encoded.
pub fn png_data_uri(r: &Raster) -> Option<String> {
    let img = image::RgbaImage::from_raw(r.width, r.height, r.rgba.clone())?;
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(22 + png.len().div_ceil(3) * 4);
    out.push_str("data:image/png;base64,");
    for c in png.chunks(3) {
        let n = c.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= c.len() {
                out.push(char::from(A.get((n >> (18 - 6 * i) & 63) as usize).copied().unwrap_or(b'=')));
            } else {
                out.push('=');
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    static READS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    pub(super) fn note_read(path: &str) {
        if let Ok(mut r) = READS.lock() {
            r.push(path.to_string());
        }
    }

    fn reads(path: &str) -> usize {
        READS.lock().map_or(0, |r| r.iter().filter(|p| p.as_str() == path).count())
    }

    #[test]
    fn an_unchanged_file_is_not_read_again() {
        let mut png = Vec::new();
        let img = image::RgbaImage::new(2, 2);
        assert!(img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).is_ok());
        let file = std::env::temp_dir().join(format!("cadcraft-images-{}.png", std::process::id()));
        assert!(std::fs::write(&file, &png).is_ok());
        let path = file.to_string_lossy().into_owned();
        assert!(load(&path).is_ok());
        assert_eq!(reads(&path), 1);
        assert!(load(&path).is_ok());
        assert_eq!(natural_size(&path), Ok((2.0, 2.0)));
        assert_eq!(natural_size(&path), Ok((2.0, 2.0)));
        // natural_size reads once for its own cache; repeats don't read.
        assert_eq!(reads(&path), 2);
        let _ = std::fs::remove_file(&file);
    }
}
