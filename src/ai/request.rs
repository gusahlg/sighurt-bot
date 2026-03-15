//! AI Request and Response Types
//!
//! Defines the data structures for AI moderation requests and responses.
//! Designed to be memory-efficient for constrained environments.

use tokio::sync::oneshot;
use twilight_model::id::{marker::GuildMarker, Id};

/// Types of AI requests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRequestKind {
    /// Content moderation check
    Moderation,
    /// Sentiment analysis
    Sentiment,
    /// Spam detection (AI-enhanced)
    SpamDetection,
    /// Custom analysis
    Custom,
}

/// An AI processing request
pub struct AiRequest {
    /// Guild ID for context
    pub guild_id: Id<GuildMarker>,
    /// Content to analyze
    pub content: String,
    /// Type of request
    pub kind: AiRequestKind,
    /// Callback channel for the response
    pub callback: Option<oneshot::Sender<AiResponse>>,
}

impl AiRequest {
    /// Create a new moderation request
    pub fn moderation(guild_id: Id<GuildMarker>, content: String) -> Self {
        Self {
            guild_id,
            content,
            kind: AiRequestKind::Moderation,
            callback: None,
        }
    }

    /// Create a new sentiment analysis request
    pub fn sentiment(guild_id: Id<GuildMarker>, content: String) -> Self {
        Self {
            guild_id,
            content,
            kind: AiRequestKind::Sentiment,
            callback: None,
        }
    }

    /// Create a new spam detection request
    pub fn spam_detection(guild_id: Id<GuildMarker>, content: String) -> Self {
        Self {
            guild_id,
            content,
            kind: AiRequestKind::SpamDetection,
            callback: None,
        }
    }

    /// Create a custom analysis request
    pub fn custom(guild_id: Id<GuildMarker>, content: String) -> Self {
        Self {
            guild_id,
            content,
            kind: AiRequestKind::Custom,
            callback: None,
        }
    }

    /// Get content length (useful for rate limiting)
    pub fn content_len(&self) -> usize {
        self.content.len()
    }
}

impl std::fmt::Debug for AiRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiRequest")
            .field("guild_id", &self.guild_id)
            .field("content_len", &self.content.len())
            .field("kind", &self.kind)
            .field("has_callback", &self.callback.is_some())
            .finish()
    }
}

/// Response from AI processing
#[derive(Debug, Clone)]
pub struct AiResponse {
    /// Whether the content should be moderated
    pub should_moderate: bool,
    /// Confidence score (0.0 to 1.0)
    pub confidence: f32,
    /// Reason for moderation (if applicable)
    pub reason: Option<String>,
    /// Category of detected issue
    pub category: Option<String>,
    /// Suggested actions
    pub suggestions: Vec<AiSuggestion>,
}

impl Default for AiResponse {
    fn default() -> Self {
        Self {
            should_moderate: false,
            confidence: 0.0,
            reason: None,
            category: None,
            suggestions: Vec::new(),
        }
    }
}

impl AiResponse {
    /// Create a clean (no moderation needed) response
    pub fn clean() -> Self {
        Self::default()
    }

    /// Create a flagged response
    pub fn flagged(confidence: f32, reason: String, category: String) -> Self {
        Self {
            should_moderate: true,
            confidence,
            reason: Some(reason),
            category: Some(category),
            suggestions: Vec::new(),
        }
    }

    /// Check if the response is high confidence
    pub fn is_high_confidence(&self) -> bool {
        self.confidence >= 0.8
    }

    /// Check if the response is medium confidence
    pub fn is_medium_confidence(&self) -> bool {
        self.confidence >= 0.5 && self.confidence < 0.8
    }

    /// Check if the response is low confidence
    pub fn is_low_confidence(&self) -> bool {
        self.confidence < 0.5
    }
}

/// Suggested action from AI
#[derive(Debug, Clone)]
pub struct AiSuggestion {
    /// Action type
    pub action: SuggestedAction,
    /// Additional context
    pub context: Option<String>,
}

