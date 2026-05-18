//! Discord channel backfill scraper.
//!
//! Walks every guild the bot is in, enumerates text-like channels, and pages
//! backwards through each channel's message history via `before(MessageId)`.
//! Resumable: per-channel cursor in `data/channels/<bucket>/<id>.cursor`
//! advances only after a batch is fully appended, so a crash mid-scrape
//! resumes at the right snowflake. Forward catch-up (since `newest_seen_id`)
//! is the live event handler's job, not the scraper's.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use twilight_http::Client;
use twilight_model::channel::ChannelType;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};

use crate::channel_log;

/// 100 ms between API calls. Discord's per-route limit for message history is
/// roughly 5 req/s; twilight blocks on 429, so this is mostly to be polite.
const PACE: Duration = Duration::from_millis(100);

pub async fn scrape_all(http: Arc<Client>) -> Result<u64> {
    let guilds = http
        .current_user_guilds()
        .await
        .context("list current user guilds")?
        .models()
        .await
        .context("decode guilds")?;

    tracing::info!("Scraping {} guild(s)", guilds.len());

    let mut total: u64 = 0;
    for guild in guilds {
        match scrape_guild(&http, guild.id).await {
            Ok(n) => total += n,
            Err(e) => tracing::warn!("guild {} failed: {}", guild.id, e),
        }
    }
    tracing::info!("Scrape complete: {} messages total", total);
    Ok(total)
}

pub async fn scrape_guild(http: &Client, guild_id: Id<GuildMarker>) -> Result<u64> {
    let channels = http
        .guild_channels(guild_id)
        .await
        .context("list guild channels")?
        .models()
        .await
        .context("decode channels")?;

    let bucket = channel_log::bucket_for_guild(Some(guild_id));
    tracing::info!("guild {}: {} channel(s)", guild_id, channels.len());

    let mut total: u64 = 0;
    for channel in channels {
        if !is_text_like(channel.kind) {
            continue;
        }
        let label = channel.name.as_deref().unwrap_or("?");
        match scrape_channel(http, &bucket, channel.id).await {
            Ok(n) => {
                if n > 0 {
                    tracing::info!("  channel #{} ({}): +{} msgs", label, channel.id, n);
                }
                total += n;
            }
            Err(e) => {
                tracing::warn!("  channel #{} ({}) failed: {}", label, channel.id, e);
            }
        }
    }
    Ok(total)
}

pub async fn scrape_channel(
    http: &Client,
    bucket: &str,
    channel_id: Id<ChannelMarker>,
) -> Result<u64> {
    let cid = channel_id.get();
    let (mut oldest, mut newest) = channel_log::read_cursor(bucket, cid)?;
    let mut total: u64 = 0;

    loop {
        let batch = match oldest {
            Some(o) => {
                http.channel_messages(channel_id)
                    .before(Id::new(o))
                    .limit(100)?
                    .await
            }
            None => http.channel_messages(channel_id).limit(100)?.await,
        };

        let messages = match batch {
            Ok(resp) => resp.models().await.context("decode messages")?,
            Err(e) => {
                tracing::debug!("channel {} fetch error (likely 403/404): {}", cid, e);
                break;
            }
        };

        if messages.is_empty() {
            break;
        }

        // Discord returns newest-first within a batch. Track the smallest id
        // in the batch — that's where we'll resume on the next iteration.
        let mut batch_oldest = u64::MAX;
        let mut batch_newest = 0u64;
        for m in &messages {
            channel_log::append(m, bucket)?;
            let mid = m.id.get();
            if mid < batch_oldest {
                batch_oldest = mid;
            }
            if mid > batch_newest {
                batch_newest = mid;
            }
            total += 1;
        }

        oldest = Some(batch_oldest);
        // On first batch ever (newest was None), seed newest with the highest
        // id we just observed so live capture's dedup gate starts strict.
        if newest.is_none() {
            newest = Some(batch_newest);
        }
        channel_log::write_cursor(bucket, cid, oldest, newest)?;

        if messages.len() < 100 {
            break;
        }
        tokio::time::sleep(PACE).await;
    }

    Ok(total)
}

fn is_text_like(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::GuildText
            | ChannelType::GuildAnnouncement
            | ChannelType::AnnouncementThread
            | ChannelType::PublicThread
            | ChannelType::PrivateThread
    )
}
