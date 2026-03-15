-- Moderation action logs
CREATE TABLE IF NOT EXISTS mod_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    moderator_id TEXT NOT NULL,
    action TEXT NOT NULL,
    reason TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_mod_logs_guild ON mod_logs(guild_id);
CREATE INDEX IF NOT EXISTS idx_mod_logs_user ON mod_logs(user_id);
CREATE INDEX IF NOT EXISTS idx_mod_logs_guild_user ON mod_logs(guild_id, user_id);
CREATE INDEX IF NOT EXISTS idx_mod_logs_guild_created ON mod_logs(guild_id, created_at);

-- Filtered words per guild
CREATE TABLE IF NOT EXISTS filtered_words (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id TEXT NOT NULL,
    word TEXT NOT NULL,
    UNIQUE(guild_id, word)
);

CREATE INDEX IF NOT EXISTS idx_filtered_words_guild ON filtered_words(guild_id);

-- Guild settings
CREATE TABLE IF NOT EXISTS guild_settings (
    guild_id TEXT PRIMARY KEY,
    spam_enabled INTEGER NOT NULL DEFAULT 1,
    spam_threshold INTEGER NOT NULL DEFAULT 5,
    spam_interval INTEGER NOT NULL DEFAULT 5,
    raid_enabled INTEGER NOT NULL DEFAULT 1,
    raid_threshold INTEGER NOT NULL DEFAULT 10,
    raid_interval INTEGER NOT NULL DEFAULT 10,
    log_channel_id TEXT,
    autorole_id TEXT,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
