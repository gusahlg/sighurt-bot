//! Tools that read the bot's own Discord knowledge: the per-channel TSV logs
//! (`data/channels/<guild>/<channel>.tsv`, written by channel_log), the rules
//! channel, and live guild state (channel names, voice occupancy, counts).

use super::ToolCtx;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use twilight_http::Client;
use twilight_model::channel::ChannelType;
use twilight_model::id::marker::GuildMarker;
use twilight_model::id::Id;

// ---------------------------------------------------------------------------
// guild directory (channel names, voice state, counts)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ChannelInfo {
    pub name: String,
    pub guild_id: u64,
    pub is_voice: bool,
    pub is_thread: bool,
}

#[derive(Default)]
struct DirectoryInner {
    channels: HashMap<u64, ChannelInfo>,
    guild_names: HashMap<u64, String>,
    /// (guild, user) -> (voice channel, display name)
    voice: HashMap<(u64, u64), (u64, String)>,
    member_counts: HashMap<u64, (u64, Option<u64>)>,
    refreshed: Option<Instant>,
}

/// Live guild knowledge, refreshed from the REST API and gateway events.
#[derive(Default)]
pub struct Directory {
    inner: RwLock<DirectoryInner>,
}

impl Directory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-read channel + thread names and member counts for every guild the
    /// bot is in. Cheap (a few REST calls per guild); called at boot and
    /// periodically.
    pub async fn refresh(&self, http: &Client) {
        let guilds = match http.current_user_guilds().await {
            Ok(r) => r.models().await.unwrap_or_default(),
            Err(e) => {
                tracing::warn!("directory: list guilds failed: {e}");
                return;
            }
        };
        let mut channels = HashMap::new();
        let mut guild_names = HashMap::new();
        let mut counts = HashMap::new();
        for g in guilds {
            guild_names.insert(g.id.get(), g.name.clone());
            if let Ok(resp) = http.guild(g.id).with_counts(true).await {
                if let Ok(full) = resp.model().await {
                    counts.insert(g.id.get(), (full.approximate_member_count.unwrap_or(0), full.approximate_presence_count));
                }
            }
            if let Ok(resp) = http.guild_channels(g.id).await {
                for c in resp.models().await.unwrap_or_default() {
                    let is_voice = matches!(c.kind, ChannelType::GuildVoice | ChannelType::GuildStageVoice);
                    channels.insert(
                        c.id.get(),
                        ChannelInfo { name: c.name.clone().unwrap_or_default(), guild_id: g.id.get(), is_voice, is_thread: false },
                    );
                }
            }
            if let Ok(resp) = http.active_threads(g.id).await {
                if let Ok(list) = resp.model().await {
                    for t in list.threads {
                        channels.insert(
                            t.id.get(),
                            ChannelInfo { name: t.name.clone().unwrap_or_default(), guild_id: g.id.get(), is_voice: false, is_thread: true },
                        );
                    }
                }
            }
        }
        let mut inner = self.inner.write();
        inner.channels = channels;
        inner.guild_names = guild_names;
        inner.member_counts = counts;
        inner.refreshed = Some(Instant::now());
    }

    /// Offline name map for tests/tools without a Discord token:
    /// `{"channels": {"<id>": {"name": ".."}}}` or `{"<id>": "name"}`.
    pub fn load_offline(&self, guild_id: u64, names: &serde_json::Value) {
        let mut inner = self.inner.write();
        let map = names.get("channels").unwrap_or(names);
        if let Some(obj) = map.as_object() {
            for (id, v) in obj {
                let Ok(id) = id.parse::<u64>() else { continue };
                let name = v.as_str().map(str::to_string).or_else(|| v.get("name").and_then(|n| n.as_str()).map(str::to_string));
                if let Some(name) = name {
                    inner.channels.insert(id, ChannelInfo { name, guild_id, is_voice: false, is_thread: false });
                }
            }
        }
        inner.refreshed = Some(Instant::now());
    }

    pub fn needs_refresh(&self) -> bool {
        self.inner.read().refreshed.map_or(true, |t| t.elapsed() > Duration::from_secs(3600))
    }

    pub fn apply_voice(&self, state: &twilight_model::voice::VoiceState) {
        let Some(guild_id) = state.guild_id else { return };
        let key = (guild_id.get(), state.user_id.get());
        let mut inner = self.inner.write();
        match state.channel_id {
            Some(ch) => {
                let name = state
                    .member
                    .as_ref()
                    .map(|m| m.nick.clone().unwrap_or_else(|| m.user.name.clone()))
                    .unwrap_or_else(|| format!("user{}", state.user_id.get()));
                inner.voice.insert(key, (ch.get(), name));
            }
            None => {
                inner.voice.remove(&key);
            }
        }
    }

    pub fn channel_name(&self, channel_id: u64) -> Option<String> {
        self.inner.read().channels.get(&channel_id).map(|c| c.name.clone())
    }

    pub fn guild_name(&self, guild_id: u64) -> Option<String> {
        self.inner.read().guild_names.get(&guild_id).cloned()
    }

    pub fn member_counts(&self, guild_id: u64) -> Option<(u64, Option<u64>)> {
        self.inner.read().member_counts.get(&guild_id).copied()
    }

    /// `#name` -> id within a guild (case-insensitive, emoji/punctuation tolerant).
    pub fn resolve_channel(&self, guild_id: u64, name: &str) -> Option<u64> {
        let want = norm_channel(name);
        if want.is_empty() {
            return None;
        }
        if let Ok(id) = want.parse::<u64>() {
            return Some(id);
        }
        let inner = self.inner.read();
        let mut best: Option<(u64, usize)> = None;
        for (id, c) in inner.channels.iter().filter(|(_, c)| c.guild_id == guild_id) {
            let n = norm_channel(&c.name);
            if n == want {
                return Some(*id);
            }
            if n.contains(&want) || want.contains(&n) && !n.is_empty() {
                let score = n.len().abs_diff(want.len());
                if best.map_or(true, |(_, s)| score < s) {
                    best = Some((*id, score));
                }
            }
        }
        best.map(|(id, _)| id)
    }

    /// Voice occupancy lines for a guild: "#general-voice: a, b".
    pub fn voice_lines(&self, guild_id: u64) -> Vec<String> {
        let inner = self.inner.read();
        let mut by_channel: HashMap<u64, Vec<String>> = HashMap::new();
        for ((g, _), (ch, name)) in inner.voice.iter() {
            if *g == guild_id {
                by_channel.entry(*ch).or_default().push(name.clone());
            }
        }
        let mut lines: Vec<String> = by_channel
            .into_iter()
            .map(|(ch, mut names)| {
                names.sort();
                let cname = inner.channels.get(&ch).map(|c| c.name.clone()).unwrap_or_else(|| format!("voice-{ch}"));
                format!("#{cname}: {}", names.join(", "))
            })
            .collect();
        lines.sort();
        lines
    }
}

