use std::time::Duration;
use tracing::{error, warn};

/// Policy governing reconnection attempts with exponential backoff.
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub backoff_multiplier: f64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(300), // 5 min cap
            backoff_multiplier: 2.0,
        }
    }
}

impl ReconnectPolicy {
    /// Backoff for re-establishing a live provider or brokerage stream.
    pub fn live_stream() -> Self {
        Self {
            max_attempts: u32::MAX,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            backoff_multiplier: 2.0,
        }
    }

    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let secs = self.initial_delay.as_secs_f64() * self.backoff_multiplier.powi(attempt as i32);
        Duration::from_secs_f64(secs.min(self.max_delay.as_secs_f64()))
    }

    pub fn should_retry(&self, attempt: u32) -> bool {
        attempt < self.max_attempts
    }
}

/// Classify a provider or brokerage failure for a reconnect supervisor.
pub fn is_transient_connection_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_lowercase();
    const HARD_MARKERS: [&str; 4] = [
        "authorization",
        "unsupported provider",
        "invalid provider configuration",
        "invalid brokerage configuration",
    ];
    !HARD_MARKERS.iter().any(|marker| message.contains(marker))
}

/// Reconnect loop wrapper with exponential backoff.
///
/// Calls `connect_fn` repeatedly until it returns `Ok(())` or the policy
/// exhausts all attempts.
pub async fn with_reconnect<F, Fut>(policy: &ReconnectPolicy, mut connect_fn: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let mut attempt = 0u32;
    loop {
        match connect_fn().await {
            Ok(()) => break,
            Err(e) => {
                attempt += 1;
                if !policy.should_retry(attempt) {
                    error!(
                        "Max reconnect attempts ({}) reached: {e}",
                        policy.max_attempts
                    );
                    break;
                }
                let delay = policy.delay_for_attempt(attempt);
                warn!(
                    "Connection failed (attempt {attempt}/{}): {e}. Retrying in {delay:?}",
                    policy.max_attempts
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_stream_backoff_starts_at_one_second_and_caps_at_sixty() {
        let policy = ReconnectPolicy::live_stream();
        assert_eq!(policy.delay_for_attempt(0), Duration::from_secs(1));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_secs(8));
        // Caps at the 60s ceiling no matter how long the provider stays down.
        assert_eq!(policy.delay_for_attempt(20), Duration::from_secs(60));
        assert_eq!(policy.delay_for_attempt(1000), Duration::from_secs(60));
    }

    #[test]
    fn live_stream_retries_indefinitely() {
        let policy = ReconnectPolicy::live_stream();
        assert!(policy.should_retry(10_000));
        assert!(policy.should_retry(u32::MAX - 1));
    }

    #[test]
    fn classifier_separates_transient_from_hard_failures() {
        assert!(is_transient_connection_error(&anyhow::anyhow!(
            "provider websocket closed"
        )));
        assert!(is_transient_connection_error(&anyhow::anyhow!(
            "failed to connect to provider: connection refused"
        )));
        assert!(!is_transient_connection_error(&anyhow::anyhow!(
            "invalid provider configuration"
        )));
    }
}
