//! AI Mode Module
//!
//! Provides AI-powered moderation and response capabilities.
//! Designed for efficient operation on Raspberry Pi 4.
//!
//! # Architecture
//! - Queue-based request processing to prevent overload
//! - Configurable concurrency limits
//! - Provider trait for pluggable AI backends
//! - Async-first design for non-blocking operation

mod provider;
mod request;

pub use provider::{AiProvider, AiProviderConfig, LocalProvider, MockProvider};
pub use request::{AiRequest, AiResponse};

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};
use twilight_model::id::{marker::GuildMarker, Id};

/// Default max concurrent AI requests (conservative for Pi 4)
const DEFAULT_MAX_CONCURRENT: usize = 2;

/// Default queue capacity
const DEFAULT_QUEUE_CAPACITY: usize = 100;

/// AI processor statistics
#[derive(Debug, Default)]
pub struct AiStats {
    pub requests_processed: AtomicU32,
    pub requests_queued: AtomicU32,
    pub requests_dropped: AtomicU32,
    pub avg_latency_ms: AtomicU32,
}

impl AiStats {
    pub fn snapshot(&self) -> AiStatsSnapshot {
        AiStatsSnapshot {
            requests_processed: self.requests_processed.load(Ordering::Relaxed),
            requests_queued: self.requests_queued.load(Ordering::Relaxed),
            requests_dropped: self.requests_dropped.load(Ordering::Relaxed),
            avg_latency_ms: self.avg_latency_ms.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AiStatsSnapshot {
    pub requests_processed: u32,
    pub requests_queued: u32,
    pub requests_dropped: u32,
    pub avg_latency_ms: u32,
}

/// Configuration for the AI processor
#[derive(Debug, Clone)]
pub struct AiConfig {
    /// Whether AI mode is enabled
    pub enabled: bool,
    /// Maximum concurrent AI requests
    pub max_concurrent: usize,
    /// Maximum queue size before dropping requests
    pub queue_capacity: usize,
    /// Request timeout in seconds
    pub timeout_secs: u64,
    /// Provider configuration
    pub provider: AiProviderConfig,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            timeout_secs: 30,
            provider: AiProviderConfig::Mock,
        }
    }
}

/// AI processor for handling moderation requests
pub struct AiProcessor {
    config: AiConfig,
    provider: Arc<dyn AiProvider>,
    stats: Arc<AiStats>,
    sender: mpsc::Sender<AiRequest>,
    enabled: AtomicBool,
    semaphore: Arc<Semaphore>,
}

impl AiProcessor {
    /// Create a new AI processor with the given configuration
    pub fn new(config: AiConfig) -> Self {
        let (sender, receiver) = mpsc::channel(config.queue_capacity);
        let provider: Arc<dyn AiProvider> = match &config.provider {
            AiProviderConfig::Mock => Arc::new(MockProvider::new()),
            AiProviderConfig::Local { model_path } => {
                Arc::new(LocalProvider::new(model_path.clone()))
            }
        };

        let stats = Arc::new(AiStats::default());
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent));
        let enabled = AtomicBool::new(config.enabled);

        let processor = Self {
            config: config.clone(),
            provider: Arc::clone(&provider),
            stats: Arc::clone(&stats),
            sender,
            enabled,
            semaphore: Arc::clone(&semaphore),
        };

        // Spawn the request processor
        if config.enabled {
            processor.spawn_processor(receiver, provider, stats, semaphore, config.timeout_secs);
        }

