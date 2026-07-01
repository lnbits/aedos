use std::{sync::Arc, sync::atomic::Ordering};

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use crate::{
    config::Config,
    db::Store,
    images::{extract_image_urls, normalize_image_url},
    metrics::Metrics,
    queue::{AnalysisJob, Queue},
    types::{BatchCheckRequest, BatchEvent, CheckRequest, SubmitRequest, TargetType, Verdict, VerdictResponse},
    websocket::ws_handler,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Store,
    pub queue: Queue,
    pub metrics: Arc<Metrics>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/check", post(check))
        .route("/v1/submit", post(submit))
        .route("/v1/check_batch", post(check_batch))
        .route("/v1/event/:event_id", get(get_event))
        .route("/v1/image/:sha256", get(get_image))
        .route("/v1/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "config": state.config.public_summary(),
    }))
}

async fn metrics(State(state): State<AppState>) -> String {
    state.metrics.render_prometheus()
}

async fn check(State(state): State<AppState>, Json(req): Json<CheckRequest>) -> Result<Json<VerdictResponse>, ApiError> {
    let event = BatchEvent {
        event_id: req.event_id,
        image_urls: req.image_urls,
    };
    let verdict = check_or_enqueue(&state, &event).await?;
    Ok(Json(VerdictResponse::from_verdict(event.event_id, &verdict)))
}

async fn submit(State(state): State<AppState>, Json(req): Json<SubmitRequest>) -> Result<Json<Vec<VerdictResponse>>, ApiError> {
    let event_id = req.event_id.unwrap_or_else(|| "manual-submit".to_string());
    let mut image_urls = req.image_urls;
    if let Some(raw_event) = req.raw_event {
        image_urls.extend(extract_urls_from_raw_event(&raw_event));
    }
    let event = BatchEvent {
        event_id,
        image_urls,
    };
    let verdict = check_or_enqueue(&state, &event).await?;
    Ok(Json(vec![VerdictResponse::from_verdict(event.event_id, &verdict)]))
}

async fn check_batch(
    State(state): State<AppState>,
    Json(req): Json<BatchCheckRequest>,
) -> Result<Json<Vec<VerdictResponse>>, ApiError> {
    let mut responses = Vec::with_capacity(req.events.len());
    for event in req.events {
        let verdict = check_or_enqueue(&state, &event).await?;
        responses.push(VerdictResponse::from_verdict(event.event_id, &verdict));
    }
    Ok(Json(responses))
}

async fn get_event(State(state): State<AppState>, Path(event_id): Path<String>) -> Result<Json<Verdict>, ApiError> {
    let verdict = state
        .store
        .latest_verdict(TargetType::Event, &event_id)
        .await?
        .unwrap_or_else(|| Verdict::unknown(TargetType::Event, event_id));
    Ok(Json(verdict))
}

async fn get_image(State(state): State<AppState>, Path(sha256): Path<String>) -> Result<Json<Verdict>, ApiError> {
    let verdict = state
        .store
        .latest_verdict(TargetType::Image, &sha256)
        .await?
        .unwrap_or_else(|| Verdict::unknown(TargetType::Image, sha256));
    Ok(Json(verdict))
}

pub async fn check_or_enqueue(state: &AppState, event: &BatchEvent) -> Result<Verdict, ApiError> {
    if let Some(mut verdict) = state.store.latest_verdict(TargetType::Event, &event.event_id).await? {
        verdict.cache = true;
        state.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        return Ok(verdict);
    }

    state.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);
    let image_urls = event
        .image_urls
        .iter()
        .map(|url| normalize_image_url(url))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    if !image_urls.is_empty() {
        state
            .queue
            .enqueue(&AnalysisJob {
                event_id: event.event_id.clone(),
                image_urls,
            })
            .await?;
        state.metrics.queued_jobs.fetch_add(1, Ordering::Relaxed);
    }

    Ok(Verdict::unknown(TargetType::Event, event.event_id.clone()))
}

fn extract_urls_from_raw_event(raw_event: &Value) -> Vec<String> {
    raw_event
        .get("content")
        .and_then(Value::as_str)
        .map(extract_image_urls)
        .unwrap_or_default()
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: value.to_string(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState {
            config: Arc::new(Config {
                database_url: None,
                redis_url: None,
                nostr_private_key: None,
                nostr_relays: vec![],
                public_base_url: None,
                label_namespace: "nostr.com/moderation".to_string(),
                default_policy: "blur_unknown".to_string(),
                enable_escalation: false,
                max_image_bytes: 10_000_000,
                image_fetch_timeout: std::time::Duration::from_secs(10),
                worker_concurrency: 4,
                http_bind: "127.0.0.1:0".parse().unwrap(),
                oracle_verdict_kind: 31494,
                api_keys: vec![],
            }),
            store: Store::memory(),
            queue: Queue::memory(),
            metrics: Arc::new(Metrics::default()),
        }
    }

    #[tokio::test]
    async fn check_returns_unknown_and_queues_for_valid_unknown_image() {
        let app = router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"event_id":"abc","image_urls":["https://example.com/a.png"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn check_blocks_ssrf_urls() {
        let app = router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"event_id":"abc","image_urls":["http://127.0.0.1/a.png"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cache_hit_returns_cached_verdict() {
        let state = test_state();
        state
            .store
            .store_verdict(&Verdict::safe(TargetType::Event, "cached-event", "test"))
            .await
            .unwrap();

        let verdict = check_or_enqueue(
            &state,
            &BatchEvent {
                event_id: "cached-event".to_string(),
                image_urls: vec![],
            },
        )
        .await
        .unwrap();

        assert!(verdict.cache);
        assert_eq!(verdict.status, crate::types::VerdictStatus::Safe);
    }
}
