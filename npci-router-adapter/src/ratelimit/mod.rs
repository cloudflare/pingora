// Rate limiting module

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use pingora_limits::rate::Rate;
use once_cell::sync::Lazy;

/// Rate limiter implementation
pub struct RateLimiter {
    /// Per-entity rate limiters
    limiters: Arc<DashMap<String, Arc<Rate>>>,

    /// Default rate limit (requests per second)
    default_rps: isize,

    /// Window duration
    window: Duration,

    /// Algorithm type
    algorithm: RateLimitAlgorithm,
}

/// Rate limiting algorithm
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateLimitAlgorithm {
    /// Token bucket algorithm
    TokenBucket,

    /// Sliding window
    SlidingWindow,

    /// Fixed window
    FixedWindow,
}

impl RateLimiter {
    /// Create a new rate limiter
    pub fn new(default_rps: isize, window_secs: u64, algorithm: RateLimitAlgorithm) -> Self {
        Self {
            limiters: Arc::new(DashMap::new()),
            default_rps,
            window: Duration::from_secs(window_secs),
            algorithm,
        }
    }

    /// Check if request is allowed for entity
    pub fn check_rate_limit(&self, entity_id: &str, custom_rps: Option<isize>) -> bool {
        let rps = custom_rps.unwrap_or(self.default_rps);

        let limiter = self
            .limiters
            .entry(entity_id.to_string())
            .or_insert_with(|| Arc::new(Rate::new(self.window)))
            .clone();

        let current_count = limiter.observe(entity_id, 1);

        log::debug!(
            "Rate limit check for {}: current={}, limit={}",
            entity_id,
            current_count,
            rps
        );

        current_count <= rps
    }

    /// Get current request count for entity
    pub fn get_current_count(&self, entity_id: &str) -> isize {
        if let Some(limiter) = self.limiters.get(entity_id) {
            limiter.observe(entity_id, 0)
        } else {
            0
        }
    }

    /// Reset rate limit for entity
    pub fn reset(&self, entity_id: &str) {
        self.limiters.remove(entity_id);
    }

    /// Reset all rate limits
    pub fn reset_all(&self) {
        self.limiters.clear();
    }

    /// Get rate limit stats for entity
    pub fn get_stats(&self, entity_id: &str) -> RateLimitStats {
        let current = self.get_current_count(entity_id);
        let limit = self.default_rps;

        RateLimitStats {
            entity_id: entity_id.to_string(),
            current_count: current,
            limit,
            remaining: (limit - current).max(0),
            reset_time: Instant::now() + self.window,
        }
    }
}

/// Rate limit statistics
#[derive(Debug, Clone)]
pub struct RateLimitStats {
    /// Entity ID
    pub entity_id: String,

    /// Current request count in window
    pub current_count: isize,

    /// Rate limit
    pub limit: isize,

    /// Remaining requests
    pub remaining: isize,

    /// When the window resets
    pub reset_time: Instant,
}

/// Token bucket rate limiter (more sophisticated)
pub struct TokenBucketLimiter {
    buckets: Arc<DashMap<String, TokenBucket>>,
}

struct TokenBucket {
    tokens: parking_lot::Mutex<f64>,
    capacity: f64,
    refill_rate: f64, // tokens per second
    last_refill: parking_lot::Mutex<Instant>,
}

impl TokenBucket {
    fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            tokens: parking_lot::Mutex::new(capacity),
            capacity,
            refill_rate,
            last_refill: parking_lot::Mutex::new(Instant::now()),
        }
    }

    fn try_consume(&self, tokens: f64) -> bool {
        // Refill tokens based on time elapsed
        let now = Instant::now();
        let mut last_refill = self.last_refill.lock();
        let elapsed = now.duration_since(*last_refill).as_secs_f64();

        let mut current_tokens = self.tokens.lock();
        *current_tokens = (*current_tokens + elapsed * self.refill_rate).min(self.capacity);
        *last_refill = now;

        // Try to consume tokens
        if *current_tokens >= tokens {
            *current_tokens -= tokens;
            true
        } else {
            false
        }
    }

    fn get_tokens(&self) -> f64 {
        *self.tokens.lock()
    }
}

impl TokenBucketLimiter {
    /// Create a new token bucket limiter
    pub fn new() -> Self {
        Self {
            buckets: Arc::new(DashMap::new()),
        }
    }

    /// Check if request is allowed
    pub fn check_limit(&self, entity_id: &str, capacity: f64, refill_rate: f64) -> bool {
        let bucket = self
            .buckets
            .entry(entity_id.to_string())
            .or_insert_with(|| TokenBucket::new(capacity, refill_rate))
            .clone();

        bucket.try_consume(1.0)
    }