        processor
    }

    fn spawn_processor(
        &self,
        mut receiver: mpsc::Receiver<AiRequest>,
        provider: Arc<dyn AiProvider>,
        stats: Arc<AiStats>,
        semaphore: Arc<Semaphore>,
        timeout_secs: u64,
    ) {
        tokio::spawn(async move {
            while let Some(request) = receiver.recv().await {
                let provider = Arc::clone(&provider);
                let stats = Arc::clone(&stats);
                let semaphore = Arc::clone(&semaphore);
                let timeout = Duration::from_secs(timeout_secs);

                tokio::spawn(async move {
                    // Acquire semaphore permit
                    let _permit = match semaphore.acquire().await {
                        Ok(permit) => permit,
                        Err(_) => {
                            tracing::error!("AI semaphore closed, dropping request");
                            stats.requests_dropped.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                    };

                    stats.requests_queued.fetch_sub(1, Ordering::Relaxed);

                    let start = std::time::Instant::now();

                    // Process with timeout
                    let result = tokio::time::timeout(timeout, provider.process(&request)).await;

                    let elapsed = start.elapsed().as_millis() as u32;

                    match result {
                        Ok(Ok(response)) => {
                            stats.requests_processed.fetch_add(1, Ordering::Relaxed);
                            // Update rolling average latency
                            let current = stats.avg_latency_ms.load(Ordering::Relaxed);
                            let new_avg = if current == 0 {
                                elapsed
                            } else {
                                (current * 7 + elapsed) / 8
                            };
                            stats.avg_latency_ms.store(new_avg, Ordering::Relaxed);

                            // Handle the response
                            if let Some(callback) = request.callback {
                                let _ = callback.send(response);
                            }
                        }
                        Ok(Err(e)) => {
                            stats.requests_dropped.fetch_add(1, Ordering::Relaxed);
                            tracing::error!("AI request failed: {}", e);
                        }
                        Err(_) => {
                            stats.requests_dropped.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!("AI request timed out after {:?}", timeout);
                        }
                    }
                });
            }
        });
    }

    /// Check if AI mode is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Enable or disable AI mode at runtime
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Submit a request to the AI processor
    /// Returns None if the queue is full or AI is disabled
    pub async fn submit(&self, request: AiRequest) -> Option<tokio::sync::oneshot::Receiver<AiResponse>> {
        if !self.is_enabled() {
            return None;
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = AiRequest {
            callback: Some(tx),
            ..request
        };

        match self.sender.try_send(request) {
            Ok(()) => {
                self.stats.requests_queued.fetch_add(1, Ordering::Relaxed);
                Some(rx)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.stats.requests_dropped.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("AI queue full, dropping request");
                None
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::error!("AI processor channel closed");
                None
            }
        }
    }

    /// Submit a request and wait for the response
    pub async fn process(&self, request: AiRequest) -> Option<AiResponse> {
        let rx = self.submit(request).await?;
        rx.await.ok()
    }

    /// Get current statistics
    pub fn stats(&self) -> AiStatsSnapshot {
        self.stats.snapshot()
    }

    /// Check if a message should be moderated by AI.
    /// Skips empty messages and truncates content exceeding 16 KiB.
    pub async fn should_moderate(&self, content: &str, guild_id: Id<GuildMarker>) -> Option<AiResponse> {
        if !self.is_enabled() || content.is_empty() {
            return None;
        }

        // Truncate very long messages to avoid overloading AI
        const MAX_AI_CONTENT_LEN: usize = 16 * 1024;
        let content = if content.len() > MAX_AI_CONTENT_LEN {
            &content[..MAX_AI_CONTENT_LEN]
        } else {
            content
        };

        let request = AiRequest::moderation(guild_id, content.to_string());
        self.process(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ai_config_default() {
        let config = AiConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_concurrent, DEFAULT_MAX_CONCURRENT);
        assert_eq!(config.queue_capacity, DEFAULT_QUEUE_CAPACITY);
    }

    #[test]
    fn test_ai_stats_default() {
        let stats = AiStats::default();
        assert_eq!(stats.requests_processed.load(Ordering::Relaxed), 0);
        assert_eq!(stats.requests_queued.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_ai_stats_snapshot() {
        let stats = AiStats::default();
        stats.requests_processed.store(10, Ordering::Relaxed);
        stats.requests_queued.store(5, Ordering::Relaxed);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.requests_processed, 10);
        assert_eq!(snapshot.requests_queued, 5);
    }

    #[tokio::test]
    async fn test_ai_processor_disabled() {
        let config = AiConfig {
            enabled: false,
            ..Default::default()
        };
        let processor = AiProcessor::new(config);

        assert!(!processor.is_enabled());

        let request = AiRequest::moderation(Id::new(1), "test".to_string());
        let result = processor.submit(request).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_ai_processor_enabled() {
        let config = AiConfig {
            enabled: true,
            ..Default::default()
        };
        let processor = AiProcessor::new(config);

        assert!(processor.is_enabled());

        processor.set_enabled(false);
        assert!(!processor.is_enabled());
    }

    #[tokio::test]
    async fn test_ai_processor_mock_request() {
        let config = AiConfig {
            enabled: true,
            provider: AiProviderConfig::Mock,
            ..Default::default()
        };
        let processor = AiProcessor::new(config);

        let request = AiRequest::moderation(Id::new(1), "test message".to_string());
        let response = processor.process(request).await;

        assert!(response.is_some());
        let response = response.unwrap();
        assert!(!response.should_moderate);
    }

    #[tokio::test]
    async fn test_ai_processor_stats() {
        let config = AiConfig {
            enabled: true,
            provider: AiProviderConfig::Mock,
            ..Default::default()
        };
        let processor = AiProcessor::new(config);

        // Process a request
        let request = AiRequest::moderation(Id::new(1), "test".to_string());
        let _ = processor.process(request).await;

        // Give time for async processing
        tokio::time::sleep(Duration::from_millis(50)).await;

        let stats = processor.stats();
        assert!(stats.requests_processed >= 1);
    }

    #[test]
    fn test_ai_config_custom() {
        let config = AiConfig {
            enabled: true,
            max_concurrent: 4,
            queue_capacity: 200,
            timeout_secs: 60,
            provider: AiProviderConfig::Mock,
        };

        assert!(config.enabled);
        assert_eq!(config.max_concurrent, 4);
        assert_eq!(config.queue_capacity, 200);
        assert_eq!(config.timeout_secs, 60);
    }
}
