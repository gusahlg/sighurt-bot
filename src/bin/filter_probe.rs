//! Offline harness for the outgoing-reply content filter.
//!
//! Reads candidate bot replies from stdin (one per line), runs each through
//! the exact production `ReplyFilter` (deny-list + AI judge from the given
//! config.toml), and prints one tab-separated verdict line per input:
//!
//!     PASS|REJECT <reason> <matched-terms> <input-prefix>
//!
//! Used to red-team the moderation path with real model outputs without
//! posting anything to Discord. Exit code 0 always; the verdicts are the
//! output.

use anyhow::{Context, Result};
use discord_bot::config::FilterConfig;
use discord_bot::reply_filter::{ReplyFilter, ReplyScreen};
use std::io::BufRead;

#[derive(serde::Deserialize)]
struct ProbeConfig {
    filter: FilterConfig,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read {config_path}"))?;
    let config: ProbeConfig = toml::from_str(&raw).context("parse [filter] config")?;
    let filter = ReplyFilter::from_config(&config.filter)?;
    eprintln!("{}", filter.boot_summary());

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let text = line?;
        if text.trim().is_empty() {
            continue;
        }
        let prefix: String = text.chars().take(80).collect();
        match filter.screen(&text).await {
            ReplyScreen::Pass => println!("PASS\t-\t-\t{prefix}"),
            ReplyScreen::Rejected { matched, reason } => {
                println!("REJECT\t{reason}\t{}\t{prefix}", matched.join(","))
            }
        }
    }
    Ok(())
}