/// Types of suggested actions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestedAction {
    /// No action needed
    None,
    /// Warn the user
    Warn,
    /// Delete the message
    Delete,
    /// Timeout the user
    Timeout,
    /// Flag for manual review
    Review,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ai_request_moderation() {
        let request = AiRequest::moderation(Id::new(123), "test content".to_string());
        assert_eq!(request.guild_id, Id::new(123));
        assert_eq!(request.content, "test content");
        assert_eq!(request.kind, AiRequestKind::Moderation);
        assert!(request.callback.is_none());
    }

    #[test]
    fn test_ai_request_sentiment() {
        let request = AiRequest::sentiment(Id::new(456), "happy message".to_string());
        assert_eq!(request.kind, AiRequestKind::Sentiment);
    }

    #[test]
    fn test_ai_request_spam_detection() {
        let request = AiRequest::spam_detection(Id::new(789), "buy now!!!".to_string());
        assert_eq!(request.kind, AiRequestKind::SpamDetection);
    }

    #[test]
    fn test_ai_request_custom() {
        let request = AiRequest::custom(Id::new(1), "custom analysis".to_string());
        assert_eq!(request.kind, AiRequestKind::Custom);
    }

    #[test]
    fn test_ai_request_content_len() {
        let request = AiRequest::moderation(Id::new(1), "hello".to_string());
        assert_eq!(request.content_len(), 5);
    }

    #[test]
    fn test_ai_request_debug() {
        let request = AiRequest::moderation(Id::new(1), "test".to_string());
        let debug = format!("{:?}", request);
        assert!(debug.contains("AiRequest"));
        assert!(debug.contains("content_len"));
        // Should not contain actual content for privacy
    }

    #[test]
    fn test_ai_response_default() {
        let response = AiResponse::default();
        assert!(!response.should_moderate);
        assert_eq!(response.confidence, 0.0);
        assert!(response.reason.is_none());
        assert!(response.category.is_none());
        assert!(response.suggestions.is_empty());
    }

    #[test]
    fn test_ai_response_clean() {
        let response = AiResponse::clean();
        assert!(!response.should_moderate);
    }

    #[test]
    fn test_ai_response_flagged() {
        let response = AiResponse::flagged(0.95, "Toxic content".to_string(), "toxicity".to_string());
        assert!(response.should_moderate);
        assert_eq!(response.confidence, 0.95);
        assert_eq!(response.reason, Some("Toxic content".to_string()));
        assert_eq!(response.category, Some("toxicity".to_string()));
    }

    #[test]
    fn test_ai_response_confidence_levels() {
        let high = AiResponse {
            confidence: 0.9,
            ..Default::default()
        };
        assert!(high.is_high_confidence());
        assert!(!high.is_medium_confidence());
        assert!(!high.is_low_confidence());

        let medium = AiResponse {
            confidence: 0.6,
            ..Default::default()
        };
        assert!(!medium.is_high_confidence());
        assert!(medium.is_medium_confidence());
        assert!(!medium.is_low_confidence());

        let low = AiResponse {
            confidence: 0.3,
            ..Default::default()
        };
        assert!(!low.is_high_confidence());
        assert!(!low.is_medium_confidence());
        assert!(low.is_low_confidence());
    }

    #[test]
    fn test_ai_suggestion() {
        let suggestion = AiSuggestion {
            action: SuggestedAction::Warn,
            context: Some("First offense".to_string()),
        };
        assert_eq!(suggestion.action, SuggestedAction::Warn);
        assert_eq!(suggestion.context, Some("First offense".to_string()));
    }

    #[test]
    fn test_suggested_action_variants() {
        assert_eq!(SuggestedAction::None, SuggestedAction::None);
        assert_ne!(SuggestedAction::Warn, SuggestedAction::Delete);
    }

    #[test]
    fn test_ai_request_kind_variants() {
        assert_eq!(AiRequestKind::Moderation, AiRequestKind::Moderation);
        assert_ne!(AiRequestKind::Moderation, AiRequestKind::Sentiment);
    }
}
