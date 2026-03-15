use anyhow::Result;
use sqlx::SqlitePool;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
    Id,
};

#[derive(Debug, Clone)]
pub struct ModLog {
    pub id: i64,
    pub guild_id: String,
    pub user_id: String,
    pub moderator_id: String,
    pub action: String,
    pub reason: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct GuildSettings {
    pub guild_id: String,
    pub spam_enabled: bool,
    pub spam_threshold: i64,
    pub spam_interval: i64,
    pub raid_enabled: bool,
    pub raid_threshold: i64,
    pub raid_interval: i64,
    pub log_channel_id: Option<String>,
}

impl Default for GuildSettings {
    fn default() -> Self {
        Self {
            guild_id: String::new(),
            spam_enabled: true,
            spam_threshold: 5,
            spam_interval: 5,
            raid_enabled: true,
            raid_threshold: 10,
            raid_interval: 10,
            log_channel_id: None,
        }
    }
}

pub async fn log_mod_action(
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    moderator_id: Id<UserMarker>,
    action: &str,
    reason: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO mod_logs (guild_id, user_id, moderator_id, action, reason) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(guild_id.to_string())
    .bind(user_id.to_string())
    .bind(moderator_id.to_string())
    .bind(action)
    .bind(reason)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn get_guild_settings(pool: &SqlitePool, guild_id: Id<GuildMarker>) -> Result<GuildSettings> {
    let guild_id_str = guild_id.to_string();

    let row: Option<(String, bool, i64, i64, bool, i64, i64, Option<String>)> = sqlx::query_as(
        r#"SELECT
            guild_id,
            spam_enabled,
            spam_threshold,
            spam_interval,
            raid_enabled,
            raid_threshold,
            raid_interval,
            log_channel_id
        FROM guild_settings WHERE guild_id = ?"#,
    )
    .bind(&guild_id_str)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(guild_id, spam_enabled, spam_threshold, spam_interval, raid_enabled, raid_threshold, raid_interval, log_channel_id)| {
        GuildSettings {
            guild_id,
            spam_enabled,
            spam_threshold,
            spam_interval,
            raid_enabled,
            raid_threshold,
            raid_interval,
            log_channel_id,
        }
    }).unwrap_or_else(|| GuildSettings {
        guild_id: guild_id_str,
        ..Default::default()
    }))
}

pub async fn update_guild_settings(pool: &SqlitePool, settings: &GuildSettings) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO guild_settings (guild_id, spam_enabled, spam_threshold, spam_interval, raid_enabled, raid_threshold, raid_interval, log_channel_id)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(guild_id) DO UPDATE SET
            spam_enabled = excluded.spam_enabled,
            spam_threshold = excluded.spam_threshold,
            spam_interval = excluded.spam_interval,
            raid_enabled = excluded.raid_enabled,
            raid_threshold = excluded.raid_threshold,
            raid_interval = excluded.raid_interval,
            log_channel_id = excluded.log_channel_id"#,
    )
    .bind(&settings.guild_id)
    .bind(settings.spam_enabled)
    .bind(settings.spam_threshold)
    .bind(settings.spam_interval)
    .bind(settings.raid_enabled)
    .bind(settings.raid_threshold)
    .bind(settings.raid_interval)
    .bind(&settings.log_channel_id)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn add_filtered_word(pool: &SqlitePool, guild_id: Id<GuildMarker>, word: &str) -> Result<bool> {
    let result = sqlx::query(
        "INSERT OR IGNORE INTO filtered_words (guild_id, word) VALUES (?, ?)",
    )
    .bind(guild_id.to_string())
    .bind(word.to_lowercase())
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn remove_filtered_word(pool: &SqlitePool, guild_id: Id<GuildMarker>, word: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM filtered_words WHERE guild_id = ? AND word = ?")
        .bind(guild_id.to_string())
        .bind(word.to_lowercase())
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn get_filtered_words(pool: &SqlitePool, guild_id: Id<GuildMarker>) -> Result<Vec<String>> {
    let words: Vec<(String,)> = sqlx::query_as("SELECT word FROM filtered_words WHERE guild_id = ?")
        .bind(guild_id.to_string())
        .fetch_all(pool)
        .await?;

    Ok(words.into_iter().map(|(w,)| w).collect())
}

pub async fn set_log_channel(
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    channel_id: Option<Id<ChannelMarker>>,
) -> Result<()> {
    let guild_id_str = guild_id.to_string();
    let channel_id_str = channel_id.map(|id| id.to_string());

    // Atomic upsert - avoids read-modify-write race condition
    sqlx::query(
        r#"INSERT INTO guild_settings (guild_id, log_channel_id)
        VALUES (?, ?)
        ON CONFLICT(guild_id) DO UPDATE SET log_channel_id = excluded.log_channel_id"#,
    )
    .bind(&guild_id_str)
    .bind(&channel_id_str)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn get_autorole(pool: &SqlitePool, guild_id: Id<GuildMarker>) -> Result<Option<Id<RoleMarker>>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT autorole_id FROM guild_settings WHERE guild_id = ?"
    )
    .bind(guild_id.to_string())
    .fetch_optional(pool)
    .await?;

    Ok(row
        .and_then(|(id,)| id)
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|&id| id != 0)
        .map(Id::new))
}

