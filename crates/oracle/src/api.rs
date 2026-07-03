use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};
use tokio::time::sleep;
use uuid::Uuid;

use crate::{
    admin,
    config::Config,
    db::Store,
    images::{extract_image_urls, extract_video_urls, normalize_image_url, normalize_video_url},
    metrics::Metrics,
    queue::{AnalysisJob, Queue, DEFAULT_STREAM_MAXLEN},
    types::{
        BatchCheckRequest, BatchEvent, CheckRequest, SubmitRequest, TargetType, Verdict,
        VerdictResponse, VerdictStatus,
    },
    websocket::ws_handler,
};

use nostr_sdk::prelude::{PublicKey, ToBech32};

const DEFAULT_WAIT_TIMEOUT_SECONDS: u64 = 30;
const MAX_WAIT_TIMEOUT_SECONDS: u64 = 60;
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Store,
    pub queue: Queue,
    pub metrics: Arc<Metrics>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(admin::router())
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/check", post(check))
        .route("/v1/submit", post(submit))
        .route("/v1/check_batch", post(check_batch))
        .route("/v1/event/:event_id", get(get_event))
        .route("/v1/image/:sha256", get(get_image))
        .route("/v1/video/:sha256", get(get_video))
        .route("/v1/npubs/nsfw", get(get_nsfw_authors))
        .route("/v1/npubs/csam", get(get_csam_authors))
        .route("/v1/ws", get(ws_handler))
        .layer(middleware::from_fn_with_state(state.clone(), require_api_key))
        .layer(cors_layer(&state.config))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn cors_layer(config: &Config) -> CorsLayer {
    let mut layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::HeaderName::from_static("x-api-key")])
        .expose_headers([header::CONTENT_TYPE]);

    if !config.allowed_origins.is_empty() {
        let origins = config
            .allowed_origins
            .iter()
            .filter_map(|origin| HeaderValue::from_str(origin).ok())
            .collect::<Vec<_>>();
        if !origins.is_empty() {
            layer = layer.allow_origin(AllowOrigin::list(origins));
        }
    }

    layer
}

async fn require_api_key(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, ApiError> {
    if !requires_public_api_key(request.uri().path()) || state.config.api_keys.is_empty() {
        return Ok(next.run(request).await);
    }

    let supplied_key = api_key_from_request(&request);
    if supplied_key
        .as_deref()
        .is_some_and(|key| state.config.api_keys.iter().any(|known| known == key))
    {
        Ok(next.run(request).await)
    } else {
        Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or invalid API key".to_string(),
        })
    }
}

fn requires_public_api_key(path: &str) -> bool {
    path.starts_with("/v1/") || path == "/metrics"
}

fn api_key_from_request(request: &Request<axum::body::Body>) -> Option<String> {
    request
        .headers()
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| bearer_token(request))
        .or_else(|| query_api_key(request.uri().query()))
}

fn bearer_token(request: &Request<axum::body::Body>) -> Option<String> {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn query_api_key(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(key, value)| (key == "api_key" && !value.is_empty()).then(|| value.to_string()))
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

async fn check(
    State(state): State<AppState>,
    Json(req): Json<CheckRequest>,
) -> Result<Json<VerdictResponse>, ApiError> {
    admin::check_rate_limit(&state, "public:check", "RATE_LIMIT_CHECKS_PER_MINUTE", 120).await?;
    let wait = req.wait;
    let timeout_seconds = req.timeout_seconds;
    let event = BatchEvent {
        event_id: req.event_id,
        pubkey: normalized_optional_pubkey(req.pubkey.as_deref())?,
        image_urls: req.image_urls,
        video_urls: req.video_urls,
    };
    let mut verdict = check_or_enqueue(&state, &event).await?;
    if wait
        && verdict.status == VerdictStatus::Unknown
        && (!event.image_urls.is_empty() || !event.video_urls.is_empty())
    {
        verdict = wait_for_event_verdict(&state, &event.event_id, timeout_seconds).await?;
    }
    Ok(Json(VerdictResponse::from_verdict(event.event_id, &verdict)))
}

async fn wait_for_event_verdict(
    state: &AppState,
    event_id: &str,
    timeout_seconds: Option<u64>,
) -> Result<Verdict, ApiError> {
    let timeout = Duration::from_secs(
        timeout_seconds
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECONDS)
            .clamp(1, MAX_WAIT_TIMEOUT_SECONDS),
    );
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(mut verdict) = state.store.latest_verdict(TargetType::Event, event_id).await? {
            if verdict.status != VerdictStatus::Unknown {
                verdict.cache = true;
                return Ok(verdict);
            }
        }

        if Instant::now() >= deadline {
            return Ok(Verdict::unknown(TargetType::Event, event_id.to_string()));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        sleep(WAIT_POLL_INTERVAL.min(remaining)).await;
    }
}

