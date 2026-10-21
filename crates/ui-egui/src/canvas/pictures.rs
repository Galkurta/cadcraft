//! Raster images and PDF underlays on the canvas: textured quads (egui meshes, so on the GPU
//! canvas they are drawn by wgpu with the texture uploaded once). Pictures come from
//! [`cadcraft_render::images`] (decoded once, checked against the file's modification time);
//! the textures are cached here by decoded picture, fade and background and dropped least
//! recently used first. A picture that can't be read draws nothing (its frame and name are
//! ordinary linework in the display list).
//!
//! The GPU canvas draws all pictures before its linework (underlays under the drawing); the
//! CPU path draws them in draw order.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cadcraft_color::Rgb;
use cadcraft_render::images::{self, Raster};
use cadcraft_render::{DPrim, DisplayList, ImageDraw, Kind};
use egui::{Color32, Mesh, Pos2, Shape, TextureHandle, TextureOptions, pos2};

use super::Xf;

/// Textures kept; the least recently used go first.
const MAX_TEXTURES: usize = 24;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    /// The cached picture (kept alive in the entry, so its address is not reused).
    picture: usize,
    fade: u8,
    bg: (u8, u8, u8),
    nearest: bool,
}

struct Entry {
    texture: TextureHandle,
    _picture: Arc<Raster>,
    used: u64,
}

type Textures = Arc<Mutex<HashMap<Key, Entry>>>;

fn textures(ctx: &egui::Context) -> Textures {
    ctx.data_mut(|d| d.get_temp_mut_or_insert_with::<Textures>(egui::Id::new("cadcraft-picture-textures"), Default::default).clone())
}

/// `r` with each side at most `max` pixels (nearest-pixel decimation).
fn shrink(r: Raster, max: usize) -> Raster {
    let (w, h) = (r.width as usize, r.height as usize);
    if w <= max && h <= max {
        return r;
    }
    let k = w.max(h).div_ceil(max.max(1));
    let (nw, nh) = ((w / k).max(1), (h / k).max(1));
    let mut rgba = Vec::with_capacity(nw * nh * 4);
    for y in 0..nh {
        for x in 0..nw {
            let at = ((y * k) * w + x * k) * 4;
            rgba.extend_from_slice(r.rgba.get(at..at + 4).unwrap_or(&[0, 0, 0, 0]));
        }
    }
    Raster { width: nw as u32, height: nh as u32, rgba }
}

fn texture(ctx: &egui::Context, picture: &Arc<Raster>, fade: u8, bg: Rgb, nearest: bool) -> TextureHandle {
    let store = textures(ctx);
    let key = Key { picture: Arc::as_ptr(picture) as usize, fade, bg: (bg.0, bg.1, bg.2), nearest };
    let now = ctx.cumulative_pass_nr();
    let Ok(mut map) = store.lock() else {
        return upload(ctx, picture, fade, bg, nearest);
    };
    if let Some(e) = map.get_mut(&key) {
        e.used = now;
        return e.texture.clone();
    }
    let texture = upload(ctx, picture, fade, bg, nearest);
    while map.len() >= MAX_TEXTURES {
        let Some(oldest) = map.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone()) else { break };
        map.remove(&oldest);
    }
    map.insert(key, Entry { texture: texture.clone(), _picture: picture.clone(), used: now });
    texture
}

fn upload(ctx: &egui::Context, picture: &Raster, fade: u8, bg: Rgb, nearest: bool) -> TextureHandle {
    let max = ctx.input(|i| i.max_texture_side).max(1024);
    let faded =
        if fade > 0 { images::faded(picture, fade, bg) } else { Raster { width: picture.width, height: picture.height, rgba: picture.rgba.clone() } };
    let r = shrink(faded, max);
    let image = egui::ColorImage::from_rgba_unmultiplied([r.width as usize, r.height as usize], &r.rgba);
    ctx.load_texture("cadcraft-picture", image, if nearest { TextureOptions::NEAREST } else { TextureOptions::LINEAR })
}

