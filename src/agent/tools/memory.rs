//! Persistent memory: notes, diary and reminders (append-only JSONL files).

use super::ToolCtx;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Entry {
    ts: String,
    text: String,
}

fn append(dir: &Path, file: &str, kind: &str, text: &str) -> Result<String, String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err(format!("{kind} error: empty text"));
    }
    let text: String = text.chars().take(600).collect();
    std::fs::create_dir_all(dir).map_err(|e| format!("{kind} error: {e}"))?;
    let entry = Entry { ts: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string(), text: text.clone() };
    let line = serde_json::to_string(&entry).map_err(|e| format!("{kind} error: {e}"))?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(file))
        .map_err(|e| format!("{kind} error: {e}"))?;
    writeln!(f, "{line}").map_err(|e| format!("{kind} error: {e}"))?;
    let preview: String = text.chars().take(120).collect();
    Ok(format!("saved to {kind} ({}): {preview}", entry.ts))
}

fn read(dir: &Path, file: &str, limit: usize, query: Option<&str>) -> Vec<Entry> {
    let Ok(raw) = std::fs::read_to_string(dir.join(file)) else { return Vec::new() };
    let q = query.map(str::to_lowercase);
    let entries: Vec<Entry> = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .filter(|e| q.as_ref().map_or(true, |q| e.text.to_lowercase().contains(q)))
        .collect();
    let skip = entries.len().saturating_sub(limit);
    entries.into_iter().skip(skip).collect()
}

pub fn write_note(dir: &Path, text: &str) -> Result<String, String> {
    append(dir, "notes.jsonl", "notes", text)
}

pub fn write_diary(dir: &Path, text: &str) -> Result<String, String> {
    append(dir, "diary.jsonl", "diary", text)
}

pub fn read_notes(dir: &Path, query: Option<&str>, limit: usize) -> Result<String, String> {
    let entries = read(dir, "notes.jsonl", limit, query);
    if entries.is_empty() {
        return Ok("no notes saved yet.".to_string());
    }
    Ok(format!(
        "Notes:\n{}",
        entries.iter().map(|e| format!("- [{}] {}", &e.ts[..10.min(e.ts.len())], e.text)).collect::<Vec<_>>().join("\n")
    ))
}

pub fn read_diary(dir: &Path, query: Option<&str>, limit: usize) -> Result<String, String> {
    let entries = read(dir, "diary.jsonl", limit, query);
    if entries.is_empty() {
        return Ok("diary is empty (no past entries yet).".to_string());
    }
    Ok(format!(
        "Past diary entries:\n{}",
        entries.iter().map(|e| format!("- [{}] {}", &e.ts[..10.min(e.ts.len())], e.text)).collect::<Vec<_>>().join("\n")
    ))
}

// ---------------------------------------------------------------------------
// reminders
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Reminder {
    pub id: u64,
    pub channel_id: u64,
    pub user_id: u64,
    pub fire_at: i64,
    pub text: String,
    pub created_at: i64,
}

/// In-memory list mirrored to a JSONL file (rewritten on every change; the
/// list is tiny).
pub struct ReminderStore {
    path: PathBuf,
    entries: Mutex<Vec<Reminder>>,
}

impl ReminderStore {
    pub fn load(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
            .unwrap_or_default();
        Self { path, entries: Mutex::new(entries) }
    }

    fn persist(&self, entries: &[Reminder]) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = entries.iter().filter_map(|r| serde_json::to_string(r).ok()).collect::<Vec<_>>().join("\n");
        let tmp = self.path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, if body.is_empty() { String::new() } else { body + "\n" }).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    pub fn add(&self, channel_id: u64, user_id: u64, fire_at: i64, text: &str) -> Result<Reminder, String> {
        let mut entries = self.entries.lock();
        if entries.iter().filter(|r| r.user_id == user_id).count() >= 20 {
            return Err("you already have 20 pending reminders, chill".to_string());
        }
        let now = chrono::Utc::now().timestamp();
        let id = entries.iter().map(|r| r.id).max().unwrap_or(0) + 1;
        let r = Reminder { id, channel_id, user_id, fire_at, text: text.to_string(), created_at: now };
        entries.push(r.clone());
        self.persist(&entries);
        Ok(r)
    }

    /// Remove and return every reminder whose time has come.
    pub fn take_due(&self, now: i64) -> Vec<Reminder> {
        let mut entries = self.entries.lock();
        let (due, keep): (Vec<_>, Vec<_>) = entries.drain(..).partition(|r| r.fire_at <= now);
        *entries = keep;
        if !due.is_empty() {
            self.persist(&entries);
        }
        due
    }

    pub fn pending_for(&self, user_id: u64) -> Vec<Reminder> {
        self.entries.lock().iter().filter(|r| r.user_id == user_id).cloned().collect()
    }
}