async fn submit(
    State(state): State<AppState>,
    Json(req): Json<SubmitRequest>,
) -> Result<Json<Vec<VerdictResponse>>, ApiError> {
    admin::check_rate_limit(&state, "public:submit", "RATE_LIMIT_CHECKS_PER_MINUTE", 120).await?;
    let raw_event = req.raw_event;
    let event_id = req
        .event_id
        .or_else(|| raw_event.as_ref().and_then(|event| event.get("id")).and_then(Value::as_str).map(ToString::to_string))
        .unwrap_or_else(|| "manual-submit".to_string());
    let mut image_urls = req.image_urls;
    let mut video_urls = req.video_urls;
    if let Some(raw_event) = raw_event {
        let request_pubkey = normalized_optional_pubkey(req.pubkey.as_deref())?;
        store_raw_event(state.store.pool(), &event_id, &raw_event, request_pubkey.as_deref()).await?;
        if let Some(verdict) = text_verdict_from_raw_event(&event_id, &raw_event) {
            state.store.store_verdict(&verdict).await?;
        }
        let (extracted_images, extracted_videos) = extract_urls_from_raw_event(&raw_event);
        image_urls.extend(extracted_images);
        video_urls.extend(extracted_videos);
        let event = BatchEvent {
            event_id,
            pubkey: request_pubkey.or_else(|| raw_event_pubkey(&raw_event).and_then(|pubkey| normalized_pubkey(pubkey).ok())),
            image_urls,
            video_urls,
        };
        let verdict = check_or_enqueue(&state, &event).await?;
        return Ok(Json(vec![VerdictResponse::from_verdict(event.event_id, &verdict)]));
    }
    let pubkey = normalized_optional_pubkey(req.pubkey.as_deref())?;
    let event = BatchEvent {
        event_id,
        pubkey,
        image_urls,
        video_urls,
    };
    let verdict = check_or_enqueue(&state, &event).await?;
    Ok(Json(vec![VerdictResponse::from_verdict(event.event_id, &verdict)]))
}

