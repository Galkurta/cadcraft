use crate::zip;

fn stream(color: &str) -> Vec<u8> {
    let mut b = b"(W2D V06.00)".to_vec();
    b.extend(format!("(Bounds 0,0 1000,1000)(Color {color},255) F P 4 100,100 900,100 900,900 100,900").as_bytes());
    b
}

const MANIFEST: &str = "<?xml version=\"1.0\"?><Manifest><Content>\
    <Section title=\"Ground floor\"><Resource href=\"sections/a.w2d\"/></Section>\
    <Section title=\"Roof\"><Resource href=\"SECTIONS\\B.W2D\"/></Section></Content></Manifest>";

fn package(deflate: bool) -> Vec<u8> {
    zip::build(
        &[
            ("sections/b.w2d", &stream("255,0,0")),
            ("Manifest.xml", MANIFEST.as_bytes()),
            ("sections/a.w2d", &stream("0,200,0")),
            ("readme.txt", b"not a sheet"),
        ],
        deflate,
    )
}

#[test]
fn manifest_orders_and_titles_the_sheets() {
    for deflate in [false, true] {
        let p = package(deflate);
        assert_eq!(crate::underlay::sheets(&p).unwrap(), vec!["Ground floor".to_string(), "Roof".to_string()]);
        let r1 = crate::underlay::render(&p, 1, 512).unwrap();
        let r2 = crate::underlay::render(&p, 2, 512).unwrap();
        let centre = |r: &cadcraft_render::images::Raster| {
            let i = (((r.height / 2) * r.width + r.width / 2) * 4) as usize;
            r.rgba.get(i..i + 3).map(<[u8]>::to_vec).unwrap_or_default()
        };
        assert_eq!(centre(&r1), vec![0, 200, 0]);
        assert_eq!(centre(&r2), vec![255, 0, 0]);
        assert!(crate::underlay::render(&p, 3, 512).is_err());
        assert!(crate::underlay::render(&p, 0, 512).is_err());
        assert_eq!(crate::underlay::page_size(&p, 2).unwrap(), (792.0, 792.0));
    }
}

#[test]
fn without_a_manifest_sheets_are_in_archive_order_named_by_file() {
    let p = zip::build(&[("x/Plan One.w2d", &stream("0,0,255")), ("two.W2D", &stream("0,0,255")), ("junk.bin", b"zz")], true);
    assert_eq!(crate::underlay::sheets(&p).unwrap(), vec!["Plan One".to_string(), "two".to_string()]);
}

#[test]
fn sections_missing_from_the_manifest_follow_it() {
    let m = "<Manifest><Section title=\"Only\" href=\"b.w2d\"/></Manifest>";
    let p = zip::build(&[("a.w2d", &stream("0,0,255")), ("b.w2d", &stream("0,0,255")), ("manifest.xml", m.as_bytes())], false);
    assert_eq!(crate::underlay::sheets(&p).unwrap(), vec!["Only".to_string(), "a".to_string()]);
}

#[test]
fn a_package_without_sheets_is_an_error() {
    let p = zip::build(&[("manifest.xml", b"<a/>"), ("notes.w2d.txt", b"x")], false);
    assert_eq!(crate::underlay::sheets(&p).err().as_deref(), Some("the package holds no 2D sheets"));
}

#[test]
fn hostile_packages_never_panic() {
    let p = package(true);
    for n in 0..p.len() {
        let cut = p.get(..n).unwrap_or(&[]);
        let _ = crate::underlay::sheets(cut);
        let _ = crate::underlay::render(cut, 2, 256);
    }
    for i in 0..p.len() {
        let mut g = p.clone();
        if let Some(v) = g.get_mut(i) {
            *v ^= 0xFF;
        }
        let _ = crate::underlay::sheets(&g);
        let _ = crate::underlay::render(&g, 2, 256);
        let _ = crate::underlay::render(&g, 1, 256);
    }
}
