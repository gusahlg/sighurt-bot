//! Keyless web tools: weather, search, wikipedia, news, page reader,
//! dictionary and urban dictionary. Every fetch is bounded (timeout + size)
//! and `fetch_url` refuses private/loopback targets.

use super::rss;
use crate::web_search::WebSearchClient;
use serde_json::Value;
use std::net::IpAddr;
use std::time::Duration;

const UA: &str = "SuperSighurt/2.0 (Discord bot; local)";
const MAX_BODY: usize = 1_500_000;

async fn get_text(client: &reqwest::Client, url: &str, timeout: Duration, accept: &str) -> Result<String, String> {
    let resp = client
        .get(url)
        .header("User-Agent", UA)
        .header("Accept", accept)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| format!("read failed: {e}"))?;
    if bytes.len() > MAX_BODY {
        return Err("response too large".to_string());
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

pub async fn weather(client: &reqwest::Client, place: &str) -> Result<String, String> {
    let place = place.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = place.to_lowercase();
    if place.is_empty() || matches!(lower.as_str(), "here" | "outside" | "there" | "my city" | "home") {
        return Err("weather error: need a city or place name (i don't know where 'here' is)".to_string());
    }
    if place.chars().count() > 60
        || place.contains(['/', '\\', '?', '#', ':'])
        || !place.chars().all(|c| c.is_alphanumeric() || " .,'-".contains(c))
    {
        return Err("weather error: that doesn't look like a place name".to_string());
    }
    let url = format!(
        "https://wttr.in/{}?format=%l:+%c+%t,+feels+like+%f,+humidity+%h,+wind+%w,+precip+%p",
        urlencode(&place)
    );
    let raw = get_text(client, &url, Duration::from_secs(9), "text/plain").await.map_err(|e| format!("weather error: {e}"))?;
    let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() || line.starts_with('<') || line.to_lowercase().contains("unknown location") {
        return Ok(format!("weather: no data for '{place}'"));
    }
    Ok(format!("weather {}", line.chars().take(240).collect::<String>()))
}

pub async fn web_search(client: &reqwest::Client, search: Option<&WebSearchClient>, query: &str, limit: usize) -> Result<String, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("web_search error: no query".to_string());
    }
    // DuckDuckGo instant answers + Wikimedia search (the bot's own client).
    // (DuckDuckGo's HTML endpoints serve a JS challenge to bots, so no scraping.)
    let mut lines: Vec<String> = Vec::new();
    if let Some(s) = search {
        if let Ok(ctx) = s.search(query).await {
            lines = ctx
                .results
                .iter()
                .take(limit)
                .map(|r| format!("{} — {} ({})", r.title, r.snippet.chars().take(220).collect::<String>(), r.url))
                .collect();
        }
    }
    if lines.is_empty() {
        // Last resort: Wikipedia's own title search (keyless, reliable).
        let url = format!("https://en.wikipedia.org/w/rest.php/v1/search/page?q={}&limit={limit}", urlencode(query));
        if let Ok(body) = get_text(client, &url, Duration::from_secs(8), "application/json").await {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                for page in v["pages"].as_array().into_iter().flatten() {
                    let title = page["title"].as_str().unwrap_or("");
                    let key = page["key"].as_str().unwrap_or(title);
                    let desc = page["description"].as_str().unwrap_or("");
                    let excerpt = clean_html(page["excerpt"].as_str().unwrap_or(""));
                    if !title.is_empty() {
                        lines.push(format!("{title} — {desc}. {} (https://en.wikipedia.org/wiki/{})", excerpt.chars().take(200).collect::<String>(), key.replace(' ', "_")));
                    }
                }
            }
        }
    }
    if lines.is_empty() {
        return Ok(format!("web_search: no results for '{query}'"));
    }
    Ok(format!(
        "Search results for '{query}':\n{}",
        lines.iter().enumerate().map(|(i, l)| format!("[{}] {l}", i + 1)).collect::<Vec<_>>().join("\n")
    ))
}