async fn check_batch(
    State(state): State<AppState>,
    Json(req): Json<BatchCheckRequest>,
) -> Result<Json<Vec<VerdictResponse>>, ApiError> {
    admin::check_rate_limit(&state, "public:check_batch", "RATE_LIMIT_CHECKS_PER_MINUTE", 120).await?;
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

async fn get_video(State(state): State<AppState>, Path(sha256): Path<String>) -> Result<Json<Verdict>, ApiError> {
    let verdict = state
        .store
        .latest_verdict(TargetType::Video, &sha256)
        .await?
        .unwrap_or_else(|| Verdict::unknown(TargetType::Video, sha256));
    Ok(Json(verdict))
}

pub async fn check_or_enqueue(state: &AppState, event: &BatchEvent) -> Result<Verdict, ApiError> {
    let cached_event_verdict = state.store.latest_verdict(TargetType::Event, &event.event_id).await?;
    let image_urls = event
        .image_urls
        .iter()
        .map(|url| normalize_image_url(url))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    let video_urls = event
        .video_urls
        .iter()
        .map(|url| normalize_video_url(url))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    if event.pubkey.is_some() {
        store_event_shell(state.store.pool(), &event.event_id, event.pubkey.as_deref()).await?;
    }

    if let Some(mut verdict) = cached_event_verdict.clone() {
        verdict.cache = true;
        state.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        if image_urls.is_empty() && video_urls.is_empty() {
            return Ok(verdict);
        }
    } else {
        state.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    if !image_urls.is_empty() || !video_urls.is_empty() {
        record_analysis_jobs(state.store.pool(), &event.event_id, &image_urls, &video_urls).await?;
        state
            .queue
            .enqueue(
                &AnalysisJob {
                    event_id: event.event_id.clone(),
                    pubkey: event.pubkey.clone(),
                    image_urls,
                    video_urls,
                    image_sha256: None,
                    force_recheck: false,
                    image_only: false,
                },
                queue_stream_maxlen(state).await,
            )
            .await?;
        state.metrics.queued_jobs.fetch_add(1, Ordering::Relaxed);
    }

    if let Some(mut verdict) = cached_event_verdict {
        verdict.cache = true;
        return Ok(verdict);
    }

    Ok(Verdict::unknown(TargetType::Event, event.event_id.clone()))
}

#[derive(Debug, serde::Deserialize)]
struct AuthorListQuery {
    limit: Option<i64>,
}

#[derive(Debug, serde::Serialize)]
struct AuthorListResponse {
    label: String,
    authors: Vec<AuthorListEntry>,
}

#[derive(Debug, serde::Serialize)]
struct AuthorListEntry {
    pubkey: String,
    npub: Option<String>,
    event_count: i64,
    last_seen_at: chrono::DateTime<chrono::Utc>,
    event_ids: Vec<String>,
}

async fn get_nsfw_authors(
    State(state): State<AppState>,
    Query(query): Query<AuthorListQuery>,
) -> Result<Json<AuthorListResponse>, ApiError> {
    author_list(&state, "nsfw", &["nsfw", "nudity", "sexual", "sexualised"], query.limit).await
}

async fn get_csam_authors(
    State(state): State<AppState>,
    Query(query): Query<AuthorListQuery>,
) -> Result<Json<AuthorListResponse>, ApiError> {
    author_list(&state, "csam", &["csam-suspected"], query.limit).await
}

async fn author_list(
    state: &AppState,
    label: &str,
    labels: &[&str],
    limit: Option<i64>,
) -> Result<Json<AuthorListResponse>, ApiError> {
    let Some(pool) = state.store.pool() else {
        return Ok(Json(AuthorListResponse {
            label: label.to_string(),
            authors: Vec::new(),
        }));
    };
    let labels = labels.iter().map(|label| label.to_string()).collect::<Vec<_>>();
    let limit = limit.unwrap_or(1000).clamp(1, 10_000);
    let rows = sqlx::query(
        r#"
        with latest_event_verdicts as (
          select distinct on (target_id)
            target_id as event_id, labels, created_at
          from verdicts
          where target_type = 'event'
          order by target_id, created_at desc
        )
        select e.pubkey,
               count(*)::bigint as event_count,
               max(v.created_at) as last_seen_at,
               array_agg(e.id order by v.created_at desc) as event_ids
        from events e
        join latest_event_verdicts v on v.event_id = e.id
        where e.pubkey is not null
          and e.pubkey <> ''
          and v.labels ?| $1::text[]
        group by e.pubkey
        order by last_seen_at desc
        limit $2
        "#,
    )
    .bind(labels)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;

    let authors = rows
        .into_iter()
        .map(|row| {
            let pubkey: String = row.try_get("pubkey")?;
            let event_ids: Vec<String> = row.try_get("event_ids")?;
            Ok(AuthorListEntry {
                npub: npub_for_pubkey(&pubkey),
                pubkey,
                event_count: row.try_get("event_count")?,
                last_seen_at: row.try_get("last_seen_at")?,
                event_ids: event_ids.into_iter().take(5).collect(),
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
        .map_err(anyhow::Error::from)?;

    Ok(Json(AuthorListResponse {
        label: label.to_string(),
        authors,
    }))
}

async fn record_analysis_jobs(
    pool: Option<&PgPool>,
    event_id: &str,
    image_urls: &[String],
    video_urls: &[String],
) -> Result<(), ApiError> {
    let Some(pool) = pool else {
        return Ok(());
    };
    ensure_analysis_jobs_schema(pool).await.map_err(anyhow::Error::from)?;
    for (media_type, url) in image_urls
        .iter()
        .map(|url| ("image", url))
        .chain(video_urls.iter().map(|url| ("video", url)))
    {
        sqlx::query(
            r#"
            insert into analysis_jobs
              (job_key, event_id, url, media_type, image_sha256, status, last_error, queued_at, started_at, finished_at, updated_at)
            values ($1, $2, $3, $4, null, 'queued', null, now(), null, null, now())
            on conflict (job_key) do update set
              media_type = excluded.media_type,
              status = 'queued',
              last_error = null,
              queued_at = now(),
              started_at = null,
              finished_at = null,
              updated_at = now()
            "#,
        )
        .bind(analysis_job_key(event_id, url))
        .bind(event_id)
        .bind(url)
        .bind(media_type)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
        sqlx::query("select pg_notify('aedos_media', $1)")
            .bind(url)
            .execute(pool)
            .await
            .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

async fn ensure_analysis_jobs_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        create table if not exists analysis_jobs (
          job_key text primary key,
          event_id text not null,
          url text not null,
          media_type text not null default 'image',
          image_sha256 text,
          status text not null,
          last_error text,
          queued_at timestamptz not null default now(),
          started_at timestamptz,
          finished_at timestamptz,
          updated_at timestamptz not null default now()
        )
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query("alter table analysis_jobs add column if not exists media_type text not null default 'image'")
        .execute(pool)
        .await?;
    Ok(())
}

fn analysis_job_key(event_id: &str, url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(event_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(url.as_bytes());
    format!("{:x}", hasher.finalize())
}

async fn queue_stream_maxlen(state: &AppState) -> usize {
    state
        .store
        .admin_setting_value("QUEUE_STREAM_MAXLEN")
        .await
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok())
        .or_else(|| {
            std::env::var("QUEUE_STREAM_MAXLEN")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(DEFAULT_STREAM_MAXLEN)
}

fn extract_urls_from_raw_event(raw_event: &Value) -> (Vec<String>, Vec<String>) {
    let Some(content) = raw_event
        .get("content")
        .and_then(Value::as_str)
    else {
        return (Vec::new(), Vec::new());
    };
    (extract_image_urls(content), extract_video_urls(content))
}

async fn store_raw_event(
    pool: Option<&PgPool>,
    event_id: &str,
    raw_event: &Value,
    fallback_pubkey: Option<&str>,
) -> Result<(), ApiError> {
    let Some(pool) = pool else {
        return Ok(());
    };
    let pubkey = fallback_pubkey
        .map(ToString::to_string)
        .or_else(|| raw_event_pubkey(raw_event).and_then(|pubkey| normalized_pubkey(pubkey).ok()));
    let kind = raw_event.get("kind").and_then(Value::as_i64).map(|kind| kind as i32);
    let content = raw_event.get("content").and_then(Value::as_str).unwrap_or_default();
    let created_at = raw_event
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    sqlx::query(
        r#"
        insert into events (id, pubkey, kind, content, raw, created_at)
        values ($1, $2, $3, $4, $5, $6)
        on conflict (id) do update set
          pubkey = coalesce(excluded.pubkey, events.pubkey),
          kind = coalesce(excluded.kind, events.kind),
          content = excluded.content,
          raw = excluded.raw,
          created_at = excluded.created_at
        "#,
    )
    .bind(event_id)
    .bind(pubkey.as_deref())
    .bind(kind)
    .bind(content)
    .bind(raw_event)
    .bind(created_at)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

async fn store_event_shell(pool: Option<&PgPool>, event_id: &str, pubkey: Option<&str>) -> Result<(), ApiError> {
    let Some(pool) = pool else {
        return Ok(());
    };
    sqlx::query(
        r#"
        insert into events (id, pubkey, content, raw, created_at)
        values ($1, $2, '', '{}'::jsonb, extract(epoch from now())::bigint)
        on conflict (id) do update set
          pubkey = coalesce(excluded.pubkey, events.pubkey)
        "#,
    )
    .bind(event_id)
    .bind(pubkey)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

fn text_verdict_from_raw_event(event_id: &str, raw_event: &Value) -> Option<Verdict> {
    let tokens = text_review_tokens(raw_event);
    let csam_matches = matched_terms(&tokens, CSAM_TEXT_MARKERS);
    if !csam_matches.is_empty() {
        return Some(text_verdict(
            event_id,
            VerdictStatus::Block,
            vec!["csam-suspected".to_string()],
            0.95,
            format!("high-risk text/tag marker(s): {}", csam_matches.join(", ")),
        ));
    }

    let nsfw_matches = matched_terms(&tokens, NSFW_TEXT_MARKERS);
    if !nsfw_matches.is_empty() {
        return Some(text_verdict(
            event_id,
            VerdictStatus::Warn,
            vec!["nsfw".to_string(), "sexual".to_string()],
            0.85,
            format!("NSFW text/tag marker(s): {}", nsfw_matches.join(", ")),
        ));
    }

    None
}

const CSAM_TEXT_MARKERS: &[&str] = &[
    "csam",
    "pedo",
    "paedo",
    "p3do",
    "loli",
    "lolicon",
    "shota",
    "toddler",
];

const NSFW_TEXT_MARKERS: &[&str] = &[
    "nsfw",
    "porn",
    "porno",
    "xxx",
    "nude",
    "nudes",
    "nudity",
    "sex",
    "sexual",
];

fn text_review_tokens(raw_event: &Value) -> Vec<String> {
    let mut tokens = Vec::new();
    if let Some(content) = raw_event.get("content").and_then(Value::as_str) {
        tokens.extend(extract_hashtags(content));
    }
    if let Some(tags) = raw_event.get("tags").and_then(Value::as_array) {
        for tag in tags {
            let Some(parts) = tag.as_array() else {
                continue;
            };
            let Some(tag_name) = parts.first().and_then(Value::as_str) else {
                continue;
            };
            if tag_name.eq_ignore_ascii_case("t") {
                if let Some(value) = parts.get(1).and_then(Value::as_str) {
                    tokens.push(normalize_text_marker(value));
                }
            }
        }
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

fn extract_hashtags(content: &str) -> Vec<String> {
    content
        .split_whitespace()
        .filter_map(|word| word.strip_prefix('#'))
        .map(normalize_text_marker)
        .filter(|word| !word.is_empty())
        .collect()
}

fn normalize_text_marker(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('#')
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn matched_terms(tokens: &[String], markers: &[&str]) -> Vec<String> {
    tokens
        .iter()
        .filter(|token| markers.contains(&token.as_str()))
        .cloned()
        .collect()
}

fn text_verdict(
    event_id: &str,
    status: VerdictStatus,
    labels: Vec<String>,
    confidence: f32,
    explanation: String,
) -> Verdict {
    let safe = matches!(status, VerdictStatus::Safe);
    let warn = matches!(status, VerdictStatus::Warn);
    let block = matches!(status, VerdictStatus::Block);
    let unknown = matches!(status, VerdictStatus::Unknown);
    let error = matches!(status, VerdictStatus::Error);
    Verdict {
        id: Uuid::new_v4(),
        target_type: TargetType::Event,
        target_id: event_id.to_string(),
        status,
        safe,
        warn,
        block,
        unknown,
        error,
        labels,
        confidence,
        source: "text_rule_detector".to_string(),
        cache: false,
        model_version: Some("text-rules-v1".to_string()),
        explanation: Some(explanation),
    }
}

fn npub_for_pubkey(pubkey: &str) -> Option<String> {
    PublicKey::parse(pubkey)
        .ok()
        .and_then(|public_key| public_key.to_bech32().ok())
}

fn normalized_optional_pubkey(pubkey: Option<&str>) -> Result<Option<String>, ApiError> {
    pubkey
        .map(normalized_pubkey)
        .transpose()
}

fn normalized_pubkey(pubkey: &str) -> Result<String, ApiError> {
    PublicKey::parse(pubkey)
        .map(|public_key| public_key.to_string())
        .map_err(|_| ApiError::bad_request("pubkey/npub is invalid".to_string()))
}

fn raw_event_pubkey(raw_event: &Value) -> Option<&str> {
    raw_event.get("pubkey").and_then(Value::as_str)
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

impl From<admin::AdminError> for ApiError {
    fn from(value: admin::AdminError) -> Self {
        Self {
            status: value.status,
            message: value.message,
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
                allowed_origins: vec!["http://localhost:3000".to_string()],
                secure_cookies: false,
                enable_label_publisher: false,
                label_publish_interval_seconds: 10,
            }),
            store: Store::memory(),
            queue: Queue::memory(),
            metrics: Arc::new(Metrics::default()),
        }
    }

    fn test_state_with_api_key() -> AppState {
        let mut state = test_state();
        Arc::get_mut(&mut state.config).unwrap().api_keys = vec!["secret-test-key".to_string()];
        state
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
    async fn api_key_is_required_when_configured() {
        let response = router(test_state_with_api_key())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"event_id":"abc"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_key_allows_public_request() {
        let response = router(test_state_with_api_key())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .header("x-api-key", "secret-test-key")
                    .body(Body::from(r#"{"event_id":"abc"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
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
                pubkey: None,
                image_urls: vec![],
                video_urls: vec![],
            },
        )
        .await
        .unwrap();

        assert!(verdict.cache);
        assert_eq!(verdict.status, crate::types::VerdictStatus::Safe);
    }

    #[tokio::test]
    async fn wait_for_event_verdict_returns_when_worker_stores_result() {
        let state = test_state();
        let store = state.store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            store
                .store_verdict(&Verdict::safe(TargetType::Event, "waited-event", "test"))
                .await
                .unwrap();
        });

        let verdict = wait_for_event_verdict(&state, "waited-event", Some(1))
            .await
            .unwrap();

        assert!(verdict.cache);
        assert_eq!(verdict.status, VerdictStatus::Safe);
        assert_eq!(verdict.confidence, 1.0);
    }

    #[test]
    fn text_detector_blocks_high_risk_nostr_tags() {
        let raw_event = json!({
            "id": "text-event",
            "content": "ordinary text",
            "tags": [["t", "loli"], ["t", "news"]]
        });

        let verdict = text_verdict_from_raw_event("text-event", &raw_event).unwrap();

        assert_eq!(verdict.status, VerdictStatus::Block);
        assert_eq!(verdict.labels, vec!["csam-suspected"]);
        assert!(verdict.explanation.unwrap().contains("loli"));
    }

    #[test]
    fn text_detector_warns_nsfw_hashtags() {
        let raw_event = json!({
            "id": "text-event",
            "content": "look #nsfw",
            "tags": []
        });

        let verdict = text_verdict_from_raw_event("text-event", &raw_event).unwrap();

        assert_eq!(verdict.status, VerdictStatus::Warn);
        assert!(verdict.labels.contains(&"nsfw".to_string()));
    }

    #[test]
    fn text_detector_does_not_match_plain_words_without_hashtag_or_tag() {
        let raw_event = json!({
            "id": "text-event",
            "content": "a normal toddler milestone note",
            "tags": []
        });

        assert!(text_verdict_from_raw_event("text-event", &raw_event).is_none());
    }

    #[tokio::test]
    async fn submit_raw_event_stores_text_verdict() {
        let state = test_state();
        let store = state.store.clone();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/submit")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r##"{"raw_event":{"id":"text-event","pubkey":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":1,"content":"hello #csam","tags":[],"created_at":1}}"##,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let verdict = store
            .latest_verdict(TargetType::Event, "text-event")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(verdict.status, VerdictStatus::Block);
        assert_eq!(verdict.labels, vec!["csam-suspected"]);
    }

    #[tokio::test]
    async fn check_accepts_optional_npub_without_media() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"event_id":"author-only","npub":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
