//! Reply hygiene for a mid-size local model: strip thinking/tool syntax, cut
//! hallucinated transcript turns, drop language-drift tails, trim repetition
//! spirals, enforce the room's no-em-dash rule, and detect garbage so the
//! caller can fall back to an in-voice line instead of posting noise.

use regex::Regex;
use std::sync::OnceLock;

/// In-voice fallback when nothing usable survives.
pub const FALLBACK_REPLY: &str = "eh, my circuits glitched for a sec there, hit me again and i'll actually answer this time";

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

/// Remove `<think>…</think>` blocks (and an unterminated trailing one).
pub fn strip_thinking(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let r = re(&RE, r"(?s)<think>.*?</think>\s*");
    let out = r.replace_all(text, "").to_string();
    match out.find("<think>") {
        Some(i) => out[..i].to_string(),
        None => out,
    }
}

/// Cut at the first hallucinated speaker turn / meta marker. Speaker labels
/// only count at the START of a line ("sig:", "tester:") so prose like "for
/// you: here it is" survives.
pub fn truncate_at_drift(text: &str, known_names: &[String]) -> String {
    static META: OnceLock<Regex> = OnceLock::new();
    let meta = re(&META, r"(?i)(<\|im_(?:start|end)\|>|__URL__|__EMOJI__|\[\d{2,}/\d{2,}\]|^\s*-#\s)");
    let mut cut = meta.find(text).map(|m| m.start());
    let mut offset = 0;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            let l = line.trim_start().to_lowercase();
            let label_end = l.find(':');
            if let Some(e) = label_end {
                let label = l[..e].trim();
                let is_label = !label.is_empty()
                    && label.len() <= 24
                    && !label.contains(' ')
                    && (matches!(label, "sig" | "supersighurt" | "tester" | "user" | "you" | "assistant" | "bot" | "system" | "human")
                        || known_names.iter().any(|n| n.to_lowercase() == label));
                if is_label {
                    cut = Some(cut.map_or(offset, |c| c.min(offset)));
                    break;
                }
            }
        }
        offset += line.len() + 1;
    }
    let Some(c) = cut else { return text.to_string() };
    let head = text[..c].trim_end_matches([' ', '\n', '-', '#', '*', ':']);
    end_on_sentence(head, 0.4)
}

/// Prefer ending on the last sentence terminator when it keeps most of the text.
fn end_on_sentence(head: &str, min_fraction: f32) -> String {
    static END: OnceLock<Regex> = OnceLock::new();
    let r = re(&END, r#"[.!?…](?:["')\]]|\s|$)"#);
    if let Some(m) = r.find_iter(head).last() {
        if m.end() as f32 >= head.len() as f32 * min_fraction {
            return head[..m.end()].trim().to_string();
        }
    }
    head.trim().to_string()
}

fn is_foreign(c: char) -> bool {
    let u = c as u32;
    (0x0400..=0x04FF).contains(&u)   // Cyrillic
        || (0x0590..=0x08FF).contains(&u) // Hebrew, Arabic, Syriac...
        || (0x0900..=0x0DFF).contains(&u) // Indic
        || (0x0E00..=0x0E7F).contains(&u) // Thai
        || (0x1100..=0x11FF).contains(&u) // Hangul Jamo
        || (0x3040..=0x30FF).contains(&u) // Hiragana/Katakana
        || (0x3400..=0x4DBF).contains(&u) || (0x4E00..=0x9FFF).contains(&u) // CJK
        || (0xAC00..=0xD7AF).contains(&u) // Hangul
        || (0xFF00..=0xFFEF).contains(&u) // fullwidth forms
}

/// A mostly-Latin reply that suddenly switches script mid-way gets its
/// foreign tail dropped (language drift). Fully non-Latin replies are kept.
pub fn truncate_foreign_tail(text: &str) -> String {
    let Some(first) = text.char_indices().find(|(_, c)| is_foreign(*c)).map(|(i, _)| i) else {
        return text.to_string();
    };
    let head = &text[..first];
    let latin = head.chars().filter(|c| c.is_ascii_alphabetic()).count();
    if latin < 12 {
        return text.to_string();
    }
    end_on_sentence(head.trim_end(), 0.3)
}

fn is_symbol_token(tok: &str) -> bool {
    !tok.is_empty() && tok.chars().all(|c| !c.is_alphanumeric())
}

/// Trim repetition spirals: repeated phrases, symbol/emoji walls, a word used
/// far too many times, and runaway trailing punctuation.
pub fn spiral_trim(text: &str) -> String {
    let original = text;
    let mut tokens: Vec<&str> = text.split_whitespace().collect();
    // 1) The same 1-4 word phrase repeated more than 4 times in a row.
    'outer: for len in 1..=4usize {
        let mut i = 0;
        while i + len * 5 <= tokens.len() {
            let unit = &tokens[i..i + len];
            let mut reps = 1;
            let mut j = i + len;
            while j + len <= tokens.len() && &tokens[j..j + len] == unit {
                reps += 1;
                j += len;
            }
            if reps > 4 {
                tokens.truncate(i + len);
                break 'outer;
            }
            i += 1;
        }
    }
    // 2) A run of 5+ consecutive symbol/emoji-only tokens: keep one.
    let mut run_start = None;
    let mut cut_at = None;
    for (i, tok) in tokens.iter().enumerate() {
        if is_symbol_token(tok) {
            if run_start.is_none() {
                run_start = Some(i);
            }
            if i + 1 - run_start.unwrap() >= 5 {
                cut_at = Some(run_start.unwrap() + 1);
                break;
            }
        } else {
            run_start = None;
        }
    }
    if let Some(c) = cut_at {
        tokens.truncate(c);
    }
    // 3) Any word used more than 8 times: cut at its 4th use.
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for tok in &tokens {
        let w: String = tok.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
        if w.len() > 1 {
            *counts.entry(w).or_insert(0) += 1;
        }
    }
    let offenders: Vec<String> = counts.into_iter().filter(|(_, c)| *c > 8).map(|(w, _)| w).collect();
    if !offenders.is_empty() {
        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let mut cut = None;
        for (i, tok) in tokens.iter().enumerate() {
            let w: String = tok.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
            if let Some(off) = offenders.iter().find(|o| **o == w) {
                let e = seen.entry(off.as_str()).or_insert(0);
                *e += 1;
                if *e == 4 {
                    cut = Some(i);
                    break;
                }
            }
        }
        if let Some(c) = cut {
            tokens.truncate(c);
        }
    }
    let out = tokens.join(" ");
    // 4) Collapse trailing runs of the same punctuation/emoji.
    let out = collapse_trailing_run(out.trim_end());
    let out = out.trim().to_string();
    if out.len() < original.len() {
        return end_on_sentence(&out, 0.5);
    }
    out
}

