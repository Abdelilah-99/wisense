//! Delivering messages to chat services: errors that say whether a retry
//! may help, retrying with backoff, and keeping secrets out of error text.

use std::time::Duration;

use tracing::debug;

use crate::mqtt::Backoff;

/// How many times a message is attempted before giving up.
pub const MAX_ATTEMPTS: u32 = 5;

#[derive(Debug)]
pub enum ApiError {
    /// Worth retrying (network, rate limit, server error), after `after` if
    /// the service said how long to wait.
    Retry {
        after: Option<Duration>,
        reason: String,
    },
    /// Retrying will not help (bad request, missing permission).
    Fatal(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Retry { reason, .. } | ApiError::Fatal(reason) => f.write_str(reason),
        }
    }
}

impl ApiError {
    /// Classifies an HTTP failure: 429 and 5xx are retried.
    pub fn from_status(status: u16, after: Option<Duration>, reason: String) -> ApiError {
        if status == 429 || (500..600).contains(&status) {
            ApiError::Retry { after, reason }
        } else {
            ApiError::Fatal(reason)
        }
    }
}

/// An error's text without its URL: chat service URLs contain the bot
/// token or webhook secret, which must never reach a log line.
pub fn scrub(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

/// Runs `attempt` until it succeeds, fails for good, or [`MAX_ATTEMPTS`].
pub async fn with_retry<F, Fut>(mut attempt: F) -> Result<(), ApiError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), ApiError>>,
{
    let mut backoff = Backoff::new(Duration::from_secs(30));
    let mut tries = 0;
    loop {
        tries += 1;
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(ApiError::Retry { after, reason }) if tries < MAX_ATTEMPTS => {
                let delay = after.unwrap_or_else(|| backoff.next_delay());
                debug!(%reason, retry_in = ?delay, "delivery failed; retrying");
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn classifies_statuses() {
        assert!(matches!(
            ApiError::from_status(429, None, "x".into()),
            ApiError::Retry { .. }
        ));
        assert!(matches!(
            ApiError::from_status(502, None, "x".into()),
            ApiError::Retry { .. }
        ));
        assert!(matches!(
            ApiError::from_status(403, None, "x".into()),
            ApiError::Fatal(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_transient_failures_then_gives_up() {
        let calls = AtomicU32::new(0);
        let result = with_retry(|| async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(ApiError::Retry {
                    after: Some(Duration::from_millis(10)),
                    reason: "busy".into(),
                })
            } else {
                Ok(())
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let calls = AtomicU32::new(0);
        let result = with_retry(|| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ApiError::Retry {
                after: None,
                reason: "down".into(),
            })
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS);

        let calls = AtomicU32::new(0);
        let result = with_retry(|| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ApiError::Fatal("forbidden".into()))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
