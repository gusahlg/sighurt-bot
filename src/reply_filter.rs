//! Two-step word filter for the bot's OWN outgoing chat replies.
//!
//! Step 1 is a cheap lexical screen against a deny-list of racist, harassing
//! and disturbing terms. A reply with no hits passes immediately — the common
//! case costs one string scan. The deny-list has two tiers:
//!
//! * HARD terms (unambiguous slurs, self-harm directives, hate slogans) have
//!   no innocent use worth arbitrating — a hit rejects the reply outright.
//! * JUDGED terms ("chink", "rape", "coon"...) have innocent embeddings and
//!   idioms, so step 2 hands the flagged text to a LOCAL judge model over
//!   the OpenAI-compatible chat-completions API (ollama, llama.cpp server,
//!   LM Studio, vLLM — anything that speaks that wire format). The shipped
//!   judge is llama-guard3:1b, a purpose-built safety classifier answering
//!   safe/unsafe; plain instruct models answering yes/no also work
//!   (`filter.judge_kind`). Tested against llama3.2:1b as an instruct judge:
//!   its verdicts were near-random — prefer a guard-class model.
//!
//! Fail-closed by design: no judge configured, judge unreachable, or an
//! ambiguous answer all REJECT the flagged reply. The deny-list only ever
//! sees the bot's own model output, not adversarial humans, so it aims for
//! severity, not evasion-proofing — profanity alone is deliberately absent
//! (the persona swears; the guidelines are about racism, harassment and
//! disturbing content, not crude language).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::FilterConfig;

/// HARD tier, matched anywhere in the text, even inside longer words.
/// Reserved for terms with no innocent embedding (no Scunthorpe risk).
const HARD_SUBSTRING_TERMS: &[&str] = &[
    "nigger", "nigga", "faggot", "wetback", "svartskalle",
];

/// HARD tier, whole-word matches. Slang self-harm directives and numeric
/// hate symbols: unambiguous as standalone tokens, unmatchable inside words.
const HARD_TOKEN_TERMS: &[&str] = &["kys", "1488"];

/// HARD tier, multi-word phrases on whitespace-collapsed folded text.
/// Self-harm directives and hate slogans; the 1B guard model provably misses
/// some of these (slang, Swedish), and their innocent uses are too rare to
/// be worth the risk of arbitration.
const HARD_PHRASES: &[&str] = &[
    "kill yourself", "kill urself", "hang yourself", "neck yourself",
    "slit your wrists", "drink bleach", "go die",
    "ta livet av dig", "häng dig",
    "white power", "heil hitler", "sieg heil", "child porn",
];

/// JUDGED tier, whole-word matches — these have innocent look-alikes or
/// embeddings ("spice", "raccoon", "chink in the armour"), so a hit is
/// arbitrated by the AI judge instead of rejecting outright.
const JUDGED_TOKEN_TERMS: &[&str] = &[
    // Racist slurs and hate symbols.
    "kike", "spic", "chink", "gook", "coon", "beaner", "raghead", "towelhead",
    "tranny", "neger", "blatte", "zipperhead", "kkk",
    // Harassment.
    "retard", "retards", "retarded", "mongo",
    // Disturbing content.
    "rape", "raped", "rapes", "raping", "rapist",
    "våldta", "våldtas", "våldtog", "våldtäkt",
    "pedo", "pedos", "pedophile", "pedofil",
    "molest", "molested", "molester",
    "bestiality", "beastiality", "necrophilia",
];

/// JUDGED tier, multi-word phrases — plausible in news/history talk.
const JUDGED_PHRASES: &[&str] = &[
    "gas the", "school shooting", "shoot up the school",
];

/// The judge sees at most this many characters (replies are capped shorter
/// than this anyway; belt-and-suspenders against a runaway input).
const JUDGE_INPUT_MAX_CHARS: usize = 2_000;