    /// Get current token count
    pub fn get_tokens(&self, entity_id: &str) -> Option<f64> {
        self.buckets.get(entity_id).map(|b| b.get_tokens())
    }

    /// Reset bucket for entity
    pub fn reset(&self, entity_id: &str) {
        self.buckets.remove(entity_id);
    }
}

impl Default for TokenBucketLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Sliding window rate limiter
pub struct SlidingWindowLimiter {
    windows: Arc<DashMap<String, SlidingWindow>>,
}

struct SlidingWindow {
    requests: parking_lot::Mutex<Vec<Instant>>,
    window_size: Duration,
    max_requests: usize,
}

impl SlidingWindow {
    fn new(window_size: Duration, max_requests: usize) -> Self {
        Self {
            requests: parking_lot::Mutex::new(Vec::new()),
            window_size,
            max_requests,
        }
    }

    fn check_and_add(&self) -> bool {
        let now = Instant::now();
        let mut requests = self.requests.lock();

        // Remove old requests outside the window
        requests.retain(|&req_time| now.duration_since(req_time) < self.window_size);

        // Check if we can add a new request
        if requests.len() < self.max_requests {
            requests.push(now);
            true
        } else {
            false
        }
    }

    fn get_count(&self) -> usize {
        let now = Instant::now();
        let mut requests = self.requests.lock();
        requests.retain(|&req_time| now.duration_since(req_time) < self.window_size);
        requests.len()
    }
}

impl SlidingWindowLimiter {
    /// Create a new sliding window limiter
    pub fn new() -> Self {
        Self {
            windows: Arc::new(DashMap::new()),
        }
    }

    /// Check if request is allowed
    pub fn check_limit(&self, entity_id: &str, window_secs: u64, max_requests: usize) -> bool {
        let window = self
            .windows
            .entry(entity_id.to_string())
            .or_insert_with(|| SlidingWindow::new(Duration::from_secs(window_secs), max_requests))
            .clone();

        window.check_and_add()
    }

    /// Get current request count
    pub fn get_count(&self, entity_id: &str) -> usize {
        self.windows
            .get(entity_id)
            .map(|w| w.get_count())
            .unwrap_or(0)
    }

    /// Reset window for entity
    pub fn reset(&self, entity_id: &str) {
        self.windows.remove(entity_id);
    }
}

impl Default for SlidingWindowLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_rate_limiter() {
        let limiter = RateLimiter::new(5, 1, RateLimitAlgorithm::TokenBucket);

        // Should allow first 5 requests
        for i in 0..5 {
            assert!(
                limiter.check_rate_limit("test", None),
                "Request {} should be allowed",
                i
            );
        }

        // Should deny 6th request
        assert!(
            !limiter.check_rate_limit("test", None),
            "Request 6 should be denied"
        );
    }

    #[test]
    fn test_custom_rate_limit() {
        let limiter = RateLimiter::new(100, 1, RateLimitAlgorithm::TokenBucket);

        // Use custom limit of 2
        assert!(limiter.check_rate_limit("test", Some(2)));
        assert!(limiter.check_rate_limit("test", Some(2)));
        assert!(!limiter.check_rate_limit("test", Some(2)));
    }

    #[test]
    fn test_token_bucket() {
        let limiter = TokenBucketLimiter::new();

        // Capacity: 5, refill: 1 per second
        assert!(limiter.check_limit("test", 5.0, 1.0));
        assert!(limiter.check_limit("test", 5.0, 1.0));

        // Wait a bit and check refill
        thread::sleep(Duration::from_millis(500));
        let tokens = limiter.get_tokens("test").unwrap();
        assert!(tokens > 3.0 && tokens < 4.0);
    }

    #[test]
    fn test_sliding_window() {
        let limiter = SlidingWindowLimiter::new();

        // Max 3 requests per second
        assert!(limiter.check_limit("test", 1, 3));
        assert!(limiter.check_limit("test", 1, 3));
        assert!(limiter.check_limit("test", 1, 3));
        assert!(!limiter.check_limit("test", 1, 3));

        assert_eq!(limiter.get_count("test"), 3);
    }

    #[test]
    fn test_reset() {
        let limiter = RateLimiter::new(2, 1, RateLimitAlgorithm::TokenBucket);

        limiter.check_rate_limit("test", None);
        limiter.check_rate_limit("test", None);
        assert!(!limiter.check_rate_limit("test", None));

        limiter.reset("test");
        assert!(limiter.check_rate_limit("test", None));
    }
}
