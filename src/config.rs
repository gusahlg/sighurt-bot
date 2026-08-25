use anyhow::Context;
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
    #[serde(default)]
    pub chat: ChatConfig,
    #[serde(default)]
    pub filter: FilterConfig,
    #[serde(default)]
    pub scrape: ScrapeConfig,
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

#[derive(Debug, Clone, Deserialize)]
pub struct ChatConfig {
    /// Initial state of the runtime AI toggle. The toggle can be flipped at
    /// runtime via the `!ai on|off` admin command; this is just the boot value.
    #[serde(default)]
    pub enabled: bool,
    /// URL of the LLM HTTP server (e.g. http://100.118.41.103:8088).
    #[serde(default = "default_chat_endpoint")]
    pub endpoint_url: String,
    /// Per-request HTTP timeout.
    #[serde(default = "default_chat_timeout")]
    pub request_timeout_secs: u64,
    /// Discord user IDs allowed to run `!ai on|off|status`. Empty disables the
    /// command entirely — leave empty if you don't want runtime toggling.
    #[serde(default)]
    pub admin_user_ids: Vec<u64>,
    /// Whether other bots' DMs/@-mentions may trigger a chat reply. Our own
    /// messages and webhooks are always ignored regardless of this.
    #[serde(default = "default_true")]
    pub respond_to_bots: bool,
    /// Max consecutive bot-triggered chat replies per channel before we go
    /// quiet until a human speaks (loop guard for bot-to-bot ping-pong).
    #[serde(default = "default_max_bot_chain")]
    pub max_bot_chain: u32,
    /// Max characters of referenced-message text forwarded to the LLM as
    /// reply context when the trigger is a Discord reply.
    #[serde(default = "default_reply_context_max_chars")]
    pub reply_context_max_chars: usize,
    /// Number of recent ambient channel messages sent to the LLM before the
    /// trigger. Discord returns at most 100; we intentionally cap this much
    /// lower so one mention cannot create an unbounded prompt.
    #[serde(default = "default_recent_context_messages")]
    pub recent_context_messages: usize,
    /// Per-message character cap for ambient context.
    #[serde(default = "default_context_message_max_chars")]
    pub context_message_max_chars: usize,
    /// Minimum seconds between chat replies per channel. A trigger arriving
    /// while a reply is in flight, or sooner than this after the previous
    /// trigger, is skipped. Applies to DMs (which automod doesn't cover) as a
    /// flood guard, and everywhere else too.
    #[serde(default = "default_min_seconds_between_replies")]
    pub min_seconds_between_replies: u64,
    /// Enable explicit live retrieval for `!search` and natural-language
    /// "search the web" requests.
    #[serde(default = "default_true")]
    pub web_search_enabled: bool,
    /// Maximum untrusted result snippets admitted to one LLM prompt.
    #[serde(default = "default_web_search_max_results")]
    pub web_search_max_results: usize,
    /// Independent timeout for the external search provider.
    #[serde(default = "default_web_search_timeout_secs")]
    pub web_search_timeout_secs: u64,
    /// Reply unprompted roughly every N human messages per channel (with
    /// jitter), so Sig joins conversations instead of only answering pings.
    /// 0 disables unprompted replies. DMs are unaffected (always answered).
    #[serde(default = "default_unprompted_reply_every")]
    pub unprompted_reply_every: u32,
    /// Probability (0.0..=1.0) that a new human message is offered to the LLM
    /// as a reaction opportunity — the model then picks one emoji or passes.
    /// 0 disables bot reactions entirely.
    #[serde(default = "default_react_probability")]
    pub react_probability: f64,
    /// Post a short public notice in-channel when the reply filter blocks
    /// Sig's own output (transparency + it's funny). Off = silent (the
    /// replied-to user still gets a private DM).
    #[serde(default = "default_true")]
    pub notify_on_rejection: bool,
    /// Post a short public notice in-channel when generation errors out
    /// (model server down, timeout, queue full) instead of failing silently.
    #[serde(default = "default_true")]
    pub notify_on_error: bool,
    /// Minimum seconds between those notices per channel, so a persistently
    /// broken backend can't spam a channel with error messages. 0 = no limit.
    #[serde(default = "default_notice_cooldown_secs")]
    pub notice_cooldown_secs: u64,
}