/// The picture's textured triangles, or `None` when it can't be drawn.
fn mesh(ctx: &egui::Context, xf: &Xf, img: &ImageDraw, alpha: u8, bg: Rgb) -> Option<Mesh> {
    if img.missing || alpha == 0 {
        return None;
    }
    let corners = [img.origin, img.origin + img.u, img.origin + img.u + img.v, img.origin + img.v];
    let screen: Vec<Pos2> = corners.iter().map(|c| xf.to_screen(*c)).collect();
    let (min, max) = screen.iter().fold((pos2(f32::MAX, f32::MAX), pos2(f32::MIN, f32::MIN)), |(a, b), p| (a.min(*p), b.max(*p)));
    if !(min.x.is_finite() && max.x.is_finite() && min.y.is_finite() && max.y.is_finite()) || !xf.rect.intersects(egui::Rect::from_min_max(min, max))
    {
        return None;
    }
    let want = (img.longest() * xf.scale).clamp(1.0, 1e5) as u32;
    let picture = images::load(&img.path, img.page, want).ok()?;
    let det = img.u.x * img.v.y - img.u.y * img.v.x;
    if !(det.is_finite() && det.abs() > 1e-300) {
        return None;
    }
    // Triangles of the picture (the quad cut by every clip polygon), in world coordinates.
    let mut tris = vec![corners[0], corners[1], corners[2], corners[0], corners[2], corners[3]];
    for poly in &img.clips {
        let clip = cadcraft_render::clip::PolyClip::new(poly)?;
        tris = clip.clip_triangles(&tris);
    }
    if tris.len() < 3 {
        return None;
    }
    // One picture pixel is at least two screen points wide: keep the pixels sharp.
    let nearest = (img.u.len() * xf.scale) > 2.0 * f64::from(picture.width);
    let tex = texture(ctx, &picture, img.fade, bg, nearest);
    let a = alpha;
    let tint = Color32::from_rgba_premultiplied(a, a, a, a);
    let mut m = Mesh::with_texture(tex.id());
    for p in &tris {
        let d = *p - img.origin;
        let s = (d.x * img.v.y - d.y * img.v.x) / det;
        let t = (img.u.x * d.y - img.u.y * d.x) / det;
        m.vertices.push(egui::epaint::Vertex { pos: xf.to_screen(*p), uv: pos2(s as f32, (1.0 - t) as f32), color: tint });
    }
    for i in (0..m.vertices.len() as u32).step_by(3) {
        m.add_triangle(i, i + 1, i + 2);
    }
    Some(m)
}

/// The shape of one picture primitive (CPU path, in draw order).
pub(crate) fn shape(ctx: &egui::Context, xf: &Xf, list: &DisplayList, prim: &DPrim, bg: Rgb) -> Option<Shape> {
    let Kind::Image(ix) = prim.kind else { return None };
    mesh(ctx, xf, list.images.get(ix as usize)?, prim.alpha(), bg).map(Shape::mesh)
}

/// Draw every picture of the list (GPU path: before the linework).
pub(crate) fn draw_all(p: &egui::Painter, xf: &Xf, list: &DisplayList, bg: Rgb) {
    if list.images.is_empty() {
        return;
    }
    let shapes: Vec<Shape> = list.prims.iter().filter_map(|prim| shape(p.ctx(), xf, list, prim, bg)).collect();
    p.extend(shapes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::{Common, Drawing, EntityKind, Image, Space};
    use cadcraft_geom::{Vec2, Vec3};
    use egui::epaint::Primitive;

    fn list(path: &str) -> DisplayList {
        let mut d = Drawing::new_imperial();
        let i = Image { u: Vec3::new(5.0, 0.0, 0.0), v: Vec3::new(0.0, 10.0, 0.0), size: Vec2::new(2.0, 1.0), path: path.into(), ..Image::default() };
        d.add(&Space::Model, Common::default(), EntityKind::Image(i)).unwrap();
        cadcraft_render::build(&d, &Space::Model, &cadcraft_render::Options::default())
    }

    /// The textured meshes (and their egui texture ids) a frame draws.
    fn frame(list: &DisplayList, scale: f64) -> Vec<egui::TextureId> {
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        let input = egui::RawInput { screen_rect: Some(rect), ..Default::default() };
        let xf = Xf { rect, center: Vec2::new(5.0, 5.0), scale };
        let mut out = ctx.run_ui(input, |ui| super::super::draw_list(ui.painter(), &xf, list, Rgb(0, 0, 0), false));
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, 1.0);
        prims
            .into_iter()
            .filter_map(|p| match p.primitive {
                Primitive::Mesh(m) if m.texture_id != egui::TextureId::default() && !m.vertices.is_empty() => Some(m.texture_id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_picture_is_a_textured_mesh_and_a_missing_file_is_not() {
        let r = Raster { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 0, 0, 255, 255] };
        let uri = images::png_data_uri(&r).unwrap();
        let bytes: Vec<u8> = {
            let b64 = uri.strip_prefix("data:image/png;base64,").unwrap();
            cadcraft_engine::cmd::file::base64_decode(b64).unwrap()
        };
        images::put_bytes("test:canvas.png", bytes).unwrap();
        assert_eq!(frame(&list("test:canvas.png"), 10.0).len(), 1, "one textured mesh");
        assert!(frame(&list("D:/gone/missing.png"), 10.0).is_empty(), "a missing file draws only its frame");
    }

    #[test]
    fn a_broken_picture_draws_nothing_and_does_not_panic() {
        images::put_bytes("test:broken.png", b"\x89PNG\r\n\x1a\ngarbage".to_vec()).unwrap();
        assert!(frame(&list("test:broken.png"), 10.0).is_empty());
    }
}
