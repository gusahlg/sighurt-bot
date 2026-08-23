//! Explicit, bounded live-web retrieval for SuperSighurt chat.
//!
//! Search is never inferred from an arbitrary URL and never fetches result
//! pages, which keeps this path out of SSRF territory.  A configured Brave API
//! key provides broad web search; without one, DuckDuckGo Instant Answers plus
//! Wikimedia's public search API provide a zero-secret fallback.  Returned
//! snippets are untrusted evidence and are labelled that way in the LLM prompt.

use crate::config::ChatConfig;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

const MAX_QUERY_CHARS: usize = 240;
const MAX_TITLE_CHARS: usize = 180;
const MAX_URL_CHARS: usize = 600;
const MAX_SNIPPET_CHARS: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSearchContext {
    pub query: String,
    pub results: Vec<WebSearchResult>,
}

#[derive(Clone)]
pub struct WebSearchClient {
    http: reqwest::Client,
    brave_api_key: Option<String>,
    max_results: usize,
}

impl WebSearchClient {
    pub fn new(cfg: &ChatConfig, brave_api_key: Option<String>) -> Result<Option<Self>> {
        if !cfg.web_search_enabled {
            return Ok(None);
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.web_search_timeout_secs))
            .user_agent("SuperSighurt/1.0 live-search")
            .build()
            .context("build web-search HTTP client")?;
        Ok(Some(Self {
            http,
            brave_api_key: brave_api_key.filter(|key| !key.trim().is_empty()),
            max_results: cfg.web_search_max_results,
        }))
    }

    pub async fn search(&self, query: &str) -> Result<WebSearchContext> {
        let query = truncate_chars(query.trim(), MAX_QUERY_CHARS);
        let mut results = if let Some(key) = &self.brave_api_key {
            self.search_brave(&query, key).await?
        } else {
            let mut fallback = self.search_duckduckgo(&query).await.unwrap_or_else(|error| {
                tracing::warn!("DuckDuckGo Instant Answer search failed: {:#}", error);
                Vec::new()
            });
            if fallback.len() < self.max_results {
                match self.search_wikipedia(&query).await {
                    Ok(mut wiki) => fallback.append(&mut wiki),
                    Err(error) => tracing::warn!("Wikimedia search failed: {:#}", error),
                }
            }
            fallback
        };
        deduplicate_results(&mut results);
        results.truncate(self.max_results);
        Ok(WebSearchContext { query, results })
    }

    async fn search_brave(&self, query: &str, key: &str) -> Result<Vec<WebSearchResult>> {
        let response = self
            .http
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", key)
            .query(&[("q", query), ("count", &self.max_results.to_string())])
            .send()
            .await
            .context("send Brave web search")?
            .error_for_status()
            .context("Brave web search status")?;
        let body = response.text().await.context("read Brave web search")?;
        let value: Value = serde_json::from_str(&body).context("parse Brave web search")?;
        Ok(value
            .pointer("/web/results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                result(
                    item.get("title")?.as_str()?,
                    item.get("url")?.as_str()?,
                    item.get("description").and_then(Value::as_str).unwrap_or(""),
                )
            })
            .collect())
    }

    async fn search_duckduckgo(&self, query: &str) -> Result<Vec<WebSearchResult>> {
        let response = self
            .http
            .get("https://api.duckduckgo.com/")
            .query(&[("q", query), ("format", "json"), ("no_html", "1"), ("skip_disambig", "1")])
            .send()
            .await
            .context("send DuckDuckGo Instant Answer search")?
            .error_for_status()
            .context("DuckDuckGo Instant Answer status")?;
        let body = response.text().await.context("read DuckDuckGo Instant Answer")?;
        let value: Value = serde_json::from_str(&body).context("parse DuckDuckGo Instant Answer")?;
        let mut results = Vec::new();
        if let (Some(url), Some(snippet)) = (
            value.get("AbstractURL").and_then(Value::as_str),
            value.get("AbstractText").and_then(Value::as_str),
        ) {
            if let Some(item) = result(
                value.get("Heading").and_then(Value::as_str).unwrap_or(query),
                url,
                snippet,
            ) {
                results.push(item);
            }
        }
        collect_related_topics(value.get("RelatedTopics"), &mut results);
        Ok(results)
    }

    async fn search_wikipedia(&self, query: &str) -> Result<Vec<WebSearchResult>> {
        let response = self
            .http
            .get("https://en.wikipedia.org/w/rest.php/v1/search/page")
            .query(&[("q", query), ("limit", &self.max_results.to_string())])
            .send()
            .await
            .context("send Wikimedia page search")?
            .error_for_status()
            .context("Wikimedia page search status")?;
        let body = response.text().await.context("read Wikimedia page search")?;
        let value: Value = serde_json::from_str(&body).context("parse Wikimedia page search")?;
        Ok(value
            .get("pages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|page| {
                let title = page.get("title")?.as_str()?;
                let key = page.get("key").and_then(Value::as_str).unwrap_or(title);
                let url = format!("https://en.wikipedia.org/wiki/{}", percent_encode_path(key));
                let excerpt = page.get("excerpt").and_then(Value::as_str).unwrap_or("");
                let description = page.get("description").and_then(Value::as_str).unwrap_or("");
                let snippet = if excerpt.is_empty() {
                    description.to_string()
                } else if description.is_empty() {
                    excerpt.to_string()
                } else {
                    format!("{description}. {excerpt}")
                };
                result(title, &url, &strip_html(&snippet))
            })
            .collect())
    }
}