pub async fn wiki(client: &reqwest::Client, topic: &str) -> Result<String, String> {
    let topic = topic.trim();
    if topic.is_empty() {
        return Err("wiki error: no topic".to_string());
    }
    // Resolve the title through search first (handles casing/redirect-ish input).
    let search_url = format!(
        "https://en.wikipedia.org/w/rest.php/v1/search/title?q={}&limit=1",
        urlencode(topic)
    );
    let title = match get_text(client, &search_url, Duration::from_secs(8), "application/json").await {
        Ok(body) => serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["pages"][0]["key"].as_str().map(str::to_string))
            .unwrap_or_else(|| topic.replace(' ', "_")),
        Err(_) => topic.replace(' ', "_"),
    };
    let url = format!("https://en.wikipedia.org/api/rest_v1/page/summary/{}", urlencode(&title));
    let body = get_text(client, &url, Duration::from_secs(8), "application/json").await.map_err(|e| format!("wiki error: {e}"))?;
    let v: Value = serde_json::from_str(&body).map_err(|_| "wiki error: bad response".to_string())?;
    let extract = v["extract"].as_str().unwrap_or("").trim();
    if extract.is_empty() {
        return Ok(format!("wiki: no article found for '{topic}'"));
    }
    let page = v["content_urls"]["desktop"]["page"].as_str().unwrap_or("");
    Ok(format!(
        "{}: {} ({page})",
        v["title"].as_str().unwrap_or(&title),
        extract.chars().take(900).collect::<String>()
    ))
}

pub async fn news(client: &reqwest::Client, feeds: &[String], topic: Option<&str>, limit: usize) -> Result<String, String> {
    let mut headlines: Vec<(String, rss::Headline)> = Vec::new();
    let sources: Vec<(String, String)> = match topic {
        Some(t) if !t.trim().is_empty() => vec![(
            format!("news: {}", t.trim()),
            format!("https://news.google.com/rss/search?q={}&hl=en-US&gl=US&ceid=US:en", urlencode(t.trim())),
        )],
        _ => feeds.iter().map(|f| (short_host(f), f.clone())).collect(),
    };
    if sources.is_empty() {
        return Err("news error: no feeds configured".to_string());
    }
    let per_feed = if sources.len() == 1 { limit } else { (limit / sources.len()).max(2) };
    for (label, url) in sources {
        match get_text(client, &url, Duration::from_secs(9), "application/rss+xml, application/atom+xml, text/xml").await {
            Ok(body) => {
                for h in rss::parse_feed(&body, per_feed) {
                    headlines.push((label.clone(), h));
                }
            }
            Err(e) => tracing::debug!("news feed {url} failed: {e}"),
        }
    }
    if headlines.is_empty() {
        return Ok("news: no headlines could be fetched right now".to_string());
    }
    headlines.truncate(limit.max(1));
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let lines = headlines
        .iter()
        .map(|(src, h)| {
            let mut line = format!("- [{src}] {}", h.title);
            if let Some(s) = &h.summary {
                if !s.is_empty() && !h.title.contains(s.as_str()) {
                    line.push_str(&format!(" — {}", s.chars().take(160).collect::<String>()));
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("Headlines as of {now}:\n{lines}"))
}

fn short_host(url: &str) -> String {
    let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url);
    host.trim_start_matches("www.").trim_start_matches("feeds.").to_string()
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_broadcast()
                || v4.is_unspecified() || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])
                || v4.octets()[0] == 0)
        }
        IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80),
    }
}