/// A long chunk (≥ 40 chars) that appears again later is an echo loop (the
/// model re-quoting a tool result with footnotes); cut at the second copy.
pub fn trim_long_repeats(text: &str) -> String {
    const WIN: usize = 40;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < WIN * 2 {
        return text.to_string();
    }
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cut: Option<usize> = None;
    for i in 0..=chars.len() - WIN {
        let window: String = chars[i..i + WIN].iter().collect();
        if window.trim().len() < WIN / 2 {
            continue;
        }
        if seen.contains(&window) && i >= WIN {
            // Only count it as a repeat if it is not simply overlapping the
            // original occurrence (i.e. the first copy ended before i).
            cut = Some(i);
            break;
        }
        seen.insert(window);
    }
    match cut {
        Some(c) if c >= chars.len() / 4 => {
            let head: String = chars[..c].iter().collect();
            end_on_sentence(head.trim_end_matches(['[', '(', '*', ' ', '\n', ':']), 0.5)
        }
        _ => text.to_string(),
    }
}

/// "wow!!!!!!" -> "wow!", "ok 🐢🐢🐢🐢🐢🐢" -> "ok 🐢": a trailing run of one
/// repeated char collapses to a single copy (3+ for punctuation, 5+ for
/// anything else, so "..." and "!!" survive).
fn collapse_trailing_run(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let Some(&last) = chars.last() else { return String::new() };
    let run = chars.iter().rev().take_while(|c| **c == last).count();
    let threshold = if "!?.,~-:;".contains(last) { 3 } else { 5 };
    if run >= threshold {
        let keep = chars.len() - run + 1;
        return chars[..keep].iter().collect();
    }
    text.to_string()
}

/// The room banned em-dashes; the persona says so too. Replace with commas.
pub fn strip_em_dashes(text: &str) -> String {
    text.replace(" — ", ", ").replace('—', ", ").replace(" – ", ", ").replace('–', "-")
}

/// Nothing a human would want to read: empty, only symbols, or a template leak.
pub fn looks_like_garbage(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    let letters = t.chars().filter(|c| c.is_alphanumeric()).count();
    let has_emoji = t.chars().any(|c| {
        let u = c as u32;
        (0x1F000..=0x1FAFF).contains(&u) || (0x2600..=0x27BF).contains(&u)
    });
    if letters == 0 && !has_emoji {
        return true;
    }
    let lower = t.to_lowercase();
    ["<|im_start|>", "sig form,", "lowercase-casual", "that user's messages", "get to be me", "as an ai language model"]
        .iter()
        .any(|m| lower.contains(m))
}

/// Full pipeline. `None` means "post the fallback instead".
pub fn finalize(raw: &str, known_names: &[String]) -> Option<String> {
    let mut text = strip_thinking(raw);
    text = super::tools::strip_tool_syntax(&text);
    text = truncate_at_drift(&text, known_names);
    text = truncate_foreign_tail(&text);
    text = spiral_trim(&text);
    text = trim_long_repeats(&text);
    text = strip_em_dashes(&text);
    let text = text.trim().to_string();
    if super::tools::looks_like_tool_syntax(&text) || looks_like_garbage(&text) {
        return None;
    }
    Some(text)
}

