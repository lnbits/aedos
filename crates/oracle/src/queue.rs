use anyhow::Result;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::types::{BatchEvent, Verdict};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalysisJob {
    pub event_id: String,
    pub image_urls: Vec<String>,
}

impl From<BatchEvent> for AnalysisJob {
    fn from(value: BatchEvent) -> Self {
        Self {
            event_id: value.event_id,
            image_urls: value.image_urls,
        }
    }
}

#[derive(Clone)]
pub struct Queue {
    redis: Option<redis::Client>,
    verdict_tx: broadcast::Sender<Verdict>,
}

impl Queue {
    pub fn new(redis_url: Option<&str>) -> Result<Self> {
        let redis = match redis_url {
            Some(url) => Some(redis::Client::open(url)?),
            None => None,
        };
        let (verdict_tx, _) = broadcast::channel(1024);
        Ok(Self { redis, verdict_tx })
    }

    pub fn memory() -> Self {
        let (verdict_tx, _) = broadcast::channel(1024);
        Self { redis: None, verdict_tx }
    }

    pub async fn enqueue(&self, job: &AnalysisJob) -> Result<()> {
        if let Some(client) = &self.redis {
            let mut conn = client.get_multiplexed_async_connection().await?;
            let payload = serde_json::to_string(job)?;
            let _: () = conn.rpush("oracle:analysis", payload).await?;
        }
        Ok(())
    }

    pub fn subscribe_verdicts(&self) -> broadcast::Receiver<Verdict> {
        self.verdict_tx.subscribe()
    }

    pub fn publish_local_verdict(&self, verdict: Verdict) {
        let _ = self.verdict_tx.send(verdict);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{TargetType, Verdict};

    #[tokio::test]
    async fn local_verdicts_can_be_broadcast_to_subscribers() {
        let queue = Queue::memory();
        let mut rx = queue.subscribe_verdicts();
        queue.publish_local_verdict(Verdict::safe(TargetType::Event, "event", "test"));

        let verdict = rx.recv().await.unwrap();
        assert_eq!(verdict.target_id, "event");
    }
}