/// System prompt for `judge_kind = "instruct"` judges. Guard-class models
/// bring their own built-in harness and get the bare message instead.
const JUDGE_INSTRUCTION: &str = "You are the content filter for a Discord server. \
You will be shown one chat message. The guidelines prohibit racist content, \
harassment or threats aimed at a person, and gratuitously disturbing content. \
Profanity and crude humor on their own are allowed. Answer with exactly one \
word: yes if the message breaks the guidelines, no if it does not.";

/// Outcome of screening one reply.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplyScreen {
    Pass,
    Rejected {
        /// Deny-list terms that flagged the text (for the log line only —
        /// never shown to users).
        matched: Vec<String>,
        /// Why the flagged reply was rejected rather than cleared.
        reason: &'static str,
    },
}

/// Step-1 output: which tier each hit belongs to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LexicalMatches {
    /// Reject outright; the judge is not consulted.
    pub hard: Vec<String>,
    /// Arbitrated by the AI judge (rejected if there is none).
    pub judged: Vec<String>,
}

pub struct ReplyFilter {
    enabled: AtomicBool,
    hard_substrings: Vec<String>,
    hard_tokens: HashSet<String>,
    hard_phrases: Vec<String>,
    judged_tokens: HashSet<String>,
    judged_phrases: Vec<String>,
    judge: Option<JudgeClient>,
}

