//! Public surface of the `discord_bot` crate.
//!
//! Only modules shared between the main bot binary (`src/main.rs`) and the
//! backfill scraper binary (`src/bin/scraper.rs`) live here. Everything else
//! is binary-private (declared via `mod` in main.rs) so it doesn't pollute
//! the public API or slow down `cargo doc`.

pub mod channel_log;
pub mod config;
pub mod reply_filter;
pub mod scrape;
