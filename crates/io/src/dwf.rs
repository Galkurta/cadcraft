//! DWF packages (ZIP): DWFx (OPC / XPS, read by [`crate::dwfx`]) or a classic DWF 6 package, a ZIP
//! with a `manifest.xml` and W2D sections (`*.w2d`, read by [`crate::w2d`]).

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::underlay::Scene;
use crate::zip::{MAX_ENTRY_BYTES, Zip};

/// Most bytes of a manifest that is read.
const MAX_MANIFEST: usize = 16 << 20;
/// Most XML events walked in a manifest.
const MAX_EVENTS: usize = 2_000_000;

const NO_SHEETS: &str = "the package holds no 2D sheets";

/// The sheets of a package, in order.
pub(crate) fn sheets(bytes: &[u8]) -> Result<Vec<String>, String> {
    let zip = Zip::open(bytes)?;
    if crate::dwfx::is_dwfx(&zip) {
        return crate::dwfx::sheets(&zip);
    }
    Ok(sections(&zip)?.into_iter().map(|s| s.title).collect())
}

/// Sheet `index` (0-based) of a package.
pub(crate) fn scene(bytes: &[u8], index: usize) -> Result<Scene, String> {
    let zip = Zip::open(bytes)?;
    if crate::dwfx::is_dwfx(&zip) {
        return crate::dwfx::scene(&zip, index);
    }
    let all = sections(&zip)?;
    let sec = all.get(index).ok_or_else(|| format!("the package has no sheet {}", index + 1))?;
    let data = zip.read(&sec.entry, MAX_ENTRY_BYTES)?;
    crate::w2d::scene_with_size(&data, None)
}

struct Section {
    entry: String,
    title: String,
}

/// A part name as compared here: forward slashes, no leading `./` or `/`, lower case.
fn norm(name: &str) -> String {
    let n = name.trim().replace('\\', "/").to_ascii_lowercase();
    n.trim_start_matches("./").trim_start_matches('/').to_string()
}

/// The `.w2d` sections: those the manifest lists first, in its order, then the rest in archive
/// order. A section is titled by the nearest `title` (or `name`) attribute before its reference,
/// else by its file name.
fn sections(zip: &Zip) -> Result<Vec<Section>, String> {
    let all: Vec<(String, String)> = zip.names().map(|n| (n.to_string(), norm(n))).filter(|(_, k)| k.ends_with(".w2d")).collect();
    if all.is_empty() {
        return Err(NO_SHEETS.into());
    }
    let mut order: Vec<(usize, Option<String>)> = Vec::new();
    if zip.contains("manifest.xml")
        && let Ok(xml) = zip.read("manifest.xml", MAX_MANIFEST)
    {
        listed(&String::from_utf8_lossy(&xml), &all, &mut order);
    }
    for i in 0..all.len() {
        if !order.iter().any(|(j, _)| *j == i) {
            order.push((i, None));
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|(i, title)| {
            let (entry, _) = all.get(i)?;
            let title = title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| stem(&entry.replace('\\', "/")));
            Some(Section { entry: entry.clone(), title })
        })
        .collect())
}

fn stem(key: &str) -> String {
    let file = key.rsplit('/').next().unwrap_or(key);
    file.rsplit_once('.').map_or(file, |(s, _)| s).to_string()
}

/// The entries a manifest refers to, in order, with their titles.
fn listed(xml: &str, all: &[(String, String)], order: &mut Vec<(usize, Option<String>)>) {
    let mut rd = Reader::from_str(xml);
    let mut title: Option<String> = None;
    for _ in 0..MAX_EVENTS {
        let e = match rd.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => e,
            Ok(Event::Eof) | Err(_) => return,
            Ok(_) => continue,
        };
        let attrs: Vec<(String, String)> = e
            .attributes()
            .flatten()
            .filter_map(|a| {
                let k = String::from_utf8_lossy(a.key.as_ref()).to_ascii_lowercase();
                Some((k, a.normalized_value(XmlVersion::Explicit1_0).ok()?.into_owned()))
            })
            .collect();
        let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        if let Some(t) = get("title").or_else(|| get("name")) {
            title = Some(t);
        }
        for (k, v) in &attrs {
            if (k == "href" || k == "source")
                && let Some(i) = find(all, v)
                && !order.iter().any(|(j, _)| *j == i)
            {
                order.push((i, title.clone()));
            }
        }
    }
}

/// The index of the `.w2d` entry that `reference` names (whole path, or a trailing part of it).
fn find(all: &[(String, String)], reference: &str) -> Option<usize> {
    let r = norm(reference);
    if !r.ends_with(".w2d") {
        return None;
    }
    all.iter().position(|(_, k)| *k == r).or_else(|| all.iter().position(|(_, k)| k.ends_with(&format!("/{r}")) || r.ends_with(&format!("/{k}"))))
}

#[cfg(test)]
#[path = "dwf_tests.rs"]
mod tests;