pub async fn fetch_url(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let url = url.trim().trim_matches(['<', '>']);
    let parsed = reqwest::Url::parse(url).map_err(|_| "fetch_url error: not a valid http(s) url".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("fetch_url error: only http(s) urls".to_string());
    }
    let host = parsed.host_str().ok_or("fetch_url error: no host")?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs = tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| format!("fetch_url error: dns: {e}"))?;
    let mut any = false;
    for a in addrs {
        any = true;
        if !is_public_ip(a.ip()) {
            return Err("fetch_url error: that host is not a public address".to_string());
        }
    }
    if !any {
        return Err("fetch_url error: host did not resolve".to_string());
    }
    let resp = client
        .get(parsed.clone())
        .header("User-Agent", UA)
        .header("Accept", "text/html, text/plain, application/json;q=0.5")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("fetch_url error: {e}"))?;
    let status = resp.status();
    if status.is_redirection() {
        let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("?");
        return Ok(format!("fetch_url: {url} redirects to {loc} (fetch that one if you want it)"));
    }
    let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let bytes = resp.bytes().await.map_err(|e| format!("fetch_url error: {e}"))?;
    if !status.is_success() {
        return Err(format!("fetch_url error: HTTP {status}"));
    }
    let body = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BODY)]).to_string();
    let (title, text) = if ctype.contains("html") || body.trim_start().starts_with('<') {
        (html_title(&body), readable_text(&body))
    } else {
        (String::new(), body.split_whitespace().collect::<Vec<_>>().join(" "))
    };
    let text: String = text.chars().take(2600).collect();
    if text.trim().is_empty() {
        return Ok(format!("fetch_url: {url} had no readable text"));
    }
    Ok(if title.is_empty() { format!("{url}\n{text}") } else { format!("{title} ({url})\n{text}") })
}

pub async fn define(client: &reqwest::Client, word: &str) -> Result<String, String> {
    let word = word.trim().trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    if word.is_empty() || word.contains(' ') {
        return Err("define error: give one word".to_string());
    }
    // Wiktionary first (keyless, reliable), dictionaryapi.dev as a fallback.
    let url = format!("https://en.wiktionary.org/api/rest_v1/page/definition/{}", urlencode(&word));
    if let Ok(body) = get_text(client, &url, Duration::from_secs(8), "application/json").await {
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            let mut lines = Vec::new();
            for entry in v["en"].as_array().into_iter().flatten().take(3) {
                let pos = entry["partOfSpeech"].as_str().unwrap_or("").to_lowercase();
                for def in entry["definitions"].as_array().into_iter().flatten().take(2) {
                    let d = clean_html(def["definition"].as_str().unwrap_or(""));
                    if !d.is_empty() {
                        lines.push(format!("({pos}) {}", d.chars().take(220).collect::<String>()));
                    }
                }
            }
            if !lines.is_empty() {
                return Ok(format!("{word}:\n{}", lines.join("\n")));
            }
        }
    }
    let url = format!("https://api.dictionaryapi.dev/api/v2/entries/en/{}", urlencode(&word));
    let body = match get_text(client, &url, Duration::from_secs(8), "application/json").await {
        Ok(b) => b,
        Err(e) if e.contains("404") => return Ok(format!("define: no dictionary entry for '{word}'")),
        Err(e) => return Err(format!("define error: {e}")),
    };
    let v: Value = serde_json::from_str(&body).map_err(|_| "define error: bad response".to_string())?;
    let mut lines = Vec::new();
    for entry in v.as_array().into_iter().flatten().take(1) {
        for meaning in entry["meanings"].as_array().into_iter().flatten().take(3) {
            let pos = meaning["partOfSpeech"].as_str().unwrap_or("");
            for def in meaning["definitions"].as_array().into_iter().flatten().take(2) {
                let d = def["definition"].as_str().unwrap_or("");
                if !d.is_empty() {
                    lines.push(format!("({pos}) {d}"));
                }
            }
        }
    }
    if lines.is_empty() {
        return Ok(format!("define: no dictionary entry for '{word}'"));
    }
    Ok(format!("{word}:\n{}", lines.join("\n")))
}

