use super::*;
use crate::zip;
use cadcraft_render::images::Raster;

const NS: &str = "xmlns=\"http://schemas.microsoft.com/xps/2005/06\" xmlns:x=\"http://schemas.microsoft.com/xps/2005/06/resourcedictionary-key\"";

fn png() -> Vec<u8> {
    let mut pm = Pixmap::new(2, 1).unwrap();
    pm.data_mut().copy_from_slice(&[255, 0, 0, 255, 0, 0, 255, 255]);
    pm.encode_png().unwrap()
}

fn page1() -> String {
    format!(
        r##"<FixedPage {NS} Width="800" Height="600">
<FixedPage.Resources><ResourceDictionary><SolidColorBrush x:Key="b1" Color="#FF800080"/></ResourceDictionary></FixedPage.Resources>
<Path Data="M 100,100 L 300,100 300,200 100,200 Z" Fill="#FFFF0000"/>
<Path Data="M 100,300 L 700,300" Stroke="#FF0000FF" StrokeThickness="10"/>
<Canvas RenderTransform="1,0,0,1,500,350" Clip="M 0,0 L 100,0 100,100 0,100 Z">
<Path Data="M 0,0 L 400,0 400,400 0,400 Z" Fill="#FF00FF00"/></Canvas>
<Glyphs OriginX="50" OriginY="550" FontRenderingEmSize="48" UnicodeString="HHH" Fill="#FF000000" FontUri="/Resources/Fonts/x.odttf"/>
<Path Data="M 400,100 L 600,100 600,200 400,200 Z"><Path.Fill><ImageBrush ImageSource="/Resources/img.png" ViewBox="0,0,2,1" Viewport="400,100,200,100"/></Path.Fill></Path>
<Path Data="M 100,400 L 480,400" Stroke="#FF000000" StrokeThickness="10" StrokeDashArray="4 4"/>
<Path Data="M 650,100 L 750,100 750,200 650,200 Z" Fill="{{StaticResource b1}}"/>
<Path Data="F1 M 650,300 A 50,50 0 1 1 750,300 A 50,50 0 1 1 650,300 Z" Fill="#FF00FFFF"/>
</FixedPage>"##
    )
}

fn package(page1: &str, deflate: bool) -> Vec<u8> {
    let ct = b"<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"/>";
    let rels = b"<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Type=\"http://schemas.microsoft.com/xps/2005/06/fixedrepresentation\" Target=\"/FixedDocumentSequence.fdseq\" Id=\"r1\"/></Relationships>";
    let seq = format!("<FixedDocumentSequence {NS}><DocumentReference Source=\"/Documents/1/FixedDocument.fdoc\"/></FixedDocumentSequence>");
    let doc = format!("<FixedDocument {NS}><PageContent Source=\"Pages/1.fpage\"/><PageContent Source=\"../1/Pages/2.fpage\"/></FixedDocument>");
    let page2 = format!("<FixedPage {NS} Width=\"96\" Height=\"96\" Name=\"Second\"/>");
    zip::build(
        &[
            ("[Content_Types].xml", ct),
            ("_rels/.rels", rels),
            ("FixedDocumentSequence.fdseq", seq.as_bytes()),
            ("Documents/1/FixedDocument.fdoc", doc.as_bytes()),
            ("Documents/1/Pages/1.fpage", page1.as_bytes()),
            ("Documents/1/Pages/2.fpage", page2.as_bytes()),
            ("Resources/img.png", &png()),
        ],
        deflate,
    )
}

/// The pixel at (`xu`, `yu`) in page units (1/96 in) of a render of a `w`-unit-wide page.
fn at(r: &Raster, w: f64, xu: f64, yu: f64) -> [u8; 4] {
    let k = f64::from(r.width) / (w * 0.75);
    let (x, y) = ((xu * 0.75 * k) as usize, (yu * 0.75 * k) as usize);
    let i = (y * r.width as usize + x) * 4;
    [r.rgba[i], r.rgba[i + 1], r.rgba[i + 2], r.rgba[i + 3]]
}

fn render(bytes: &[u8], page: u32) -> Raster {
    crate::underlay::render(bytes, page, 800).unwrap()
}

const WHITE: [u8; 4] = [255, 255, 255, 255];

#[test]
fn sheets_sizes_and_detection() {
    for deflate in [false, true] {
        let b = package(&page1(), deflate);
        let z = Zip::open(&b).unwrap();
        assert!(is_dwfx(&z));
        assert_eq!(sheets(&z).unwrap(), vec!["Sheet 1".to_string(), "Second".to_string()]);
        assert_eq!(crate::underlay::page_size(&b, 1).unwrap(), (600.0, 450.0));
        assert_eq!(crate::underlay::page_size(&b, 2).unwrap(), (72.0, 72.0));
        assert!(crate::underlay::page_size(&b, 3).is_err());
        assert_eq!(crate::underlay::sheets(&b).unwrap().len(), 2);
    }
}

