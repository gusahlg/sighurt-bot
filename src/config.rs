use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub automod: AutomodConfig,
    #[serde(default)]
    pub moderation: ModerationConfig,
    #[serde(default)]
    pub ai: AiModeConfig,
}

#[derive(Debug, Deserialize)]
pub struct AutomodConfig {
    #[serde(default = "default_true")]
    pub spam_enabled: bool,
    #[serde(default = "default_spam_threshold")]
    pub spam_threshold: u32,
    #[serde(default = "default_spam_interval")]
    pub spam_interval: u64,
    #[serde(default = "default_true")]
    pub raid_enabled: bool,
    #[serde(default = "default_raid_threshold")]
    pub raid_threshold: u32,
    #[serde(default = "default_raid_interval")]
    pub raid_interval: u64,
}

#[derive(Debug, Deserialize)]
pub struct ModerationConfig {
    #[serde(default = "default_reason")]
    pub default_reason: String,
    #[serde(default = "default_delete_days")]
    pub default_delete_days: u8,
}

#[derive(Debug, Deserialize)]
pub struct AiModeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub model_path: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_spam_threshold() -> u32 {
    5
}

fn default_spam_interval() -> u64 {
    5
}

fn default_raid_threshold() -> u32 {
    10
}

fn default_raid_interval() -> u64 {
    10
}

fn default_reason() -> String {
    "No reason provided".to_string()
}

fn default_delete_days() -> u8 {
    1
}

fn default_max_concurrent() -> usize {
    2 // Conservative for Pi 4
}

fn default_queue_capacity() -> usize {
    100
}

fn default_timeout_secs() -> u64 {
    30
}

impl Default for AutomodConfig {
    fn default() -> Self {
        Self {
            spam_enabled: true,
            spam_threshold: 5,
            spam_interval: 5,
            raid_enabled: true,
            raid_threshold: 10,
            raid_interval: 10,
        }
    }
}

impl Default for ModerationConfig {
    fn default() -> Self {
        Self {
            default_reason: "No reason provided".to_string(),
            default_delete_days: 1,
        }
    }
}

