//! AI Provider Trait and Implementations
//!
//! Provides a pluggable interface for different AI backends.
//! Designed for extensibility to support local models, API-based providers, etc.

use super::request::{AiRequest, AiResponse};
use anyhow::Result;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Configuration for AI providers
#[derive(Debug, Clone)]
pub enum AiProviderConfig {
    /// Mock provider for testing
    Mock,
    /// Local model provider (for future use)
    Local { model_path: String },
}

/// Unified AI provider trait using dynamic dispatch
/// This allows different providers to be used interchangeably
pub trait AiProvider: Send + Sync {
    /// Get the provider name
    fn name(&self) -> &str;

    /// Check if the provider is ready
    fn is_ready(&self) -> bool;

    /// Get estimated processing time in milliseconds
    fn estimated_latency_ms(&self) -> u32;

    /// Process a request - boxed future for trait object compatibility
    fn process<'a>(
        &'a self,
        request: &'a AiRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AiResponse>> + Send + 'a>>;
}

/// Mock provider for testing and development
pub struct MockProvider {
    request_count: AtomicU64,
    latency_ms: u32,
}

impl MockProvider {
    pub fn new() -> Self {
        Self {
            request_count: AtomicU64::new(0),
            latency_ms: 10,
        }
    }

    pub fn with_latency(latency_ms: u32) -> Self {
        Self {
            request_count: AtomicU64::new(0),
            latency_ms,
        }
    }

    pub fn request_count(&self) -> u64 {
        self.request_count.load(Ordering::Relaxed)
    }

    async fn process_request(&self, request: &AiRequest) -> Result<AiResponse> {
        self.request_count.fetch_add(1, Ordering::Relaxed);

        // Simulate processing delay
        tokio::time::sleep(Duration::from_millis(self.latency_ms as u64)).await;

        // Simple mock logic: flag messages containing "spam" or "bad"
        let content = request.content.to_lowercase();
        let should_moderate = content.contains("spam")
            || content.contains("bad")
            || content.contains("toxic");

        let reason = if should_moderate {
            Some("Mock AI flagged content".to_string())
        } else {
            None
        };

        Ok(AiResponse {
            should_moderate,
            confidence: if should_moderate { 0.85 } else { 0.1 },
            reason,
            category: if should_moderate {
                Some("mock_detection".to_string())
            } else {
                None
            },
            suggestions: Vec::new(),
        })
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AiProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn is_ready(&self) -> bool {
        true
    }

    fn estimated_latency_ms(&self) -> u32 {
        self.latency_ms
    }

    fn process<'a>(
        &'a self,
        request: &'a AiRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AiResponse>> + Send + 'a>> {
        Box::pin(self.process_request(request))
    }
}

/// Local model provider placeholder
/// This will be expanded when integrating actual local models
pub struct LocalProvider {
    model_path: String,
    ready: bool,
}

impl LocalProvider {
    pub fn new(model_path: String) -> Self {
        Self {
            model_path,
            ready: false, // Not implemented yet
        }
    }

    pub fn model_path(&self) -> &str {
        &self.model_path
    }

    async fn process_request(&self, _request: &AiRequest) -> Result<AiResponse> {
        // Placeholder - will be implemented when adding actual local model support
        anyhow::bail!("Local AI provider not yet implemented")
    }
}

impl AiProvider for LocalProvider {
    fn name(&self) -> &str {
        "local"
    }

    fn is_ready(&self) -> bool {
        self.ready
    }

    fn estimated_latency_ms(&self) -> u32 {
        100 // Estimate for local model inference
    }

    fn process<'a>(
        &'a self,
        request: &'a AiRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AiResponse>> + Send + 'a>> {
        Box::pin(self.process_request(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::request::AiRequestKind;
    use twilight_model::id::Id;

    #[test]
    fn test_mock_provider_new() {
        let provider = MockProvider::new();
        assert_eq!(provider.name(), "mock");
        assert!(provider.is_ready());
        assert_eq!(provider.request_count(), 0);
    }

    #[test]
    fn test_mock_provider_with_latency() {
        let provider = MockProvider::with_latency(50);
        assert_eq!(provider.estimated_latency_ms(), 50);
    }

    #[tokio::test]
    async fn test_mock_provider_process_clean() {
        let provider = MockProvider::new();
        let request = AiRequest {
            guild_id: Id::new(1),
            content: "Hello world".to_string(),
            kind: AiRequestKind::Moderation,
            callback: None,
        };

        let response = provider.process_request(&request).await.unwrap();
        assert!(!response.should_moderate);
        assert!(response.confidence < 0.5);
        assert_eq!(provider.request_count(), 1);
    }

    #[tokio::test]
    async fn test_mock_provider_process_flagged() {
        let provider = MockProvider::new();
        let request = AiRequest {
            guild_id: Id::new(1),
            content: "This is spam content".to_string(),
            kind: AiRequestKind::Moderation,
            callback: None,
        };

        let response = provider.process_request(&request).await.unwrap();
        assert!(response.should_moderate);
        assert!(response.confidence > 0.5);
        assert!(response.reason.is_some());
    }

    #[tokio::test]
    async fn test_mock_provider_process_toxic() {
        let provider = MockProvider::new();
        let request = AiRequest {
            guild_id: Id::new(1),
            content: "toxic behavior detected".to_string(),
            kind: AiRequestKind::Moderation,
            callback: None,
        };

        let response = provider.process_request(&request).await.unwrap();
        assert!(response.should_moderate);
    }

    #[test]
    fn test_local_provider_new() {
        let provider = LocalProvider::new("/path/to/model".to_string());
        assert_eq!(provider.name(), "local");
        assert!(!provider.is_ready()); // Not implemented
        assert_eq!(provider.model_path(), "/path/to/model");
    }

    #[tokio::test]
    async fn test_local_provider_not_implemented() {
        let provider = LocalProvider::new("/path/to/model".to_string());
        let request = AiRequest {
            guild_id: Id::new(1),
            content: "test".to_string(),
            kind: AiRequestKind::Moderation,
            callback: None,
        };

        let result = provider.process_request(&request).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_provider_config_mock() {
        let config = AiProviderConfig::Mock;
        match config {
            AiProviderConfig::Mock => {}
            _ => panic!("Expected Mock variant"),
        }
    }

    #[test]
    fn test_provider_config_local() {
        let config = AiProviderConfig::Local {
            model_path: "/model".to_string(),
        };
        match config {
            AiProviderConfig::Local { model_path } => {
                assert_eq!(model_path, "/model");
            }
            _ => panic!("Expected Local variant"),
        }
    }
}