pub async fn set_autorole(
    pool: &SqlitePool,
    guild_id: Id<GuildMarker>,
    role_id: Option<Id<RoleMarker>>,
) -> Result<()> {
    let guild_id_str = guild_id.to_string();
    let role_id_str = role_id.map(|id| id.to_string());

    // Include all defaults so first-insert creates a complete row
    sqlx::query(
        r#"INSERT INTO guild_settings (guild_id, spam_enabled, spam_threshold, spam_interval, raid_enabled, raid_threshold, raid_interval, autorole_id)
        VALUES (?, 1, 5, 5, 1, 10, 10, ?)
        ON CONFLICT(guild_id) DO UPDATE SET autorole_id = excluded.autorole_id"#,
    )
    .bind(&guild_id_str)
    .bind(&role_id_str)
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guild_settings_default() {
        let settings = GuildSettings::default();

        assert!(settings.guild_id.is_empty());
        assert!(settings.spam_enabled);
        assert_eq!(settings.spam_threshold, 5);
        assert_eq!(settings.spam_interval, 5);
        assert!(settings.raid_enabled);
        assert_eq!(settings.raid_threshold, 10);
        assert_eq!(settings.raid_interval, 10);
        assert!(settings.log_channel_id.is_none());
    }

    #[test]
    fn test_guild_settings_clone() {
        let mut settings = GuildSettings::default();
        settings.guild_id = "123456789".to_string();
        settings.spam_threshold = 10;

        let cloned = settings.clone();

        assert_eq!(cloned.guild_id, "123456789");
        assert_eq!(cloned.spam_threshold, 10);
    }

    #[test]
    fn test_guild_settings_debug() {
        let settings = GuildSettings::default();
        let debug_str = format!("{:?}", settings);

        assert!(debug_str.contains("GuildSettings"));
        assert!(debug_str.contains("spam_enabled"));
    }

    #[test]
    fn test_guild_settings_with_log_channel() {
        let mut settings = GuildSettings::default();
        settings.log_channel_id = Some("987654321".to_string());

        assert_eq!(settings.log_channel_id.as_deref(), Some("987654321"));
    }

    #[test]
    fn test_mod_log_struct() {
        let log = ModLog {
            id: 1,
            guild_id: "123".to_string(),
            user_id: "456".to_string(),
            moderator_id: "789".to_string(),
            action: "ban".to_string(),
            reason: Some("Spam".to_string()),
            created_at: "2025-01-01T00:00:00Z".to_string(),
        };

        assert_eq!(log.id, 1);
        assert_eq!(log.guild_id, "123");
        assert_eq!(log.action, "ban");
        assert_eq!(log.reason, Some("Spam".to_string()));
    }

    #[test]
    fn test_mod_log_clone() {
        let log = ModLog {
            id: 1,
            guild_id: "123".to_string(),
            user_id: "456".to_string(),
            moderator_id: "789".to_string(),
            action: "kick".to_string(),
            reason: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        };

        let cloned = log.clone();
        assert_eq!(cloned.id, log.id);
        assert_eq!(cloned.action, "kick");
        assert!(cloned.reason.is_none());
    }

    #[test]
    fn test_mod_log_debug() {
        let log = ModLog {
            id: 1,
            guild_id: "123".to_string(),
            user_id: "456".to_string(),
            moderator_id: "789".to_string(),
            action: "mute".to_string(),
            reason: Some("Testing".to_string()),
            created_at: "2025-01-01T00:00:00Z".to_string(),
        };

        let debug_str = format!("{:?}", log);
        assert!(debug_str.contains("ModLog"));
        assert!(debug_str.contains("mute"));
    }

    #[test]
    fn test_guild_settings_spread_with_defaults() {
        // Test the pattern used in get_guild_settings
        let custom_guild_id = "custom_123".to_string();
        let settings = GuildSettings {
            guild_id: custom_guild_id.clone(),
            ..Default::default()
        };

        assert_eq!(settings.guild_id, custom_guild_id);
        assert!(settings.spam_enabled);
        assert_eq!(settings.spam_threshold, 5);
    }

    #[test]
    fn test_spam_settings_range() {
        let mut settings = GuildSettings::default();

        // Test various threshold values
        settings.spam_threshold = 0;
        assert_eq!(settings.spam_threshold, 0);

        settings.spam_threshold = 100;
        assert_eq!(settings.spam_threshold, 100);

        settings.spam_interval = 0;
        assert_eq!(settings.spam_interval, 0);

        settings.spam_interval = 3600;
        assert_eq!(settings.spam_interval, 3600);
    }

    #[test]
    fn test_raid_settings_range() {
        let mut settings = GuildSettings::default();

        settings.raid_threshold = 1;
        assert_eq!(settings.raid_threshold, 1);

        settings.raid_threshold = 1000;
        assert_eq!(settings.raid_threshold, 1000);

        settings.raid_interval = 1;
        assert_eq!(settings.raid_interval, 1);

        settings.raid_interval = 600;
        assert_eq!(settings.raid_interval, 600);
    }

    #[test]
    fn test_guild_settings_toggle_features() {
        let mut settings = GuildSettings::default();

        // Initially enabled
        assert!(settings.spam_enabled);
        assert!(settings.raid_enabled);

        // Disable features
        settings.spam_enabled = false;
        settings.raid_enabled = false;

        assert!(!settings.spam_enabled);
        assert!(!settings.raid_enabled);
    }
}