pub async fn urban(client: &reqwest::Client, term: &str) -> Result<String, String> {
    let term = term.trim();
    if term.is_empty() {
        return Err("urban error: no term".to_string());
    }
    let url = format!("https://api.urbandictionary.com/v0/define?term={}", urlencode(term));
    let body = get_text(client, &url, Duration::from_secs(8), "application/json").await.map_err(|e| format!("urban error: {e}"))?;
    let v: Value = serde_json::from_str(&body).map_err(|_| "urban error: bad response".to_string())?;
    let Some(list) = v["list"].as_array().filter(|l| !l.is_empty()) else {
        return Ok(format!("urban: nobody has defined '{term}' yet"));
    };
    let best = list
        .iter()
        .max_by_key(|d| d["thumbs_up"].as_i64().unwrap_or(0) - d["thumbs_down"].as_i64().unwrap_or(0))
        .unwrap_or(&list[0]);
    let clean = |s: &str| s.replace(['[', ']'], "").split_whitespace().collect::<Vec<_>>().join(" ");
    let def = clean(best["definition"].as_str().unwrap_or(""));
    let ex = clean(best["example"].as_str().unwrap_or(""));
    let mut out = format!("urban '{}': {}", best["word"].as_str().unwrap_or(term), def.chars().take(500).collect::<String>());
    if !ex.is_empty() {
        out.push_str(&format!(" | example: {}", ex.chars().take(200).collect::<String>()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// html helpers
// ---------------------------------------------------------------------------

fn attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=\"");
    let start = tag.find(&pat)? + pat.len();
    let end = tag[start..].find('"')? + start;
    Some(rss::decode_entities(&tag[start..end]))
}

fn clean_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    rss::decode_entities(&out).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn html_title(html: &str) -> String {
    let lower = html.to_lowercase();
    let Some(s) = lower.find("<title") else { return String::new() };
    let Some(gt) = lower[s..].find('>').map(|g| s + g + 1) else { return String::new() };
    let Some(e) = lower[gt..].find("</title>").map(|e| gt + e) else { return String::new() };
    clean_html(&html[gt..e]).chars().take(160).collect()
}

/// Drop script/style/nav/header/footer blocks, then strip tags.
fn readable_text(html: &str) -> String {
    let lower = html.to_lowercase();
    let mut keep = String::with_capacity(html.len());
    let mut i = 0;
    let blocked = ["script", "style", "nav", "header", "footer", "noscript", "svg", "aside", "head", "title"];
    while i < html.len() {
        if lower[i..].starts_with('<') {
            let tag_name: String = lower[i + 1..].chars().take_while(|c| c.is_alphanumeric()).collect();
            if blocked.contains(&tag_name.as_str()) {
                let close = format!("</{tag_name}");
                match lower[i..].find(&close) {
                    Some(c) => {
                        let after = lower[i + c..].find('>').map(|g| i + c + g + 1).unwrap_or(html.len());
                        i = after;
                        continue;
                    }
                    None => break,
                }
            }
            // Block-level tags become newlines so paragraphs stay separated.
            if matches!(tag_name.as_str(), "p" | "br" | "div" | "li" | "h1" | "h2" | "h3" | "h4" | "tr" | "section" | "article") {
                keep.push('\n');
            }
            match html[i..].find('>') {
                Some(g) => i += g + 1,
                None => break,
            }
            continue;
        }
        let ch = html[i..].chars().next().unwrap_or(' ');
        keep.push(ch);
        i += ch.len_utf8();
    }
    let decoded = rss::decode_entities(&keep);
    decoded
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_text_drops_scripts() {
        let html = "<html><head><title>T &amp; U</title><script>var x=1;</script></head><body><nav>menu</nav><p>Hello <b>world</b></p><p>Second</p></body></html>";
        assert_eq!(html_title(html), "T & U");
        assert_eq!(readable_text(html), "Hello world\nSecond");
    }

    #[test]
    fn public_ip_guard() {
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("10.1.2.3".parse().unwrap()));
        assert!(!is_public_ip("100.118.41.103".parse().unwrap()));
        assert!(!is_public_ip("169.254.1.1".parse().unwrap()));
        assert!(is_public_ip("93.184.216.34".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
    }

    #[test]
    fn url_helpers() {
        assert_eq!(urlencode("ham atoms"), "ham%20atoms");
        assert_eq!(urldecode("a%20b+c"), "a b c");
        assert_eq!(short_host("https://feeds.bbci.co.uk/news/world/rss.xml"), "bbci.co.uk");
    }
}
