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

use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use tracing::info;

use crate::{api::AppState, config::Config, db::Store, metrics::Metrics, queue::Queue};

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

    let listener = TcpListener::bind(bind).await?;
    info!(%bind, "starting nostr label oracle");
    axum::serve(listener, api::router(state)).await?;
    Ok(())
}
