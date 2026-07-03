mod admin;
mod api;
mod config;
mod db;
mod images;
mod labels;
mod metrics;
mod nostr;
mod queue;
mod types;
mod websocket;

use std::{sync::{atomic::Ordering, Arc}, time::Duration};

use anyhow::Result;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::{
    api::AppState,
    config::Config,
    db::Store,
    labels::build_nip32_label,
    metrics::Metrics,
    nostr::{publish_draft, PublisherConfig},
    queue::Queue,
    types::TargetType,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::from_env()?;
    let store = Store::new(config.database_url.as_deref()).await?;
    let queue = Queue::new(config.redis_url.as_deref())?;
    let bind = config.http_bind;
    let state = AppState {
        config: Arc::new(config),
        store,
        queue,
        metrics: Arc::new(Metrics::default()),
    };
    spawn_label_publisher(state.clone());

    let listener = TcpListener::bind(bind).await?;
    info!(%bind, "starting nostr label oracle");
    axum::serve(listener, api::router(state)).await?;
    Ok(())
}

fn spawn_label_publisher(state: AppState) {
    if !state.config.enable_label_publisher
        || state.config.nostr_private_key.is_none()
        || state.config.nostr_relays.is_empty()
        || state.store.pool().is_none()
    {
        return;
    }

    tokio::spawn(async move {
        let interval_seconds = state.config.label_publish_interval_seconds.max(1);
        let mut interval = tokio::time::interval(Duration::from_secs(interval_seconds));
        let publisher_config = PublisherConfig {
            private_key: state.config.nostr_private_key.clone(),
            relays: state.config.nostr_relays.clone(),
        };

        loop {
            interval.tick().await;
            let verdicts = match state.store.unpublished_event_verdicts(100).await {
                Ok(verdicts) => verdicts,
                Err(error) => {
                    warn!(%error, "failed to load unpublished label verdicts");
                    continue;
                }
            };

            for verdict in verdicts {
                let draft = build_nip32_label(
                    &state.config.label_namespace,
                    TargetType::Event,
                    &verdict.target_id,
                    &verdict,
                );
                match publish_draft(&draft, &publisher_config).await {
                    Ok(nostr_event_id) => {
                        if let Err(error) = state
                            .store
                            .store_published_label(
                                TargetType::Event,
                                &verdict.target_id,
                                nostr_event_id.as_deref(),
                                &draft,
                            )
                            .await
                        {
                            warn!(%error, target_id = %verdict.target_id, "failed to store published label");
                        } else {
                            state.metrics.published_labels.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(error) => {
                        warn!(%error, target_id = %verdict.target_id, "failed to publish NIP-32 label");
                    }
                }
            }
        }
    });
}