#[test]
fn draws_the_page() {
    let b = package(&page1(), true);
    let r = render(&b, 1);
    assert_eq!(at(&r, 800.0, 200.0, 150.0), [255, 0, 0, 255], "filled rectangle");
    assert_eq!(at(&r, 800.0, 50.0, 50.0), WHITE, "paper");
    assert_eq!(at(&r, 800.0, 200.0, 250.0), WHITE);
    assert_eq!(at(&r, 800.0, 400.0, 300.0), [0, 0, 255, 255], "stroke");
    assert_eq!(at(&r, 800.0, 400.0, 320.0), WHITE);
    assert_eq!(at(&r, 800.0, 550.0, 400.0), [0, 255, 0, 255], "moved canvas");
    assert_eq!(at(&r, 800.0, 650.0, 400.0), WHITE, "clipped");
    assert_eq!(at(&r, 800.0, 550.0, 340.0), WHITE, "above the clip");
    assert_eq!(at(&r, 800.0, 425.0, 150.0), [255, 0, 0, 255], "image left");
    assert_eq!(at(&r, 800.0, 575.0, 150.0), [0, 0, 255, 255], "image right");
    assert_eq!(at(&r, 800.0, 120.0, 400.0), [0, 0, 0, 255], "dash");
    assert_eq!(at(&r, 800.0, 160.0, 400.0), WHITE, "gap");
    assert_eq!(at(&r, 800.0, 700.0, 150.0), [128, 0, 128, 255], "resource brush");
    assert_eq!(at(&r, 800.0, 700.0, 300.0), [0, 255, 255, 255], "arc circle");
    assert_eq!(at(&r, 800.0, 700.0, 380.0), WHITE);
    let dark = |x0: usize, x1: usize, y0: usize, y1: usize| (y0..y1).any(|y| (x0..x1).any(|x| at(&r, 800.0, x as f64, y as f64)[..3] != WHITE[..3]));
    assert!(dark(45, 200, 505, 555), "text near the origin");
    assert!(!dark(300, 800, 480, 600), "no text far away");
}

#[test]
fn gradient_and_unsupported() {
    let page = format!(
        r##"<FixedPage {NS} Width="100" Height="100"><Path Data="M 0,0 L 100,0 100,100 0,100 Z"><Path.Fill><LinearGradientBrush StartPoint="0,0" EndPoint="1,0"><LinearGradientBrush.GradientStops><GradientStop Color="#FFFF0000" Offset="0"/><GradientStop Color="#FF0000FF" Offset="1"/></LinearGradientBrush.GradientStops></LinearGradientBrush></Path.Fill></Path><Foo/></FixedPage>"##
    );
    let b = package(&page, false);
    let z = Zip::open(&b).unwrap();
    let s = scene(&z, 0).unwrap();
    assert_eq!(s.skipped, 1);
    let r = s.render(100).unwrap();
    let left = at(&r, 100.0, 5.0, 50.0);
    let right = at(&r, 100.0, 95.0, 50.0);
    assert!(left[0] > 200 && left[2] < 50, "{left:?}");
    assert!(right[2] > 200 && right[0] < 50, "{right:?}");
}

#[test]
fn resolves_parts() {
    assert_eq!(resolve("Documents/1/FixedDocument.fdoc", "Pages/1.fpage"), "Documents/1/Pages/1.fpage");
    assert_eq!(resolve("/Documents/1/FixedDocument.fdoc", "../2/a.fpage"), "Documents/2/a.fpage");
    assert_eq!(resolve("a/b.fpage", "/Resources/x.png#f"), "Resources/x.png");
}

fn all(bytes: &[u8]) {
    if let Ok(z) = Zip::open(bytes) {
        let _ = is_dwfx(&z);
        if let Ok(names) = sheets(&z) {
            for i in 0..names.len().min(3) {
                if let Ok(s) = scene(&z, i) {
                    let _ = s.render(64);
                }
            }
        }
    }
}

#[test]
fn hostile_packages_do_not_panic() {
    for deflate in [false, true] {
        let b = package(&page1(), deflate);
        for n in (0..b.len()).step_by(if deflate { 1 } else { 3 }) {
            all(&b[..n]);
        }
        for i in (0..b.len()).step_by(if deflate { 1 } else { 3 }) {
            let mut c = b.clone();
            c[i] ^= 0xFF;
            all(&c);
        }
    }
    // The page's own XML cut and flipped inside a sound package.
    let p = page1();
    for n in (0..p.len()).step_by(7) {
        all(&package(&p[..n], false));
    }
    for i in (0..p.len()).step_by(5) {
        let mut c = p.clone().into_bytes();
        c[i] ^= 0x20;
        all(&package(&String::from_utf8_lossy(&c), false));
    }
}

#[test]
fn deep_and_huge_inputs_do_not_panic() {
    let deep = format!(
        "<FixedPage {NS} Width=\"10\" Height=\"10\">{}<Path Data=\"M 0,0 L 5,5\" Stroke=\"#FF000000\"/>{}</FixedPage>",
        "<Canvas>".repeat(10_000),
        "</Canvas>".repeat(10_000)
    );
    all(&package(&deep, false));
    let mut d = String::from("M 0,0 ");
    for _ in 0..300_000 {
        d.push_str("L 1,1 L 2,2 A 1,1 0 1 1 3,3 ");
    }
    let huge = format!("<FixedPage {NS} Width=\"10\" Height=\"10\"><Path Data=\"{d}\" Stroke=\"#FF000000\"/></FixedPage>");
    let b = package(&huge, false);
    let z = Zip::open(&b).unwrap();
    assert!(scene(&z, 0).is_ok());
    let nan = format!(
        "<FixedPage {NS} Width=\"10\" Height=\"10\"><Path Data=\"M 1e39,1e39 L -1e39,3 A 0,1e30 1e30 1 1 5,5\" Stroke=\"#FF000000\" StrokeThickness=\"1e39\" RenderTransform=\"1e30,0,0,1e30,0,0\"/></FixedPage>"
    );
    all(&package(&nan, false));
}