/// Two-step content filter over the bot's own outgoing chat replies: a
/// lexical deny-list screen, then a local AI judge for EVERY reply (not just
/// lexically flagged ones). See `reply_filter.rs` for the mechanics.
#[derive(Debug, Deserialize)]
pub struct FilterConfig {
    /// Boot state of the runtime filter toggle (`!filter on|off` flips it at
    /// runtime, same admin allowlist as `!ai`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// OpenAI-compatible chat-completions URL of a LOCAL judge model, e.g.
    /// ollama's `http://127.0.0.1:11434/v1/chat/completions`. Anything that
    /// speaks that wire format works (llama.cpp server, LM Studio, vLLM...).
    /// Unset = no judge: lexically flagged replies are rejected outright.
    #[serde(default)]
    pub judge_url: Option<String>,
    /// Model name passed to the judge endpoint (e.g. "llama-guard3:1b").
    /// Required when judge_url is set.
    #[serde(default)]
    pub judge_model: Option<String>,
    /// "guard" (default) for safety-classifier models that answer
    /// safe/unsafe with their own built-in prompt (llama-guard); "instruct"
    /// for generic chat models that get a yes/no moderation instruction.
    #[serde(default = "default_judge_kind")]
    pub judge_kind: String,
    /// Per-request judge timeout. Generous by default: a CPU-only 1B model
    /// cold-loading can take a while, and a timeout rejects (fail-closed).
    #[serde(default = "default_judge_timeout_secs")]
    pub judge_timeout_secs: u64,
    /// Optional extra deny-list file: one lowercase term per line, `#`
    /// comments. Multi-word lines become phrases, single words exact-token
    /// terms. Lets admins extend the list without recompiling.
    #[serde(default)]
    pub words_file: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ScrapeConfig {
    /// Run the in-process periodic catch-up scrape (backfill-if-needed plus
    /// forward pass over all guilds, channels and threads). First run fires
    /// ~30s after startup so any offline gap heals on boot.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Hours between catch-up runs after the initial one.
    #[serde(default = "default_scrape_interval_hours")]
    pub interval_hours: u64,
}

fn default_true() -> bool {
    true
}

fn default_scrape_interval_hours() -> u64 {
    24
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

fn default_chat_endpoint() -> String {
    "http://127.0.0.1:8088".to_string()
}

fn default_chat_timeout() -> u64 {
    90
}

fn default_max_bot_chain() -> u32 {
    3
}

fn default_reply_context_max_chars() -> usize {
    300
}

fn default_recent_context_messages() -> usize {
    12
}

fn default_context_message_max_chars() -> usize {
    400
}

fn default_min_seconds_between_replies() -> u64 {
    2
}

fn default_web_search_max_results() -> usize {
    4
}

fn default_web_search_timeout_secs() -> u64 {
    12
}

fn default_unprompted_reply_every() -> u32 {
    30
}

fn default_react_probability() -> f64 {
    0.2
}

fn default_notice_cooldown_secs() -> u64 {
    30
}

fn default_judge_timeout_secs() -> u64 {
    45
}

fn default_judge_kind() -> String {
    "guard".to_string()
}

impl FilterConfig {
    pub fn judge_kind(&self) -> anyhow::Result<crate::reply_filter::JudgeKind> {
        match self.judge_kind.trim() {
            "guard" => Ok(crate::reply_filter::JudgeKind::Guard),
            "instruct" => Ok(crate::reply_filter::JudgeKind::Instruct),
            other => anyhow::bail!(
                "filter.judge_kind must be \"guard\" or \"instruct\", got {:?}",
                other
            ),
        }
    }
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

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint_url: default_chat_endpoint(),
            request_timeout_secs: default_chat_timeout(),
            admin_user_ids: Vec::new(),
            respond_to_bots: true,
            max_bot_chain: 3,
            reply_context_max_chars: 300,
            recent_context_messages: default_recent_context_messages(),
            context_message_max_chars: default_context_message_max_chars(),
            min_seconds_between_replies: 2,
            web_search_enabled: true,
            web_search_max_results: default_web_search_max_results(),
            web_search_timeout_secs: default_web_search_timeout_secs(),
            unprompted_reply_every: default_unprompted_reply_every(),
            react_probability: default_react_probability(),
            notify_on_rejection: true,
            notify_on_error: true,
            notice_cooldown_secs: default_notice_cooldown_secs(),
        }
    }
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            judge_url: None,
            judge_model: None,
            judge_kind: default_judge_kind(),
            judge_timeout_secs: default_judge_timeout_secs(),
            words_file: None,
        }
    }
}

impl Default for ScrapeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_hours: 24,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            automod: AutomodConfig::default(),
            moderation: ModerationConfig::default(),
            ai: AiModeConfig::default(),
            chat: ChatConfig::default(),
            filter: FilterConfig::default(),
            scrape: ScrapeConfig::default(),
        }
    }
}