/// Return a query only for explicit retrieval language. Ordinary discussion
/// about searching does not cause network traffic.
pub fn explicit_search_query(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let prefixes = [
        "!search ",
        "/search ",
        "search the web for ",
        "search the internet for ",
        "search online for ",
        "look up ",
        "please look up ",
        "can you look up ",
        "could you look up ",
        "can you search the web for ",
        "could you search the web for ",
        "please search the web for ",
        "can you search the internet for ",
        "could you search the internet for ",
    ];
    for prefix in prefixes {
        if lowered.starts_with(prefix) {
            let query = trimmed[prefix.len()..]
                .trim()
                .trim_end_matches(['?', '.', '!'])
                .trim();
            if query.chars().count() >= 2 {
                return Some(truncate_chars(query, MAX_QUERY_CHARS));
            }
        }
    }
    None
}

fn collect_related_topics(value: Option<&Value>, output: &mut Vec<WebSearchResult>) {
    let Some(items) = value.and_then(Value::as_array) else {
        return;
    };
    for item in items {
        if let Some(nested) = item.get("Topics") {
            collect_related_topics(Some(nested), output);
            continue;
        }
        if let (Some(text), Some(url)) = (
            item.get("Text").and_then(Value::as_str),
            item.get("FirstURL").and_then(Value::as_str),
        ) {
            let title = text.split(" - ").next().unwrap_or(text);
            if let Some(found) = result(title, url, text) {
                output.push(found);
            }
        }
    }
}

fn result(title: &str, url: &str, snippet: &str) -> Option<WebSearchResult> {
    if !url.starts_with("https://")
        || url.chars().any(|character| {
            character.is_whitespace() || matches!(character, '<' | '>' | '`' | '"')
        })
    {
        return None;
    }
    let title = normalized_text(title, MAX_TITLE_CHARS);
    let url = truncate_chars(url.trim(), MAX_URL_CHARS);
    let snippet = normalized_text(snippet, MAX_SNIPPET_CHARS);
    if title.is_empty() || url.is_empty() || snippet.is_empty() {
        return None;
    }
    Some(WebSearchResult { title, url, snippet })
}

fn deduplicate_results(results: &mut Vec<WebSearchResult>) {
    let mut urls = HashSet::new();
    results.retain(|item| urls.insert(item.url.clone()));
}

fn normalized_text(value: &str, max_chars: usize) -> String {
    truncate_chars(&strip_html(value).split_whitespace().collect::<Vec<_>>().join(" "), max_chars)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn strip_html(value: &str) -> String {
    let mut output = String::new();
    let mut inside_tag = false;
    for character in value.chars() {
        match character {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => output.push(character),
            _ => {}
        }
    }
    output
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn percent_encode_path(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_search_language_triggers_network_path() {
        assert_eq!(explicit_search_query("!search tensor ash"), Some("tensor ash".into()));
        assert_eq!(
            explicit_search_query("Could you search the web for Runpod H100 pricing?"),
            Some("Runpod H100 pricing".into())
        );
        assert_eq!(explicit_search_query("please look up NixOS online"), Some("NixOS online".into()));
        assert_eq!(explicit_search_query("we should improve search someday"), None);
        assert_eq!(explicit_search_query("look up"), None);
    }

    #[test]
    fn result_rejects_non_https_and_strips_markup() {
        assert!(result("bad", "http://example.com", "nope").is_none());
        assert!(result("bad", "https://example.com/>evil", "nope").is_none());
        let value = result("<b>Rust</b>", "https://example.com/rust", "A <span>language</span> &amp; tool").unwrap();
        assert_eq!(value.title, "Rust");
        assert_eq!(value.snippet, "A language & tool");
    }
}