/// Lowercase, strip `#`, emoji and punctuation so "🎃general" == "general".
pub fn norm_channel(name: &str) -> String {
    name.trim()
        .trim_start_matches('#')
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>()
        .to_lowercase()
        .trim_matches(['-', '_'])
        .to_string()
}

// ---------------------------------------------------------------------------
// TSV log access
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct LogRow {
    pub id: u64,
    pub ts: String,
    pub user_id: u64,
    pub author: String,
    pub text: String,
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn parse_row(line: &str) -> Option<LogRow> {
    let mut cols = line.splitn(6, '\t');
    let id = cols.next()?.parse().ok()?;
    let ts = cols.next()?.to_string();
    let user_id = cols.next()?.parse().unwrap_or(0);
    let author = unescape(cols.next()?);
    let _reply_to = cols.next()?;
    let text = unescape(cols.next().unwrap_or(""));
    Some(LogRow { id, ts, user_id, author, text })
}

/// Read one channel's rows (oldest first, as logged).
pub fn read_channel(path: &Path) -> Vec<LogRow> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter_map(parse_row).collect())
        .unwrap_or_default()
}

/// Every (channel_id, path) in a guild's log bucket.
pub fn guild_channel_files(data_root: &Path, guild_id: u64) -> Vec<(u64, PathBuf)> {
    let dir = data_root.join(guild_id.to_string());
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<(u64, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let stem = name.strip_suffix(".tsv")?;
            if stem.ends_with(".reactions") {
                return None;
            }
            Some((stem.parse().ok()?, e.path()))
        })
        .collect();
    out.sort();
    out
}