impl Config {
    /// Load config from `path`.
    ///
    /// A MISSING file is not an error: we log and return `Config::default()`
    /// (the documented "no config.toml → defaults" behavior). A file that is
    /// PRESENT but fails to read or parse IS a hard error — returning defaults
    /// there silently boots production with chat disabled and no admins, so we
    /// propagate the parse error for the caller to treat as fatal.
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            tracing::warn!("Config file not found, using defaults");
            return Ok(Config::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let config: Config = toml::from_str(&content)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        Ok(config)
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

        // Validate chat settings UNCONDITIONALLY. The boot `chat.enabled` flag
        // only sets the *initial* runtime toggle — `!ai on` can activate the
        // chat path later — so a ChatRuntime gets built whenever LLM_API_KEY is
        // present regardless of `enabled`. Validating only when enabled=true
        // let a garbage endpoint slip through and blow up at runtime toggle.
        if self.chat.endpoint_url.trim().is_empty() {
            anyhow::bail!("chat.endpoint_url must not be empty");
        }
        if self.chat.request_timeout_secs == 0 {
            anyhow::bail!("chat.request_timeout_secs must be > 0");
        }
        if self.chat.min_seconds_between_replies == 0 {
            anyhow::bail!("chat.min_seconds_between_replies must be > 0");
        }
        if self.chat.recent_context_messages > 50 {
            anyhow::bail!("chat.recent_context_messages must be <= 50");
        }
        if self.chat.context_message_max_chars == 0 || self.chat.context_message_max_chars > 2_000 {
            anyhow::bail!("chat.context_message_max_chars must be in 1..=2000");
        }
        if self.chat.web_search_max_results == 0 || self.chat.web_search_max_results > 8 {
            anyhow::bail!("chat.web_search_max_results must be in 1..=8");
        }
        if self.chat.web_search_timeout_secs == 0 || self.chat.web_search_timeout_secs > 60 {
            anyhow::bail!("chat.web_search_timeout_secs must be in 1..=60");
        }
        if !(0.0..=1.0).contains(&self.chat.react_probability) {
            anyhow::bail!("chat.react_probability must be in 0.0..=1.0");
        }

        // Validate reply-filter settings (same unconditional rule as chat:
        // `!filter on` can activate the path at runtime).
        let judge_url = self.filter.judge_url.as_deref().map(str::trim).unwrap_or("");
        if !judge_url.is_empty() {
            if !judge_url.starts_with("http://") && !judge_url.starts_with("https://") {
                anyhow::bail!("filter.judge_url must be an http(s) URL");
            }
            let judge_model = self.filter.judge_model.as_deref().map(str::trim).unwrap_or("");
            if judge_model.is_empty() {
                anyhow::bail!("filter.judge_model is required when filter.judge_url is set");
            }
        }
        self.filter.judge_kind()?;
        if self.filter.judge_timeout_secs == 0 || self.filter.judge_timeout_secs > 300 {
            anyhow::bail!("filter.judge_timeout_secs must be in 1..=300");
        }

        // Validate scrape settings
        if self.scrape.enabled && self.scrape.interval_hours == 0 {
            anyhow::bail!("scrape.interval_hours must be > 0 when the catch-up scraper is enabled");
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
    fn test_chat_config_default() {
        let config = ChatConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.endpoint_url, "http://127.0.0.1:8088");
        assert_eq!(config.request_timeout_secs, 90);
        assert!(config.respond_to_bots);
        assert_eq!(config.max_bot_chain, 3);
        assert_eq!(config.reply_context_max_chars, 300);
        assert_eq!(config.recent_context_messages, 12);
        assert_eq!(config.context_message_max_chars, 400);
        assert_eq!(config.min_seconds_between_replies, 2);
        assert!(config.web_search_enabled);
        assert_eq!(config.web_search_max_results, 4);
        assert_eq!(config.web_search_timeout_secs, 12);
    }

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert!(config.automod.spam_enabled);
        assert!(!config.ai.enabled);
        assert!(!config.chat.enabled);
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
    fn test_config_parse_chat() {
        let toml_content = r#"
            [chat]
            enabled = true
            endpoint_url = "http://10.0.0.5:9000"
            request_timeout_secs = 15
        "#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.chat.enabled);
        assert_eq!(config.chat.endpoint_url, "http://10.0.0.5:9000");
        assert_eq!(config.chat.request_timeout_secs, 15);
        // Keys absent from the TOML fall back to serde defaults.
        assert!(config.chat.respond_to_bots);
        assert_eq!(config.chat.max_bot_chain, 3);
        assert_eq!(config.chat.reply_context_max_chars, 300);
        assert!(config.chat.web_search_enabled);
        assert_eq!(config.chat.web_search_max_results, 4);
    }

    #[test]
    fn test_config_parse_chat_bot_keys() {
        let toml_content = r#"
            [chat]
            respond_to_bots = false
            max_bot_chain = 7
            reply_context_max_chars = 120
        "#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(!config.chat.respond_to_bots);
        assert_eq!(config.chat.max_bot_chain, 7);
        assert_eq!(config.chat.reply_context_max_chars, 120);
    }

    #[test]
    fn test_scrape_config_default() {
        let config = ScrapeConfig::default();
        assert!(config.enabled);
        assert_eq!(config.interval_hours, 24);
    }

    #[test]
    fn test_config_parse_scrape() {
        let toml_content = r#"
            [scrape]
            enabled = false
            interval_hours = 6
        "#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(!config.scrape.enabled);
        assert_eq!(config.scrape.interval_hours, 6);
        // Absent section falls back to defaults.
        let config: Config = toml::from_str("").unwrap();
        assert!(config.scrape.enabled);
        assert_eq!(config.scrape.interval_hours, 24);
    }

    #[test]
    fn test_config_validate_scrape_zero_interval() {
        let mut config = Config::default();
        config.scrape.enabled = true;
        config.scrape.interval_hours = 0;
        assert!(config.validate().is_err());
        config.scrape.enabled = false;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validate_defaults() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validate_chat_zero_timeout() {
        let mut config = Config::default();
        config.chat.enabled = true;
        config.chat.request_timeout_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_chat_empty_endpoint() {
        let mut config = Config::default();
        config.chat.enabled = true;
        config.chat.endpoint_url = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_chat_unconditional_when_disabled() {
        // enabled=false must NOT let a garbage endpoint slip through: `!ai on`
        // could activate it at runtime.
        let mut config = Config::default();
        config.chat.enabled = false;
        config.chat.endpoint_url = "   ".to_string();
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.chat.enabled = false;
        config.chat.request_timeout_secs = 0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.chat.enabled = false;
        config.chat.min_seconds_between_replies = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_web_search_bounds() {
        let mut config = Config::default();
        config.chat.web_search_max_results = 0;
        assert!(config.validate().is_err());
        config.chat.web_search_max_results = 4;
        config.chat.web_search_timeout_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_filter_config_default() {
        let config = FilterConfig::default();
        assert!(config.enabled);
        assert!(config.judge_url.is_none());
        assert!(config.judge_model.is_none());
        assert_eq!(config.judge_kind, "guard");
        assert_eq!(config.judge_timeout_secs, 45);
        assert!(config.words_file.is_none());
    }

    #[test]
    fn test_config_parse_filter() {
        let toml_content = r#"
            [filter]
            enabled = false
            judge_url = "http://127.0.0.1:11434/v1/chat/completions"
            judge_model = "llama-guard3:1b"
        "#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(!config.filter.enabled);
        assert_eq!(
            config.filter.judge_url.as_deref(),
            Some("http://127.0.0.1:11434/v1/chat/completions")
        );
        assert_eq!(config.filter.judge_model.as_deref(), Some("llama-guard3:1b"));
        assert!(config.validate().is_ok());
        // Absent section falls back to defaults (filter ON, no judge).
        let config: Config = toml::from_str("").unwrap();
        assert!(config.filter.enabled);
        assert!(config.filter.judge_url.is_none());
    }

    #[test]
    fn test_config_validate_filter_judge() {
        // judge_url without a model name is a config error.
        let mut config = Config::default();
        config.filter.judge_url = Some("http://127.0.0.1:11434/v1/chat/completions".into());
        assert!(config.validate().is_err());
        config.filter.judge_model = Some("llama-guard3:1b".into());
        assert!(config.validate().is_ok());
        // Non-http URL is a config error.
        config.filter.judge_url = Some("ollama:11434".into());
        assert!(config.validate().is_err());
        // Unknown judge kind is a config error even without a judge_url.
        let mut config = Config::default();
        config.filter.judge_kind = "vibes".into();
        assert!(config.validate().is_err());
        // Timeout bounds.
        let mut config = Config::default();
        config.filter.judge_timeout_secs = 0;
        assert!(config.validate().is_err());
        config.filter.judge_timeout_secs = 301;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_load_parse_error_is_fatal() {
        // Write a syntactically broken TOML to a temp file; load must return an
        // error rather than silently falling back to defaults.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "discord_bot_bad_config_{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "this is = = not valid toml [[[").unwrap();
        let result = Config::load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_config_load_type_error_is_fatal() {
        // Wrong type for a field is a parse error too (must not boot defaults).
        let mut path = std::env::temp_dir();
        path.push(format!(
            "discord_bot_type_config_{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "[chat]\nmax_bot_chain = \"lots\"\n").unwrap();
        let result = Config::load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }
}
