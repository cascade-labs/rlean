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
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let secs = self.initial_delay.as_secs_f64() * self.backoff_multiplier.powi(attempt as i32);
        Duration::from_secs_f64(secs.min(self.max_delay.as_secs_f64()))
    }

    pub fn should_retry(&self, attempt: u32) -> bool {
        attempt < self.max_attempts
    }
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
