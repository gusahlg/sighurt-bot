//! Minimal RSS 2.0 / Atom headline parser (std only).

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Headline {
    pub title: String,
    pub link: String,
    pub published: Option<String>,
    pub summary: Option<String>,
}

const SUMMARY_MAX: usize = 240;

/// Parse an RSS 2.0 (`<item>`) or Atom (`<entry>`) feed into at most `limit` headlines.
pub fn parse_feed(xml: &str, limit: usize) -> Vec<Headline> {
    let mut out = Vec::new();
    let is_atom = xml.contains("<feed") && !xml.contains("<item>") && !xml.contains("<item ");
    let (open, close) = if is_atom { ("<entry", "</entry>") } else { ("<item", "</item>") };
    let mut rest = xml;
    while out.len() < limit {
        let Some(start) = rest.find(open) else { break };
        let Some(end_rel) = rest[start..].find(close) else { break };
        let block = &rest[start..start + end_rel];
        rest = &rest[start + end_rel + close.len()..];
        let Some(title) = element_text(block, "title").filter(|t| !t.is_empty()) else { continue };
        let link = if is_atom {
            atom_link(block).unwrap_or_default()
        } else {
            element_text(block, "link").unwrap_or_default()
        };
        let published = element_text(block, "pubDate")
            .or_else(|| element_text(block, "published"))
            .or_else(|| element_text(block, "updated"))
            .or_else(|| element_text(block, "dc:date"));
        let summary = element_text(block, "description")
            .or_else(|| element_text(block, "summary"))
            .or_else(|| element_text(block, "content"))
            .map(|s| truncate(&s, SUMMARY_MAX))
            .filter(|s| !s.is_empty());
        out.push(Headline { title, link, published, summary });
    }
    out
}

/// Text of the first `<tag ...>...</tag>` in `block`, decoded and cleaned.
fn element_text(block: &str, tag: &str) -> Option<String> {
    let open_a = format!("<{tag}>");
    let open_b = format!("<{tag} ");
    let close = format!("</{tag}>");
    let (start, tag_len) = match (block.find(&open_a), block.find(&open_b)) {
        (Some(a), Some(b)) if b < a => (b, block[b..].find('>')? + 1),
        (Some(a), _) => (a, open_a.len()),
        (None, Some(b)) => (b, block[b..].find('>')? + 1),
        (None, None) => return None,
    };
    let body_start = start + tag_len;
    let end = block[body_start..].find(&close)? + body_start;
    Some(clean_text(&block[body_start..end]))
}

fn atom_link(block: &str) -> Option<String> {
    let mut rest = block;
    let mut fallback = None;
    while let Some(pos) = rest.find("<link") {
        let tag_end = rest[pos..].find('>')? + pos;
        let tag = &rest[pos..tag_end];
        let href = attr(tag, "href");
        let rel = attr(tag, "rel");
        if let Some(h) = href {
            match rel.as_deref() {
                None | Some("alternate") => return Some(h),
                _ => {
                    if fallback.is_none() {
                        fallback = Some(h);
                    }
                }
            }
        }
        rest = &rest[tag_end..];
    }
    fallback
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=\"");
    let start = tag.find(&pat)? + pat.len();
    let end = tag[start..].find('"')? + start;
    Some(decode_entities(&tag[start..end]))
}

/// CDATA unwrap, HTML strip, entity decode, whitespace collapse.
fn clean_text(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if let Some(inner) = s.strip_prefix("<![CDATA[") {
        s = inner.strip_suffix("]]>").unwrap_or(inner).to_string();
    }
    let decoded = decode_entities(&strip_tags(&s));
    // Entity-encoded HTML ("&lt;p&gt;text&lt;/p&gt;") is common in feeds:
    // strip again only when the decoded text contains real-looking tags.
    let stripped = if looks_like_html(&decoded) { strip_tags(&decoded) } else { decoded };
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn looks_like_html(s: &str) -> bool {
    let lower = s.to_lowercase();
    ["<p>", "<p ", "</p>", "<br", "<a ", "<b>", "<i>", "<em>", "<strong>", "<div", "<span", "<img", "<ul", "<li", "<h1", "<h2", "<h3"]
        .iter()
        .any(|t| lower.contains(t))
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Decode the XML/HTML entities that matter for headlines.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let Some(semi) = tail.find(';').filter(|i| *i <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    match cut.rfind(' ') {
        Some(i) if i > max / 2 => format!("{}…", &cut[..i]),
        _ => format!("{cut}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Feed</title>
<item><title><![CDATA[First &amp; foremost]]></title><link>https://a.example/1</link><pubDate>Mon, 01 Sep 2026 10:00:00 GMT</pubDate><description><![CDATA[<p>Hello <b>world</b> &#8212; it works</p>]]></description></item>
<item><title>Second</title><link>https://a.example/2</link></item>
<item><title>Third</title><link>https://a.example/3</link><description>plain</description></item>
</channel></rss>"#;

    const ATOM: &str = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><title>F</title>
<entry><title>Atom one</title><link rel="self" href="https://x/self"/><link rel="alternate" href="https://x/1"/><updated>2026-09-01T00:00:00Z</updated><summary>s1</summary></entry>
<entry><title type="html">Atom &lt;two&gt;</title><link href="https://x/2"/><content type="html">&lt;p&gt;c2&lt;/p&gt;</content></entry>
</feed>"#;

    #[test]
    fn parses_rss() {
        let h = parse_feed(RSS, 10);
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].title, "First & foremost");
        assert_eq!(h[0].link, "https://a.example/1");
        assert_eq!(h[0].summary.as_deref(), Some("Hello world — it works"));
        assert!(h[0].published.as_deref().unwrap().starts_with("Mon"));
        assert_eq!(h[1].summary, None);
        assert_eq!(parse_feed(RSS, 1).len(), 1);
    }

    #[test]
    fn parses_atom() {
        let h = parse_feed(ATOM, 10);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].link, "https://x/1");
        assert_eq!(h[0].summary.as_deref(), Some("s1"));
        assert_eq!(h[1].title, "Atom <two>");
        assert_eq!(h[1].link, "https://x/2");
        assert_eq!(h[1].summary.as_deref(), Some("c2"));
    }

    #[test]
    fn garbage_is_empty() {
        assert!(parse_feed("", 5).is_empty());
        assert!(parse_feed("<html>nope</html>", 5).is_empty());
        assert_eq!(decode_entities("a &amp; b &#65; &#x42; &bogus; c"), "a & b A B &bogus; c");
    }
}
