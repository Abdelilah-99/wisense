//! Fetching pictures from Frigate's HTTP API (on loopback; see
//! docker-compose.yml).

use std::time::Duration;

use crate::notify::safe_path_segment;

#[derive(Debug, Clone)]
pub struct FrigateClient {
    http: reqwest::Client,
    base: String,
}

impl FrigateClient {
    pub fn new(url: &str) -> anyhow::Result<FrigateClient> {
        Ok(FrigateClient {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            base: url.trim_end_matches('/').to_string(),
        })
    }

    /// The snapshot of a Frigate event (what alerts attach).
    pub async fn event_snapshot(&self, event_id: &str) -> Result<Vec<u8>, String> {
        if !safe_path_segment(event_id) {
            return Err(format!("unexpected characters in event id {event_id:?}"));
        }
        self.jpeg(&format!("events/{event_id}/snapshot.jpg")).await
    }

    /// The camera's latest frame.
    pub async fn latest(&self, camera: &str) -> Result<Vec<u8>, String> {
        if !safe_path_segment(camera) {
            return Err(format!("unexpected characters in camera {camera:?}"));
        }
        self.jpeg(&format!("{camera}/latest.jpg")).await
    }

    async fn jpeg(&self, path: &str) -> Result<Vec<u8>, String> {
        let url = format!("{}/api/{path}", self.base);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("{url}: {}", response.status()));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return Err(format!("{url}: empty"));
        }
        Ok(bytes.to_vec())
    }
}