/// Port of serve_llama.rs clean_reply for the legacy raw backend.
pub fn clean_legacy_reply(decoded: &str, labels: &[String]) -> String {
    fn payload_start(line: &str, label: &str) -> Option<usize> {
        let trimmed = line.trim_start_matches([' ', '\t', '\r']);
        let indent = line.len() - trimmed.len();
        if trimmed.len() >= label.len()
            && trimmed[..label.len()].eq_ignore_ascii_case(label)
            && trimmed[label.len()..].starts_with(':')
        {
            Some(indent + label.len() + 1)
        } else {
            None
        }
    }
    let mut all_labels: Vec<String> = vec!["SuperSighurt".into(), "Assistant".into(), "User".into()];
    all_labels.extend(labels.iter().cloned());
    // Keep the last SuperSighurt/Assistant-labelled segment.
    let mut assistant_payload = None;
    let mut line_starts = vec![0usize];
    line_starts.extend(decoded.char_indices().filter(|(_, c)| *c == '\n').map(|(i, _)| i + 1));
    for &ls in &line_starts {
        for a in ["SuperSighurt", "Assistant"] {
            if let Some(p) = payload_start(&decoded[ls..], a) {
                assistant_payload = Some(ls + p);
            }
        }
    }
    let mut reply = assistant_payload.map(|p| &decoded[p..]).unwrap_or(decoded).trim().to_string();
    loop {
        let mut stripped = None;
        for label in &all_labels {
            if let Some(p) = payload_start(&reply, label) {
                stripped = Some(reply[p..].trim_start().to_string());
                break;
            }
        }
        match stripped {
            Some(s) if s.len() < reply.len() => reply = s,
            _ => break,
        }
    }
    let mut end = reply.len();
    for (i, c) in reply.char_indices() {
        if c != '\n' {
            continue;
        }
        let line = &reply[i + 1..];
        if line.trim_start().starts_with("<|") || all_labels.iter().any(|l| payload_start(line, l).is_some()) {
            end = end.min(i);
        }
    }
    reply[..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_and_tool_syntax_go() {
        assert_eq!(strip_thinking("<think>hmm</think>\nhi"), "hi");
        assert_eq!(strip_thinking("hi <think>unfinished"), "hi ");
        assert_eq!(finalize("<think>x</think>TOOL_CALL: {\"name\":\"a\"}\nok cool", &[]).unwrap(), "ok cool");
    }

    #[test]
    fn drift_only_cuts_line_start_labels() {
        assert_eq!(truncate_at_drift("here's what i found for you: nothing", &[]), "here's what i found for you: nothing");
        assert_eq!(truncate_at_drift("nice one.\ntester: what else", &[]), "nice one.");
        assert_eq!(truncate_at_drift("ok\nwalnutty2: lol", &["walnutty2".into()]), "ok");
        assert_eq!(truncate_at_drift("yo <|im_start|>user", &[]), "yo");
    }

    #[test]
    fn foreign_tail_dropped_but_not_foreign_replies() {
        assert_eq!(truncate_foreign_tail("the answer is forty two, easy. 这是中文的尾巴"), "the answer is forty two, easy.");
        assert_eq!(truncate_foreign_tail("你好世界"), "你好世界");
    }

    #[test]
    fn spirals() {
        assert_eq!(spiral_trim("no no no no no no no way"), "no");
        assert!(spiral_trim("great 🎃✨ 🐢✨ 🍄✨ 🤡✨ 🌈✨ 🔥✨ done").starts_with("great 🎃✨"));
        let many = "lol ".repeat(12) + "ok";
        assert!(spiral_trim(&many).split_whitespace().count() <= 4);
        assert_eq!(spiral_trim("wow!!!!!!"), "wow!");
        assert_eq!(spiral_trim("fine as is."), "fine as is.");
    }

    #[test]
    fn long_repeats_cut() {
        let unit = "[weather] weather Stockholm: 🌤️ +11°C, feels like +9°C, humidity 94%, wind 15km/h";
        let text = format!("it's mostly sunny in stockholm, plus eleven. {unit} i didn't make that up [^1] {unit} check it [^2] {unit}");
        let out = trim_long_repeats(&text);
        assert!(out.len() < text.len());
        assert!(out.starts_with("it's mostly sunny in stockholm, plus eleven."), "{out}");
        assert_eq!(trim_long_repeats("short text stays"), "short text stays");
    }

    #[test]
    fn dashes_and_garbage() {
        assert_eq!(strip_em_dashes("a — b"), "a, b");
        assert!(looks_like_garbage("   "));
        assert!(looks_like_garbage("!!!! ???"));
        assert!(!looks_like_garbage(":3"));
        assert!(!looks_like_garbage("k"));
        assert!(finalize("TOOL_CALL: {\"name\": \"x\"}", &[]).is_none());
        assert_eq!(finalize("yes, but a score.", &[]).unwrap(), "yes, but a score.");
    }

    #[test]
    fn legacy_clean() {
        assert_eq!(clean_legacy_reply("SuperSighurt: hi there\nUser: what", &["zuna".into()]), "hi there");
        assert_eq!(clean_legacy_reply("plain reply\n<|user|>\nmore", &[]), "plain reply");
        assert_eq!(clean_legacy_reply("zuna: fake\nSuperSighurt: real", &["zuna".into()]), "real");
    }
}