impl ReplyFilter {
    pub fn from_config(cfg: &FilterConfig) -> Result<Self> {
        let mut judged_tokens: HashSet<String> =
            JUDGED_TOKEN_TERMS.iter().map(|t| t.to_string()).collect();
        let mut judged_phrases: Vec<String> =
            JUDGED_PHRASES.iter().map(|t| t.to_string()).collect();

        // Optional admin-extendable deny-list: one lowercase term per line,
        // `#` comments. File terms land in the JUDGED tier (the judge can
        // still rescue idioms; hard terms stay curated in code). Terms with
        // whitespace become phrases, single words token terms. Missing file
        // is a loud warning, not a fatal — a typo'd path must not keep the
        // bot down.
        if let Some(path) = cfg.words_file.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    let mut added = 0usize;
                    for line in content.lines() {
                        let term = line.trim().to_lowercase();
                        if term.is_empty() || term.starts_with('#') {
                            continue;
                        }
                        if term.split_whitespace().count() > 1 {
                            judged_phrases
                                .push(term.split_whitespace().collect::<Vec<_>>().join(" "));
                        } else {
                            judged_tokens.insert(term);
                        }
                        added += 1;
                    }
                    tracing::info!("Loaded {} extra filter terms from {}", added, path);
                }
                Err(e) => {
                    tracing::warn!(
                        "filter.words_file {} unreadable ({}); using built-in terms only",
                        path,
                        e
                    );
                }
            }
        }
        judged_phrases.sort();
        judged_phrases.dedup();

        let judge = match cfg.judge_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(url) => Some(JudgeClient::new(
                url,
                cfg.judge_model.as_deref().unwrap_or_default().trim(),
                cfg.judge_kind()?,
                cfg.judge_timeout_secs,
            )?),
            None => None,
        };

        Ok(Self {
            enabled: AtomicBool::new(cfg.enabled),
            hard_substrings: HARD_SUBSTRING_TERMS.iter().map(|t| t.to_string()).collect(),
            hard_tokens: HARD_TOKEN_TERMS.iter().map(|t| t.to_string()).collect(),
            hard_phrases: HARD_PHRASES.iter().map(|t| t.to_string()).collect(),
            judged_tokens,
            judged_phrases,
            judge,
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Returns the previous value.
    pub fn set_enabled(&self, value: bool) -> bool {
        self.enabled.swap(value, Ordering::Relaxed)
    }

    /// Human-readable judge state for `!filter status` and the boot log.
    pub fn judge_description(&self) -> String {
        match &self.judge {
            Some(judge) => format!("{} via {}", judge.model, judge.url),
            None => "none (flagged replies are rejected outright)".to_string(),
        }
    }

    pub fn boot_summary(&self) -> String {
        format!(
            "Reply filter: {} ({} hard terms, {} judged terms; judge: {})",
            if self.is_enabled() { "ON" } else { "OFF" },
            self.hard_substrings.len() + self.hard_tokens.len() + self.hard_phrases.len(),
            self.judged_tokens.len() + self.judged_phrases.len(),
            self.judge_description()
        )
    }

    /// Step 1: deny-list hits in `text`, split by tier, deduped. Runs over
    /// two foldings — plain lowercase and leetspeak-normalized — so `n1gger`
    /// and `1488` are both catchable even though normalization would mangle
    /// the digits of the latter.
    pub fn lexical_matches(&self, text: &str) -> LexicalMatches {
        let mut matches = LexicalMatches::default();
        for folded in [fold(text, false), fold(text, true)] {
            for term in &self.hard_substrings {
                if folded.contains(term.as_str()) {
                    matches.hard.push(term.clone());
                }
            }
            for phrase in &self.hard_phrases {
                if folded.contains(phrase.as_str()) {
                    matches.hard.push(phrase.clone());
                }
            }
            for phrase in &self.judged_phrases {
                if folded.contains(phrase.as_str()) {
                    matches.judged.push(phrase.clone());
                }
            }
            for token in folded.split(' ') {
                if self.hard_tokens.contains(token) {
                    matches.hard.push(token.to_string());
                } else if self.judged_tokens.contains(token) {
                    matches.judged.push(token.to_string());
                }
            }
        }
        matches.hard.sort();
        matches.hard.dedup();
        matches.judged.sort();
        matches.judged.dedup();
        matches
    }

    /// The full two-step screen. Lexically clean text passes without any
    /// model traffic; hard-tier hits reject outright; judged-tier hits are
    /// cleared only by an explicit "safe"/"no" from the judge — everything
    /// else (no judge, judge down, ambiguous answer) rejects.
    pub async fn screen(&self, text: &str) -> ReplyScreen {
        if !self.is_enabled() {
            return ReplyScreen::Pass;
        }
        let matches = self.lexical_matches(text);
        if !matches.hard.is_empty() {
            return ReplyScreen::Rejected {
                matched: matches.hard,
                reason: "hard deny-list term",
            };
        }
        if matches.judged.is_empty() {
            return ReplyScreen::Pass;
        }
        let matched = matches.judged;
        let Some(judge) = &self.judge else {
            return ReplyScreen::Rejected {
                matched,
                reason: "no AI judge configured",
            };
        };
        let input: String = text.chars().take(JUDGE_INPUT_MAX_CHARS).collect();
        match judge.judge(&input).await {
            Ok(false) => ReplyScreen::Pass,
            Ok(true) => ReplyScreen::Rejected {
                matched,
                reason: "AI judge: guideline violation",
            },
            Err(e) => {
                tracing::warn!("AI judge unavailable ({:#}); rejecting flagged reply", e);
                ReplyScreen::Rejected {
                    matched,
                    reason: "AI judge unavailable",
                }
            }
        }
    }
}

/// Lowercase and strip punctuation to spaces; with `leet` also map the
/// common digit/symbol substitutions to letters. Whitespace is collapsed so
/// phrase matching is layout-independent.
fn fold(text: &str, leet: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.to_lowercase().chars() {
        let mapped = match c {
            '0' if leet => 'o',
            '1' if leet => 'i',
            '3' if leet => 'e',
            '4' if leet => 'a',
            '5' if leet => 's',
            '7' if leet => 't',
            '$' if leet => 's',
            '@' if leet => 'a',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        };
        out.push(mapped);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First standalone verdict token in the judge's answer. Guard classifiers
/// say safe/unsafe (llama-guard appends a category, e.g. "unsafe\nS10");
/// instruct judges say yes/no. `None` when the answer contains neither
/// (which the caller treats as a rejection).
fn judge_answer(content: &str) -> Option<bool> {
    for word in content.split(|c: char| !c.is_ascii_alphabetic()) {
        match word.to_ascii_lowercase().as_str() {
            "yes" | "unsafe" => return Some(true),
            "no" | "safe" => return Some(false),
            _ => {}
        }
    }
    None
}

/// How to talk to the judge model. Guard-class classifiers (llama-guard)
/// carry their own harness in the model template and get the bare message;
/// generic instruct models need the yes/no system instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeKind {
    Guard,
    Instruct,
}

struct JudgeClient {
    url: String,
    model: String,
    kind: JudgeKind,
    http: reqwest::Client,
}

impl JudgeClient {
    fn new(url: &str, model: &str, kind: JudgeKind, timeout_secs: u64) -> Result<Self> {
        if model.is_empty() {
            bail!("filter.judge_model is required when filter.judge_url is set");
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .build()
            .context("build judge HTTP client")?;
        Ok(Self {
            url: url.to_string(),
            model: model.to_string(),
            kind,
            http,
        })
    }

    /// Ask the local model whether `text` violates the guidelines.
    /// `Ok(true)` = violation, `Ok(false)` = clean, `Err` = judge unusable.
    async fn judge(&self, text: &str) -> Result<bool> {
        let messages = match self.kind {
            JudgeKind::Guard => json!([{"role": "user", "content": text}]),
            JudgeKind::Instruct => json!([
                {"role": "system", "content": JUDGE_INSTRUCTION},
                {"role": "user", "content": text},
            ]),
        };
        let body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0,
            "max_tokens": 10,
        });
        // reqwest is built without the `json` feature; serialize by hand.
        let resp = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .context("send judge request")?;
        let status = resp.status();
        let payload = resp.text().await.context("read judge response")?;
        if !status.is_success() {
            bail!(
                "judge returned {}: {}",
                status,
                payload.chars().take(300).collect::<String>()
            );
        }
        let value: serde_json::Value =
            serde_json::from_str(&payload).context("parse judge response JSON")?;
        let content = value["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow!("judge response missing choices[0].message.content"))?;
        tracing::debug!("AI judge answered {:?}", content);
        judge_answer(content)
            .ok_or_else(|| anyhow!("judge answered ambiguously: {:?}", content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(enabled: bool) -> ReplyFilter {
        let cfg = FilterConfig {
            enabled,
            ..FilterConfig::default()
        };
        ReplyFilter::from_config(&cfg).unwrap()
    }

    #[test]
    fn fold_lowercases_strips_and_optionally_deleets() {
        assert_eq!(fold("Hej,  DÅ!", false), "hej då");
        assert_eq!(fold("n1gg3r", false), "n1gg3r");
        assert_eq!(fold("n1gg3r", true), "nigger");
        // Plain fold keeps digits so numeric hate symbols stay matchable.
        assert_eq!(fold("14 88", false), "14 88");
    }

    #[test]
    fn clean_text_has_no_flags() {
        let f = filter(true);
        assert_eq!(f.lexical_matches("what a nice keyboard, truly cursed"), LexicalMatches::default());
        // Persona profanity is deliberately not a trigger.
        assert_eq!(f.lexical_matches("that is fucking wild lmao"), LexicalMatches::default());
    }

    #[test]
    fn hard_terms_flag_even_embedded_and_leetspeak() {
        let f = filter(true);
        assert_eq!(f.lexical_matches("you absolute n1ggers").hard, vec!["nigger"]);
        assert_eq!(f.lexical_matches("just KILL   yourself already").hard, vec!["kill yourself"]);
        assert_eq!(f.lexical_matches("kys lol").hard, vec!["kys"]);
        assert_eq!(f.lexical_matches("ta livet av dig.").hard, vec!["ta livet av dig"]);
        // Hard tokens only match standalone ("1488" yes, "14880" no).
        assert_eq!(f.lexical_matches("total 1488 vibes").hard, vec!["1488"]);
        assert!(f.lexical_matches("item 14880 in stock").hard.is_empty());
    }

    #[test]
    fn judged_terms_do_not_flag_innocent_embeddings() {
        let f = filter(true);
        assert_eq!(f.lexical_matches("add some spice to the racoon stew"), LexicalMatches::default());
        assert_eq!(f.lexical_matches("the grape harvest therapist"), LexicalMatches::default());
        let m = f.lexical_matches("shut up you spic");
        assert!(m.hard.is_empty());
        assert_eq!(m.judged, vec!["spic"]);
        // Judged phrases match across whitespace and case.
        assert_eq!(f.lexical_matches("a SCHOOL   shooting documentary").judged, vec!["school shooting"]);
    }

    #[test]
    fn judge_answer_parses_verdict_tokens() {
        assert_eq!(judge_answer("Yes."), Some(true));
        assert_eq!(judge_answer("no, that is fine"), Some(false));
        assert_eq!(judge_answer("The answer is: NO"), Some(false));
        // llama-guard style verdicts, with and without category.
        assert_eq!(judge_answer("safe"), Some(false));
        assert_eq!(judge_answer("unsafe\nS10"), Some(true));
        // "eyes" must not read as yes; no verdict at all is None.
        assert_eq!(judge_answer("eyes maybe"), None);
        assert_eq!(judge_answer(""), None);
    }

    #[tokio::test]
    async fn screen_passes_clean_and_disabled_text() {
        let f = filter(true);
        assert_eq!(f.screen("hello there").await, ReplyScreen::Pass);
        let off = filter(false);
        assert_eq!(off.screen("kill yourself").await, ReplyScreen::Pass);
    }

    #[tokio::test]
    async fn screen_rejects_hard_terms_without_consulting_judge() {
        // No judge is configured here, but the reason must be the hard tier,
        // not the missing judge — hard hits never reach the judge at all.
        let f = filter(true);
        match f.screen("go drink bleach").await {
            ReplyScreen::Rejected { matched, reason } => {
                assert_eq!(matched, vec!["drink bleach"]);
                assert_eq!(reason, "hard deny-list term");
            }
            ReplyScreen::Pass => panic!("hard-tier text must not pass"),
        }
    }

    #[tokio::test]
    async fn screen_rejects_judged_terms_without_a_judge() {
        let f = filter(true);
        match f.screen("what a coon").await {
            ReplyScreen::Rejected { matched, reason } => {
                assert_eq!(matched, vec!["coon"]);
                assert_eq!(reason, "no AI judge configured");
            }
            ReplyScreen::Pass => panic!("judged-tier text must not pass without a judge"),
        }
    }

    #[test]
    fn words_file_extends_judged_tier() {
        let mut path = std::env::temp_dir();
        path.push(format!("filter_words_test_{}.txt", std::process::id()));
        std::fs::write(&path, "# comment\nzorbleblat\nfrobnicate the cat\n\n").unwrap();
        let cfg = FilterConfig {
            words_file: Some(path.to_string_lossy().into_owned()),
            ..FilterConfig::default()
        };
        let f = ReplyFilter::from_config(&cfg).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(f.lexical_matches("total zorbleblat energy").judged, vec!["zorbleblat"]);
        assert_eq!(
            f.lexical_matches("please frobnicate  the CAT").judged,
            vec!["frobnicate the cat"]
        );
        // A term embedded in a longer word stays token-tier (no flag).
        assert_eq!(f.lexical_matches("zorbleblattery"), LexicalMatches::default());
    }

    #[test]
    fn missing_words_file_is_nonfatal() {
        let cfg = FilterConfig {
            words_file: Some("/definitely/not/a/real/path.txt".to_string()),
            ..FilterConfig::default()
        };
        assert!(ReplyFilter::from_config(&cfg).is_ok());
    }

    #[test]
    fn judge_requires_model_name() {
        let cfg = FilterConfig {
            judge_url: Some("http://127.0.0.1:11434/v1/chat/completions".to_string()),
            judge_model: None,
            ..FilterConfig::default()
        };
        assert!(ReplyFilter::from_config(&cfg).is_err());
    }
}
