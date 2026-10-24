//! `/RC` rich text (a small XHTML subset) as plain text: tags dropped, entities decoded, white
//! space collapsed as a browser would, `<p>`/`<div>` and `<br>` kept as line breaks.

/// The plain text of an XHTML fragment (lenient: broken markup yields what could be read).
pub(super) fn plain(x: &str) -> String {
    let mut paras: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut rest = x;
    let end_para = |cur: &mut String, paras: &mut Vec<String>| {
        // Spaces around line breaks are layout, not text.
        let t: Vec<&str> = cur.split('\n').map(str::trim).collect();
        let t = t.join("\n");
        if !t.trim().is_empty() {
            paras.push(t.trim().to_string());
        }
        cur.clear();
    };
    while !rest.is_empty() {
        if let Some(tag) = rest.strip_prefix('<') {
            let close = tag.find('>').map_or(tag.len(), |i| i + 1);
            let body = tag.get(..close.saturating_sub(1)).unwrap_or_default();
            rest = tag.get(close..).unwrap_or_default();
            let name: String =
                body.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
            match name.as_str() {
                "br" => cur.push('\n'),
                "p" | "div" | "body" | "li" => end_para(&mut cur, &mut paras),
                _ => {}
            }
        } else {
            let at = rest.find('<').unwrap_or(rest.len());
            let (text, tail) = rest.split_at(at);
            rest = tail;
            for c in decode_entities(text).chars() {
                if c.is_whitespace() {
                    if !cur.is_empty() && !cur.ends_with([' ', '\n']) {
                        cur.push(' ');
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    end_para(&mut cur, &mut paras);
    paras.join("\n")
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(rest.get(..i).unwrap_or_default());
        let tail = rest.get(i + 1..).unwrap_or_default();
        let decoded = tail.find(';').filter(|e| *e <= 10).and_then(|e| {
            let name = tail.get(..e)?;
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ => {
                    let n = name.strip_prefix('#')?;
                    let v = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok()?,
                        None => n.parse().ok()?,
                    };
                    char::from_u32(v)?
                }
            };
            Some((c, e + 1))
        });
        match decoded {
            Some((c, used)) => {
                out.push(c);
                rest = tail.get(used..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}
