//! Discord history backfill + catch-up scraper.
//!
//! One-shot binary. Reads DISCORD_TOKEN from the bot's `.env`, walks every
//! guild the bot is in (channels AND threads), pages backward through each
//! one's history and then forward past the stored cursor into
//! `data/channels/<bucket>/<channel_id>.tsv`. Resumable — re-run as needed;
//! cursor state in `*.cursor` files means each run only fetches history it
//! hasn't seen. The running bot performs the same scrape periodically
//! in-process (see `[scrape]` in config.toml); this binary remains for
//! manual/offline runs.

use anyhow::Result;
use std::sync::Arc;
use twilight_http::Client;

use discord_bot::scrape;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("discord_bot=info".parse()?)
                .add_directive("scraper=info".parse()?),
        )
        .init();

    let token = std::env::var("DISCORD_TOKEN")
        .map_err(|_| anyhow::anyhow!("DISCORD_TOKEN env var not set"))?;
    if token.is_empty() {
        return Err(anyhow::anyhow!("DISCORD_TOKEN must not be empty"));
    }

    let http = Arc::new(Client::new(token));
    tracing::info!("Scraper starting");
    let stats = scrape::scrape_all(http).await?;
    tracing::info!(
        "Scrape finished: {} channel(s)/thread(s) scanned, {} new message(s), {} skipped",
        stats.channels_scanned,
        stats.new_messages,
        stats.skipped_channels.len()
    );
    Ok(())
}
