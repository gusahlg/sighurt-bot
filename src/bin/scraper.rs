//! Discord history backfill scraper.
//!
//! One-shot binary. Reads DISCORD_TOKEN from the bot's `.env`, walks every
//! guild the bot is in, and pages backward through every accessible
//! text-channel into `data/channels/<bucket>/<channel_id>.tsv`. Resumable —
//! re-run as needed; cursor state in `*.cursor` files means each run only
//! fetches further back than the previous one.

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
    let total = scrape::scrape_all(http).await?;
    tracing::info!("Scrape finished: {} messages total", total);
    Ok(())
}