impl Default for AiModeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_concurrent: 2,
            queue_capacity: 100,
            timeout_secs: 30,
            model_path: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            automod: AutomodConfig::default(),
            moderation: ModerationConfig::default(),
            ai: AiModeConfig::default(),
        }
    }
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        if path.as_ref().exists() {
            let content = std::fs::read_to_string(path)?;
            let config: Config = toml::from_str(&content)?;
            Ok(config)
        } else {
            tracing::warn!("Config file not found, using defaults");
            Ok(Config::default())
        }
    }

    /// Validate configuration values for correctness.
    /// Returns an error if any values are out of acceptable range.
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate automod settings
        if self.automod.spam_interval == 0 && self.automod.spam_enabled {
            anyhow::bail!("automod.spam_interval must be > 0 when spam detection is enabled");
        }
        if self.automod.raid_interval == 0 && self.automod.raid_enabled {
            anyhow::bail!("automod.raid_interval must be > 0 when raid detection is enabled");
        }
        if self.automod.spam_threshold == 0 && self.automod.spam_enabled {
            tracing::warn!(
                "automod.spam_threshold is 0: every single message will trigger spam detection"
            );
        }
        if self.automod.raid_threshold == 0 && self.automod.raid_enabled {
            tracing::warn!(
                "automod.raid_threshold is 0: every single join will trigger raid detection"
            );
        }

        // Validate moderation settings
        if self.moderation.default_delete_days > 7 {
            anyhow::bail!("moderation.default_delete_days must be 0-7 (Discord API limit)");
        }

        // Validate AI settings
        if self.ai.enabled {
            if self.ai.max_concurrent == 0 {
                anyhow::bail!("ai.max_concurrent must be > 0 when AI is enabled");
            }
            if self.ai.queue_capacity == 0 {
                anyhow::bail!("ai.queue_capacity must be > 0 when AI is enabled");
            }
            if self.ai.timeout_secs == 0 {
                anyhow::bail!("ai.timeout_secs must be > 0 when AI is enabled");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_automod_config_default() {
        let config = AutomodConfig::default();
        assert!(config.spam_enabled);
        assert_eq!(config.spam_threshold, 5);
        assert_eq!(config.spam_interval, 5);
        assert!(config.raid_enabled);
        assert_eq!(config.raid_threshold, 10);
        assert_eq!(config.raid_interval, 10);
    }

    #[test]
    fn test_moderation_config_default() {
        let config = ModerationConfig::default();
        assert_eq!(config.default_reason, "No reason provided");
        assert_eq!(config.default_delete_days, 1);
    }

    #[test]
    fn test_ai_mode_config_default() {
        let config = AiModeConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_concurrent, 2);
        assert_eq!(config.queue_capacity, 100);
        assert_eq!(config.timeout_secs, 30);
        assert!(config.model_path.is_none());
    }

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert!(config.automod.spam_enabled);
        assert!(!config.ai.enabled);
    }

    #[test]
    fn test_config_load_missing_file() {
        let result = Config::load("nonexistent.toml");
        assert!(result.is_ok());
        let config = result.unwrap();
        assert!(config.automod.spam_enabled);
    }

    #[test]
    fn test_config_parse_minimal() {
        let toml_content = r#"
            [automod]
            spam_enabled = false

            [ai]
            enabled = true
            max_concurrent = 4
        "#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(!config.automod.spam_enabled);
        assert!(config.ai.enabled);
        assert_eq!(config.ai.max_concurrent, 4);
    }

    #[test]
    fn test_config_parse_full() {
        let toml_content = r#"
            [automod]
            spam_enabled = true
            spam_threshold = 10
            spam_interval = 30
            raid_enabled = false
            raid_threshold = 20
            raid_interval = 60

            [moderation]
            default_reason = "Custom reason"
            default_delete_days = 3

            [ai]
            enabled = true
            max_concurrent = 1
            queue_capacity = 50
            timeout_secs = 60
            model_path = "/path/to/model"
        "#;
        let config: Config = toml::from_str(toml_content).unwrap();

        assert!(config.automod.spam_enabled);
        assert_eq!(config.automod.spam_threshold, 10);
        assert!(!config.automod.raid_enabled);

        assert_eq!(config.moderation.default_reason, "Custom reason");
        assert_eq!(config.moderation.default_delete_days, 3);

        assert!(config.ai.enabled);
        assert_eq!(config.ai.max_concurrent, 1);
        assert_eq!(config.ai.queue_capacity, 50);
        assert_eq!(config.ai.model_path, Some("/path/to/model".to_string()));
    }

    #[test]
    fn test_config_validate_defaults() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validate_zero_spam_interval() {
        let mut config = Config::default();
        config.automod.spam_interval = 0;
        config.automod.spam_enabled = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_zero_raid_interval() {
        let mut config = Config::default();
        config.automod.raid_interval = 0;
        config.automod.raid_enabled = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_disabled_zero_interval_ok() {
        let mut config = Config::default();
        config.automod.spam_interval = 0;
        config.automod.spam_enabled = false;
        config.automod.raid_interval = 0;
        config.automod.raid_enabled = false;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validate_delete_days_too_high() {
        let mut config = Config::default();
        config.moderation.default_delete_days = 8;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_ai_zero_concurrent() {
        let mut config = Config::default();
        config.ai.enabled = true;
        config.ai.max_concurrent = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_ai_zero_queue() {
        let mut config = Config::default();
        config.ai.enabled = true;
        config.ai.queue_capacity = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_ai_zero_timeout() {
        let mut config = Config::default();
        config.ai.enabled = true;
        config.ai.timeout_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_ai_disabled_zero_values_ok() {
        let mut config = Config::default();
        config.ai.enabled = false;
        config.ai.max_concurrent = 0;
        config.ai.queue_capacity = 0;
        config.ai.timeout_secs = 0;
        assert!(config.validate().is_ok());
    }
}