/// Net reaction counts per message id from `<channel>.reactions.tsv`.
fn reaction_counts(data_root: &Path, guild_id: u64, channel_id: u64) -> HashMap<u64, i64> {
    let path = data_root.join(guild_id.to_string()).join(format!("{channel_id}.reactions.tsv"));
    let mut counts = HashMap::new();
    if let Ok(s) = std::fs::read_to_string(path) {
        for line in s.lines() {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() >= 5 {
                if let (Ok(id), Ok(delta)) = (cols[0].parse::<u64>(), cols[4].parse::<i64>()) {
                    *counts.entry(id).or_insert(0) += delta;
                }
            }
        }
    }
    counts
}

fn snippet(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        format!("{}…", flat.chars().take(limit).collect::<String>())
    }
}

fn ts_secs(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts).map(|d| d.timestamp()).unwrap_or(0)
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn channel_label(ctx: &ToolCtx<'_>, channel_id: u64) -> String {
    ctx.directory.channel_name(channel_id).map(|n| format!("#{n}")).unwrap_or_else(|| format!("#chan-{channel_id}"))
}

fn author_matches(row: &LogRow, author: Option<&str>) -> bool {
    match author {
        None => true,
        Some(a) => {
            let a = a.trim_start_matches('@').to_lowercase();
            row.author.to_lowercase() == a || row.author.to_lowercase().contains(&a)
        }
    }
}

fn selected_channels(ctx: &ToolCtx<'_>, guild_id: u64, channel: Option<&str>) -> Result<Vec<(u64, PathBuf)>, String> {
    let all = guild_channel_files(ctx.data_root, guild_id);
    match channel {
        None => Ok(all),
        Some(name) => {
            let id = ctx.directory.resolve_channel(guild_id, name).ok_or_else(|| format!("no channel called '{name}' here"))?;
            Ok(all.into_iter().filter(|(cid, _)| *cid == id).collect())
        }
    }
}

