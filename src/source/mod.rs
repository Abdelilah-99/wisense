//! Sample sources. Each one runs in its own task and pushes into a shared
//! channel; the detector keeps separate state per `link_id`.

#[cfg(feature = "esp32")]
pub mod esp32;
#[cfg(feature = "nl80211")]
pub mod nl80211;
pub mod procfs;
pub mod replay;
pub mod synthetic;

use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::sample::Sample;

#[async_trait]
pub trait SampleSource: Send {
    fn link_id(&self) -> &str;
    /// The next sample, or `None` at the end of the stream.
    async fn next(&mut self) -> anyhow::Result<Option<Sample>>;
}

/// Forward samples from `source` into `tx` until the stream ends, the source
/// fails, or the receiver goes away. Meant to run in its own task.
pub async fn run(
    mut source: Box<dyn SampleSource>,
    tx: mpsc::Sender<Sample>,
) -> anyhow::Result<()> {
    loop {
        let sample = source
            .next()
            .await
            .with_context(|| format!("source {}", source.link_id()))?;
        let Some(sample) = sample else { return Ok(()) };
        if tx.send(sample).await.is_err() {
            // Receiver dropped: we are shutting down.
            return Ok(());
        }
    }
}