/// Parse "in 10 minutes", "in 2h", "in 1 day 2 hours", "at 18:30",
/// "tomorrow 09:00", "18:30" into an absolute unix timestamp (local time).
pub fn parse_when(when: &str, now: chrono::DateTime<chrono::Local>) -> Result<chrono::DateTime<chrono::Local>, String> {
    use chrono::{Duration, NaiveTime, TimeZone};
    let w = when.trim().to_lowercase();
    if w.is_empty() {
        return Err("when is empty".to_string());
    }
    // Relative: "in N unit" (repeatable) or bare "10m"/"2h"/"1d".
    let rel = w.strip_prefix("in ").unwrap_or(&w);
    let mut total = Duration::zero();
    let mut matched = false;
    let tokens: Vec<&str> = rel.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        let (num_str, unit_str) = match tok.find(|c: char| c.is_alphabetic()) {
            Some(pos) if pos > 0 => (&tok[..pos], tok[pos..].to_string()),
            _ => {
                let unit = tokens.get(i + 1).map(|s| s.to_string()).unwrap_or_default();
                i += 1;
                (tok, unit)
            }
        };
        let Ok(n) = num_str.parse::<f64>() else { break };
        let unit = unit_str.trim_end_matches(',');
        let secs = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
            "d" | "day" | "days" => 86400.0,
            "w" | "wk" | "week" | "weeks" => 604800.0,
            _ => break,
        };
        total = total + Duration::seconds((n * secs) as i64);
        matched = true;
        i += 1;
    }
    if matched {
        if total < Duration::seconds(30) || total > Duration::days(60) {
            return Err("reminders must be between 30 seconds and 60 days away".to_string());
        }
        return Ok(now + total);
    }
    // Absolute: [tomorrow] [at] HH:MM
    let mut tomorrow = false;
    let mut rest = w.as_str();
    if let Some(r) = rest.strip_prefix("tomorrow") {
        tomorrow = true;
        rest = r.trim();
    }
    rest = rest.strip_prefix("at ").unwrap_or(rest).trim();
    let time = NaiveTime::parse_from_str(rest, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(rest, "%H.%M"))
        .or_else(|_| NaiveTime::parse_from_str(rest, "%H"))
        .map_err(|_| format!("could not understand '{when}' (try 'in 10 minutes' or 'at 18:30')"))?;
    let mut date = now.date_naive();
    if tomorrow {
        date = date.succ_opt().ok_or("date overflow")?;
    }
    let mut candidate = chrono::Local
        .from_local_datetime(&date.and_time(time))
        .single()
        .ok_or("ambiguous local time")?;
    if !tomorrow && candidate <= now {
        candidate = candidate + Duration::days(1);
    }
    Ok(candidate)
}

pub fn remind(ctx: &ToolCtx<'_>, when: &str, text: &str) -> Result<String, String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err("remind error: what should i remind you about?".to_string());
    }
    let now = chrono::Local::now();
    let at = parse_when(when, now).map_err(|e| format!("remind error: {e}"))?;
    let r = ctx
        .reminders
        .add(ctx.channel_id, ctx.user_id, at.timestamp(), &text.chars().take(300).collect::<String>())
        .map_err(|e| format!("remind error: {e}"))?;
    Ok(format!(
        "reminder #{} set for {} ({}): {}",
        r.id,
        at.format("%a %H:%M"),
        humanize(at.signed_duration_since(now)),
        r.text
    ))
}

fn humanize(d: chrono::Duration) -> String {
    let m = d.num_minutes();
    if m < 60 {
        format!("in {m} min")
    } else if m < 60 * 36 {
        format!("in {}h {}m", m / 60, m % 60)
    } else {
        format!("in {} days", d.num_days())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t() -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 9, 18, 10, 0, 0).single().unwrap()
    }

    #[test]
    fn relative_and_absolute() {
        let now = t();
        assert_eq!(parse_when("in 10 minutes", now).unwrap(), now + chrono::Duration::minutes(10));
        assert_eq!(parse_when("2h", now).unwrap(), now + chrono::Duration::hours(2));
        assert_eq!(parse_when("in 1 day 2 hours", now).unwrap(), now + chrono::Duration::hours(26));
        assert_eq!(parse_when("at 18:30", now).unwrap().format("%d %H:%M").to_string(), "18 18:30");
        assert_eq!(parse_when("09:00", now).unwrap().format("%d %H:%M").to_string(), "19 09:00");
        assert_eq!(parse_when("tomorrow 09:00", now).unwrap().format("%d %H:%M").to_string(), "19 09:00");
        assert!(parse_when("in 5 seconds", now).is_err());
        assert!(parse_when("whenever", now).is_err());
    }

    #[test]
    fn store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sig-rem-{}", std::process::id()));
        let store = ReminderStore::load(dir.join("reminders.jsonl"));
        let r = store.add(1, 2, 100, "x").unwrap();
        assert_eq!(r.id, 1);
        assert!(store.take_due(50).is_empty());
        assert_eq!(store.take_due(100).len(), 1);
        let reloaded = ReminderStore::load(dir.join("reminders.jsonl"));
        assert!(reloaded.pending_for(2).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notes_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sig-notes-{}", std::process::id()));
        assert!(write_note(&dir, "  remember   tabs ").unwrap().contains("remember tabs"));
        assert!(read_notes(&dir, Some("tabs"), 5).unwrap().contains("remember tabs"));
        assert!(read_notes(&dir, Some("zzz"), 5).unwrap().contains("no notes"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