pub fn search(ctx: &ToolCtx<'_>, query: &str, channel: Option<&str>, author: Option<&str>, n: Option<usize>, limit: usize) -> Result<String, String> {
    let guild_id = ctx.guild_id.ok_or("search_discord error: only works inside a server, not in DMs")?;
    let query = query.trim();
    if query.is_empty() && n.is_none() && author.is_none() {
        return Err(r#"search_discord error: need {"query": "<words>"} (optionally channel, author, n)"#.to_string());
    }
    let terms: Vec<String> = query.split('|').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect();
    let files = selected_channels(ctx, guild_id, channel).map_err(|e| format!("search_discord error: {e}"))?;
    let mut hits: Vec<(i64, String)> = Vec::new();
    let mut total = 0usize;
    for (cid, path) in &files {
        let rows = read_channel(path);
        let count = rows.len();
        for (idx, row) in rows.iter().enumerate() {
            if row.user_id == ctx.bot_user_id && author.is_none() {
                continue;
            }
            if !author_matches(row, author) {
                continue;
            }
            if let Some(want) = n {
                if idx + 1 != want {
                    continue;
                }
            }
            let lower = row.text.to_lowercase();
            if !terms.is_empty() && !terms.iter().any(|t| lower.contains(t)) {
                continue;
            }
            total += 1;
            hits.push((
                ts_secs(&row.ts),
                format!("{} n={}/{} @{} {}: {}", channel_label(ctx, *cid), idx + 1, count, row.author, &row.ts[..10.min(row.ts.len())], snippet(&row.text, 220)),
            ));
        }
    }
    if hits.is_empty() {
        return Ok(format!("search_discord: no messages matching '{query}' on this server"));
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    let shown: Vec<String> = hits.iter().take(limit).map(|(_, l)| l.clone()).collect();
    Ok(format!(
        "{total} hit(s) for '{query}' across {} channel(s), newest first (n= is the message number from the start of that channel):\n{}",
        files.len(),
        shown.join("\n")
    ))
}

pub fn random_message(ctx: &ToolCtx<'_>, channel: Option<&str>, author: Option<&str>, contains: Option<&str>, sort: &str, min_len: usize) -> Result<String, String> {
    let guild_id = ctx.guild_id.ok_or("random_message error: only works inside a server")?;
    let files = selected_channels(ctx, guild_id, channel).map_err(|e| format!("random_message error: {e}"))?;
    let needle = contains.map(|c| c.to_lowercase());
    let mut pool: Vec<(i64, u64, usize, LogRow)> = Vec::new();
    for (cid, path) in &files {
        let rows = read_channel(path);
        let reactions = if sort == "reactions" { reaction_counts(ctx.data_root, guild_id, *cid) } else { HashMap::new() };
        for (idx, row) in rows.into_iter().enumerate() {
            if row.user_id == ctx.bot_user_id || row.text.chars().count() < min_len || row.text.starts_with("http") {
                continue;
            }
            if !author_matches(&row, author) {
                continue;
            }
            if let Some(nd) = &needle {
                if !row.text.to_lowercase().contains(nd.as_str()) {
                    continue;
                }
            }
            let score = reactions.get(&row.id).copied().unwrap_or(0);
            if sort == "reactions" && score <= 0 {
                continue;
            }
            pool.push((score, *cid, idx + 1, row));
        }
    }
    if pool.is_empty() {
        return Ok("random_message: nothing matched those filters".to_string());
    }
    let picks: Vec<&(i64, u64, usize, LogRow)> = if sort == "reactions" {
        pool.sort_by(|a, b| b.0.cmp(&a.0));
        pool.iter().take(3).collect()
    } else {
        let seed = now_secs() as u64 ^ (pool.len() as u64).rotate_left(17) ^ ctx.channel_id;
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut picks = Vec::new();
        for _ in 0..2.min(pool.len()) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            picks.push(&pool[(x % pool.len() as u64) as usize]);
        }
        picks
    };
    let lines: Vec<String> = picks
        .iter()
        .map(|(score, cid, n, row)| {
            let mut l = format!("{} n={} @{} {}: {}", channel_label(ctx, *cid), n, row.author, &row.ts[..10.min(row.ts.len())], snippet(&row.text, 260));
            if *score > 0 {
                l.push_str(&format!(" [{score} reactions]"));
            }
            l
        })
        .collect();
    Ok(format!("real message(s) from the server logs:\n{}", lines.join("\n")))
}

pub fn lookup_rule(ctx: &ToolCtx<'_>, label: &str) -> Result<String, String> {
    let Some(rules_channel) = ctx.rules_channel_id else {
        return Err("lookup_rule error: no rules channel configured".to_string());
    };
    let guild_id = ctx.guild_id.or_else(|| ctx.directory.inner.read().channels.get(&rules_channel).map(|c| c.guild_id));
    let Some(guild_id) = guild_id else {
        return Err("lookup_rule error: rules channel guild unknown".to_string());
    };
    let path = ctx.data_root.join(guild_id.to_string()).join(format!("{rules_channel}.tsv"));
    let mut rows = read_channel(&path);
    rows.sort_by(|a, b| a.ts.cmp(&b.ts));
    let rules = parse_rules(&rows);
    let label = label.trim().trim_start_matches('#').trim_end_matches(['?', '.', '!']).trim();
    let low = label.to_lowercase();
    if label.is_empty() {
        return Err(r#"lookup_rule error: need {"label": "4"} (the rule number/symbol) or "count""#.to_string());
    }
    let label = match low.as_str() {
        "infinity" | "inf" | "infty" | "∞" => "∞".to_string(),
        _ => label.to_string(),
    };
    let unique: usize = rules.iter().map(|r| r.label.as_str()).collect::<HashSet<_>>().len();
    if matches!(low.as_str(), "count" | "how many" | "howmany" | "total" | "all" | "rules" | "n" | "number" | "list") || low.contains("how many") {
        return Ok(format!(
            "{} labelled rule posts, {unique} unique labels (two posts share rule 7), plus unnumbered lore. do not invent a tidy fake total.",
            rules.len()
        ));
    }
    let matches: Vec<&Rule> = rules.iter().filter(|r| r.label == label).collect();
    if matches.is_empty() {
        // Sub-rules like 14.1 live inside a parent post.
        for r in &rules {
            for line in r.body.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix(&format!("{label}.")) {
                    let rest = rest.trim();
                    if !rest.is_empty() && rest.chars().next().is_some_and(|c| c.is_whitespace() || c.is_alphabetic()) {
                        return Ok(format!("rule {label} (inside rule {}, by {}): {}", r.label, r.author, rest.trim()));
                    }
                }
            }
        }
        return Ok(format!("no posted rule labelled '{label}'. do not invent one; if it isn't on the wall it isn't a rule."));
    }
    if matches.len() == 1 {
        let r = matches[0];
        return Ok(format!("rule {}, posted by {} on {}: {}", r.label, r.author, &r.ts[..10.min(r.ts.len())], snippet(&r.body, 900)));
    }
    Ok(format!(
        "{} posts are labelled rule {label}: {}",
        matches.len(),
        matches.iter().map(|r| format!("[{}] {}", r.author, snippet(&r.body, 400))).collect::<Vec<_>>().join(" ALSO ")
    ))
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub label: String,
    pub body: String,
    pub author: String,
    pub ts: String,
}

/// Port of training/build_rules_data.py RULE_LABEL_RE: an optional "rule "
/// prefix, then a label (numbers with . , ^ * + i ( ) letters, or ∞, or the
/// 10²³+4 family, or i^i*…), then `.`/`:` and whitespace.
pub fn parse_rules(rows: &[LogRow]) -> Vec<Rule> {
    let mut out = Vec::new();
    for row in rows {
        let text = row.text.trim().trim_start_matches('`');
        if text.is_empty() || text.starts_with("the of the most of the most") || text.starts_with("Rule 59a.") {
            continue;
        }
        let lower = text.to_lowercase();
        let after_prefix = if lower.starts_with("rule ") { text[5..].trim_start() } else { text };
        let Some((label, body)) = split_label(after_prefix) else { continue };
        out.push(Rule { label, body: body.trim().trim_end_matches("```").trim().to_string(), author: row.author.clone(), ts: row.ts.clone() });
    }
    out
}

fn split_label(text: &str) -> Option<(String, String)> {
    let mut label = String::new();
    let rest: &str;
    if let Some(r) = text.strip_prefix('∞') {
        label.push('∞');
        rest = r;
    } else if text.starts_with("10²³+4") {
        let l = if text.starts_with("10²³+4.001") { "10²³+4.001" } else { "10²³+4" };
        label.push_str(l);
        rest = &text[l.len()..];
    } else if text.starts_with("i^i*") {
        let end = text[4..].find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+')).map(|e| 4 + e).unwrap_or(text.len());
        label.push_str(&text[..end]);
        rest = &text[end..];
    } else {
        let mut chars = text.char_indices().peekable();
        if let Some((_, '-')) = chars.peek() {
            label.push('-');
            chars.next();
        }
        match chars.peek() {
            Some((_, c)) if c.is_ascii_digit() => {}
            _ => return None,
        }
        let mut end = 0;
        for (i, c) in chars {
            if c.is_ascii_digit() || "., ^*+i()".contains(c) && c != ' ' || c.is_ascii_lowercase() {
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        label.push_str(&text[if label.starts_with('-') { 1 } else { 0 }..end]);
        if label.starts_with('-') {
            label = format!("-{}", &text[1..end]);
        }
        rest = &text[end..];
    }
    let label = label.trim_end_matches('.').to_string();
    let rest = rest.strip_prefix(['.', ':']).unwrap_or(rest);
    if !rest.starts_with(char::is_whitespace) && !rest.is_empty() {
        return None;
    }
    if label.is_empty() {
        return None;
    }
    Some((label, rest.to_string()))
}

pub async fn server_activity(ctx: &ToolCtx<'_>, hours: usize, channel: Option<&str>) -> Result<String, String> {
    let guild_id = ctx.guild_id.ok_or("server_activity error: only works inside a server")?;
    let files = selected_channels(ctx, guild_id, channel).map_err(|e| format!("server_activity error: {e}"))?;
    let since = now_secs() - (hours as i64) * 3600;
    let mut per_channel: Vec<(usize, u64, Vec<LogRow>)> = Vec::new();
    let mut authors: HashMap<String, usize> = HashMap::new();
    let mut total = 0;
    for (cid, path) in &files {
        let rows: Vec<LogRow> = read_channel(path).into_iter().filter(|r| ts_secs(&r.ts) >= since).collect();
        if rows.is_empty() {
            continue;
        }
        for r in &rows {
            if r.user_id != ctx.bot_user_id {
                *authors.entry(r.author.clone()).or_insert(0) += 1;
            }
        }
        total += rows.len();
        per_channel.push((rows.len(), *cid, rows));
    }
    if per_channel.is_empty() {
        return Ok(format!("server_activity: dead quiet, no messages in the last {hours}h"));
    }
    per_channel.sort_by(|a, b| b.0.cmp(&a.0));
    let mut top_authors: Vec<(String, usize)> = authors.into_iter().collect();
    top_authors.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out = format!(
        "last {hours}h: {total} messages in {} channel(s). most active people: {}.\n",
        per_channel.len(),
        top_authors.iter().take(6).map(|(a, n)| format!("{a} ({n})")).collect::<Vec<_>>().join(", ")
    );
    for (n, cid, rows) in per_channel.iter().take(6) {
        out.push_str(&format!("{} ({n} msgs), latest:\n", channel_label(ctx, *cid)));
        let tail = rows.len().saturating_sub(4);
        for r in &rows[tail..] {
            out.push_str(&format!("  {} {}: {}\n", &r.ts[11..16.min(r.ts.len())], r.author, snippet(&r.text, 120)));
        }
    }
    Ok(out.trim_end().to_string())
}

pub async fn who_is(ctx: &ToolCtx<'_>, name: &str) -> Result<String, String> {
    let guild_id = ctx.guild_id.ok_or("who_is error: only works inside a server")?;
    let name = name.trim().trim_start_matches('@');
    if name.is_empty() {
        return Err("who_is error: who?".to_string());
    }
    let needle = name.to_lowercase();
    let mut count = 0usize;
    let mut first: Option<String> = None;
    let mut last: Option<String> = None;
    let mut per_channel: HashMap<u64, usize> = HashMap::new();
    let mut recent: Vec<(i64, String)> = Vec::new();
    let mut canonical: Option<String> = None;
    let mut user_id: Option<u64> = None;
    for (cid, path) in guild_channel_files(ctx.data_root, guild_id) {
        for row in read_channel(&path) {
            let a = row.author.to_lowercase();
            if a != needle && !a.contains(&needle) {
                continue;
            }
            if canonical.is_none() || a == needle {
                canonical = Some(row.author.clone());
                user_id = Some(row.user_id);
            }
            count += 1;
            *per_channel.entry(cid).or_insert(0) += 1;
            if first.as_ref().map_or(true, |f| row.ts < *f) {
                first = Some(row.ts.clone());
            }
            if last.as_ref().map_or(true, |l| row.ts > *l) {
                last = Some(row.ts.clone());
            }
            if row.text.chars().count() >= 15 {
                recent.push((ts_secs(&row.ts), snippet(&row.text, 140)));
            }
        }
    }
    if count == 0 {
        return Ok(format!("who_is: nobody called '{name}' in the logs"));
    }
    let mut chans: Vec<(u64, usize)> = per_channel.into_iter().collect();
    chans.sort_by(|a, b| b.1.cmp(&a.1));
    recent.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = format!(
        "{}: {count} logged messages, first seen {}, last seen {}. hangs out in {}.",
        canonical.clone().unwrap_or_else(|| name.to_string()),
        first.as_deref().map(|s| &s[..10.min(s.len())]).unwrap_or("?"),
        last.as_deref().map(|s| &s[..10.min(s.len())]).unwrap_or("?"),
        chans.iter().take(3).map(|(c, n)| format!("{} ({n})", channel_label(ctx, *c))).collect::<Vec<_>>().join(", ")
    );
    if let Some(uid) = user_id {
        if let Ok(resp) = ctx.http.guild_member(Id::<GuildMarker>::new(guild_id), Id::new(uid)).await {
            if let Ok(m) = resp.model().await {
                let joined = m.joined_at.iso_8601().to_string();
                if joined.len() >= 10 {
                    out.push_str(&format!(" joined the server {}.", &joined[..10]));
                }
                if let Some(nick) = m.nick {
                    out.push_str(&format!(" nick: {nick}."));
                }
            }
        }
    }
    let sample: Vec<String> = recent.iter().take(4).map(|(_, t)| format!("  \"{t}\"")).collect();
    if !sample.is_empty() {
        out.push_str("\nrecent things they said:\n");
        out.push_str(&sample.join("\n"));
    }
    Ok(out)
}

/// Upcoming scheduled events (the studio's event nights are the heartbeat of
/// this server).
pub async fn events(ctx: &ToolCtx<'_>) -> Result<String, String> {
    let Some(guild_id) = ctx.guild_id else {
        return Err("events error: only works inside a server".to_string());
    };
    let resp = ctx
        .http
        .guild_scheduled_events(Id::<GuildMarker>::new(guild_id))
        .await
        .map_err(|e| format!("events error: {e}"))?;
    let mut list = resp.models().await.map_err(|e| format!("events error: {e}"))?;
    let now = chrono::Utc::now().timestamp();
    list.retain(|e| e.scheduled_start_time.as_secs() >= now - 3 * 3600);
    list.sort_by_key(|e| e.scheduled_start_time.as_secs());
    if list.is_empty() {
        return Ok("events: nothing scheduled right now. walnutty has been slacking".to_string());
    }
    let lines: Vec<String> = list
        .iter()
        .take(6)
        .map(|e| {
            let start = chrono::DateTime::<chrono::Utc>::from_timestamp(e.scheduled_start_time.as_secs(), 0)
                .map(|t| t.with_timezone(&chrono::Local))
                .map(|t| t.format("%a %Y-%m-%d %H:%M %Z").to_string())
                .unwrap_or_else(|| "?".to_string());
            let delta = e.scheduled_start_time.as_secs() - now;
            let when = if delta < 0 {
                "happening now".to_string()
            } else if delta < 3600 {
                format!("in {} min", delta / 60)
            } else if delta < 48 * 3600 {
                format!("in {}h", delta / 3600)
            } else {
                format!("in {} days", delta / 86400)
            };
            let place = e
                .entity_metadata
                .as_ref()
                .and_then(|m| m.location.clone())
                .or_else(|| e.channel_id.and_then(|c| ctx.directory.channel_name(c.get()).map(|n| format!("#{n}"))))
                .unwrap_or_else(|| "somewhere on the server".to_string());
            let mut line = format!("- {} ({when}) {}: {place}", start, e.name);
            if let Some(n) = e.user_count {
                line.push_str(&format!(", {n} interested"));
            }
            if let Some(d) = &e.description {
                let d = d.split_whitespace().collect::<Vec<_>>().join(" ");
                if !d.is_empty() {
                    line.push_str(&format!(" ({})", d.chars().take(140).collect::<String>()));
                }
            }
            line
        })
        .collect();
    Ok(format!("upcoming events:\n{}", lines.join("\n")))
}

#[allow(dead_code)]
pub async fn server_status(ctx: &ToolCtx<'_>) -> Result<String, String> {
    let Some(guild_id) = ctx.guild_id else {
        return Ok(format!("this is a DM with {}", ctx.user_name));
    };
    if ctx.directory.needs_refresh() {
        ctx.directory.refresh(ctx.http).await;
    }
    let gname = ctx.directory.guild_name(guild_id).unwrap_or_else(|| "this server".to_string());
    let (members, online) = ctx.directory.member_counts(guild_id).unwrap_or((0, None));
    let voice = ctx.directory.voice_lines(guild_id);
    let mut out = format!("{gname}: {members} members", );
    if let Some(o) = online {
        out.push_str(&format!(", ~{o} online"));
    }
    out.push_str(&format!(". you are in {}.", channel_label(ctx, ctx.channel_id)));
    if voice.is_empty() {
        out.push_str(" nobody is in voice right now.");
    } else {
        out.push_str(&format!(" in voice: {}.", voice.join("; ")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_norm() {
        assert_eq!(norm_channel("🎃general"), "general");
        assert_eq!(norm_channel("#Bot-Spam"), "bot-spam");
    }

    #[test]
    fn row_parsing() {
        let r = parse_row("1\t2026-01-01T00:00:00Z\t42\tzuna\\tbaro\t\thello\\nworld").unwrap();
        assert_eq!(r.author, "zuna\tbaro");
        assert_eq!(r.text, "hello\nworld");
        assert!(parse_row("garbage").is_none());
    }

    #[test]
    fn rule_labels() {
        let rows: Vec<LogRow> = [
            ("1", "4. ABSOLUTELY NO AI CONTENT"),
            ("2", "rule ∞. I rule all."),
            ("3", "rule -1. not noticing new rules"),
            ("4", "rule 16.1 uncloudy is a furry"),
            ("5", "rule 3.14159. Only follow rules whose number is a PI value"),
            ("6", "```rule 55. the speed of light is 8e```"),
            ("7", "this is full of contradictions, wow"),
            ("8", "rule 10²³+4.001. NEVER listen"),
            ("9", "rule 35i. Follow the inverse"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (_, t))| LogRow { id: i as u64, ts: format!("2026-01-0{}T00:00:00Z", i + 1), user_id: 1, author: "a".into(), text: t.to_string() })
        .collect();
        let rules = parse_rules(&rows);
        let labels: Vec<&str> = rules.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["4", "∞", "-1", "16.1", "3.14159", "55", "10²³+4.001", "35i"]);
        assert_eq!(rules[0].body, "ABSOLUTELY NO AI CONTENT");
        assert_eq!(rules[5].body, "the speed of light is 8e");
    }
}
