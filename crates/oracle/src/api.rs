use std::{
    collections::HashSet,
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
    websocket::{firehose_ws_handler, ws_handler},
};

use nostr_sdk::prelude::{
    Client, Event, EventId, Filter, FilterOptions, FromBech32, JsonUtil, Nip19Event, PublicKey,
    RelayPoolNotification, SubscribeAutoCloseOptions, ToBech32,
};

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
        .route("/v1/ws/firehose", get(firehose_ws_handler))
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
    let api_keys = configured_api_keys(&state).await?;
    if !requires_public_api_key(request.uri().path()) || api_keys.is_empty() {
        if request.uri().path() == "/v1/ws/firehose" && api_keys.is_empty() {
            return Err(ApiError {
                status: StatusCode::UNAUTHORIZED,
                message: "firehose WebSocket requires API_KEYS to be configured".to_string(),
            });
        }
        return Ok(next.run(request).await);
    }

    let supplied_key = api_key_from_request(&request);
    if supplied_key
        .as_deref()
        .is_some_and(|key| api_keys.iter().any(|known| constant_time_eq(known.as_bytes(), key.as_bytes())))
    {
        Ok(next.run(request).await)
    } else {
        Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or invalid API key".to_string(),
        })
    }
}

async fn configured_api_keys(state: &AppState) -> Result<Vec<String>, ApiError> {
    match state
        .store
        .admin_setting_value("API_KEYS")
        .await
        .map_err(anyhow::Error::from)?
    {
        Some(value) => Ok(split_csv_setting(&value)),
        None => Ok(state.config.api_keys.clone()),
    }
}

fn split_csv_setting(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
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

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        diff |= usize::from(left_byte ^ right_byte);
    }
    diff == 0
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
    let wait = req.wait;
    let timeout_seconds = req.timeout_seconds;
    if let Some((event_id, verdict)) = cached_completed_event_verdict(&state, req.event_id.as_deref()).await? {
        return Ok(Json(VerdictResponse::from_verdict(event_id, &verdict)));
    }
    admin::check_rate_limit(&state, "public:check", "RATE_LIMIT_CHECKS_PER_MINUTE", 120).await?;
    let event = prepare_signed_event(
        &state,
        req.event_id,
        normalized_optional_pubkey(req.pubkey.as_deref())?,
        req.image_urls,
        req.video_urls,
        req.raw_event,
    )
    .await?;
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
    let event = prepare_signed_event(
        &state,
        req.event_id,
        normalized_optional_pubkey(req.pubkey.as_deref())?,
        req.image_urls,
        req.video_urls,
        req.raw_event,
    )
    .await?;
    let verdict = check_or_enqueue(&state, &event).await?;
    Ok(Json(vec![VerdictResponse::from_verdict(event.event_id, &verdict)]))
}

async fn check_batch(
    State(state): State<AppState>,
    Json(req): Json<BatchCheckRequest>,
) -> Result<Json<Vec<VerdictResponse>>, ApiError> {
    let mut responses = Vec::with_capacity(req.events.len());
    let mut has_uncached_event = false;
    for event in req.events {
        if let Some((event_id, verdict)) = cached_completed_event_verdict(&state, Some(&event.event_id)).await? {
            responses.push(VerdictResponse::from_verdict(event_id, &verdict));
            continue;
        }
        if !has_uncached_event {
            admin::check_rate_limit(&state, "public:check_batch", "RATE_LIMIT_CHECKS_PER_MINUTE", 120).await?;
            has_uncached_event = true;
        }
        let event = prepare_signed_event(
            &state,
            Some(event.event_id),
            normalized_optional_pubkey(event.pubkey.as_deref())?,
            event.image_urls,
            event.video_urls,
            event.raw_event,
        )
        .await?;
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

pub async fn prepare_signed_event(
    state: &AppState,
    event_id: Option<String>,
    pubkey: Option<String>,
    image_urls: Vec<String>,
    video_urls: Vec<String>,
    raw_event: Option<Value>,
) -> Result<BatchEvent, ApiError> {
    let raw_event = match raw_event {
        Some(raw_event) => raw_event,
        None => {
            let Some(event_id) = event_id.as_deref().filter(|value| !value.trim().is_empty()) else {
                return Err(ApiError::bad_request("signed raw_event or event_id is required".to_string()));
            };
            fetch_raw_event_from_relays(state, event_id).await?
        }
    };

    let (verified_event_id, verified_pubkey) = verified_raw_event_identity(event_id.as_deref(), &raw_event)?;
    if let Some(pubkey) = pubkey.as_deref() {
        if pubkey != verified_pubkey {
            return Err(ApiError::bad_request("supplied pubkey does not match signed event pubkey".to_string()));
        }
    }

    store_raw_event(state.store.pool(), &verified_event_id, &raw_event, Some(&verified_pubkey)).await?;
    if let Some(verdict) = text_verdict_from_raw_event(state, &verified_event_id, &raw_event).await? {
        state.store.store_verdict(&verdict).await?;
    }

    let (extracted_images, extracted_videos) = extract_urls_from_raw_event(&raw_event);
    ensure_urls_belong_to_event("image", &image_urls, &extracted_images)?;
    ensure_urls_belong_to_event("video", &video_urls, &extracted_videos)?;

    Ok(BatchEvent {
        event_id: verified_event_id,
        pubkey: Some(verified_pubkey.to_string()),
        image_urls: extracted_images,
        video_urls: extracted_videos,
        raw_event: None,
    })
}

pub(crate) async fn cached_completed_event_verdict(
    state: &AppState,
    event_id: Option<&str>,
) -> Result<Option<(String, Verdict)>, ApiError> {
    let Some(event_id) = event_id.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let event_id = normalize_event_id_reference(event_id)?;
    let Some(mut verdict) = state.store.latest_verdict(TargetType::Event, &event_id).await? else {
        return Ok(None);
    };
    if verdict.status == VerdictStatus::Unknown {
        return Ok(None);
    }
    verdict.cache = true;
    Ok(Some((event_id, verdict)))
}

pub(crate) fn normalize_event_id_reference(event_id: &str) -> Result<String, ApiError> {
    Ok(parse_event_reference(event_id)?.event_id.to_string())
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
pub struct AuthorListQuery {
    limit: Option<i64>,
    min_events: Option<i64>,
}

#[derive(Debug, serde::Serialize)]
pub struct AuthorListResponse {
    pub label: String,
    pub min_events: i64,
    pub authors: Vec<AuthorListEntry>,
}

#[derive(Debug, serde::Serialize)]
pub struct AuthorListEntry {
    pub pubkey: String,
    pub npub: Option<String>,
    pub event_count: i64,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub event_ids: Vec<String>,
}

async fn get_nsfw_authors(
    State(state): State<AppState>,
    Query(query): Query<AuthorListQuery>,
) -> Result<Json<AuthorListResponse>, ApiError> {
    author_list_json(&state, "nsfw", &["nsfw", "nudity", "sexual", "sexualised"], query.limit, query.min_events).await
}

async fn get_csam_authors(
    State(state): State<AppState>,
    Query(query): Query<AuthorListQuery>,
) -> Result<Json<AuthorListResponse>, ApiError> {
    author_list_json(&state, "csam", &["csam-suspected"], query.limit, query.min_events).await
}

async fn author_list_json(
    state: &AppState,
    label: &str,
    labels: &[&str],
    limit: Option<i64>,
    min_events: Option<i64>,
) -> Result<Json<AuthorListResponse>, ApiError> {
    Ok(Json(author_list(state, label, labels, limit, min_events).await?))
}

pub async fn author_list(
    state: &AppState,
    label: &str,
    labels: &[&str],
    limit: Option<i64>,
    min_events: Option<i64>,
) -> Result<AuthorListResponse, ApiError> {
    let min_events = min_events.unwrap_or(1).clamp(1, 10_000);
    let Some(pool) = state.store.pool() else {
        return Ok(AuthorListResponse {
            label: label.to_string(),
            min_events,
            authors: Vec::new(),
        });
    };
    ensure_events_schema(pool).await.map_err(anyhow::Error::from)?;
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
          and e.pubkey_verified = true
          and v.labels ?| $1::text[]
        group by e.pubkey
        having count(*) >= $2
        order by last_seen_at desc
        limit $3
        "#,
    )
    .bind(labels)
    .bind(min_events)
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

    Ok(AuthorListResponse {
        label: label.to_string(),
        min_events,
        authors,
    })
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

async fn ensure_events_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("alter table if exists events add column if not exists pubkey_verified boolean not null default false")
        .execute(pool)
        .await?;
    sqlx::query("create index if not exists events_verified_pubkey_idx on events (pubkey, pubkey_verified)")
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
    ensure_events_schema(pool).await.map_err(anyhow::Error::from)?;
    let verified_pubkey = verified_raw_event_pubkey(event_id, raw_event);
    let pubkey = verified_pubkey
        .clone()
        .or_else(|| fallback_pubkey.map(ToString::to_string))
        .or_else(|| raw_event_pubkey(raw_event).and_then(|pubkey| normalized_pubkey(pubkey).ok()));
    let pubkey_verified = verified_pubkey.is_some();
    let kind = raw_event.get("kind").and_then(Value::as_i64).map(|kind| kind as i32);
    let content = raw_event.get("content").and_then(Value::as_str).unwrap_or_default();
    let created_at = raw_event
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    sqlx::query(
        r#"
        insert into events (id, pubkey, pubkey_verified, kind, content, raw, created_at)
        values ($1, $2, $3, $4, $5, $6, $7)
        on conflict (id) do update set
          pubkey = case
            when excluded.pubkey_verified then excluded.pubkey
            when events.pubkey_verified then events.pubkey
            else coalesce(events.pubkey, excluded.pubkey)
          end,
          pubkey_verified = events.pubkey_verified or excluded.pubkey_verified,
          kind = coalesce(excluded.kind, events.kind),
          content = excluded.content,
          raw = excluded.raw,
          created_at = excluded.created_at
        "#,
    )
    .bind(event_id)
    .bind(pubkey.as_deref())
    .bind(pubkey_verified)
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
    ensure_events_schema(pool).await.map_err(anyhow::Error::from)?;
    sqlx::query(
        r#"
        insert into events (id, pubkey, pubkey_verified, content, raw, created_at)
        values ($1, $2, false, '', '{}'::jsonb, extract(epoch from now())::bigint)
        on conflict (id) do update set
          pubkey = case
            when events.pubkey_verified then events.pubkey
            else coalesce(events.pubkey, excluded.pubkey)
          end
        "#,
    )
    .bind(event_id)
    .bind(pubkey)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

async fn text_verdict_from_raw_event(
    state: &AppState,
    event_id: &str,
    raw_event: &Value,
) -> Result<Option<Verdict>, ApiError> {
    let csam_markers = configured_text_markers(state, "TEXT_MARKERS_CSAM", CSAM_TEXT_MARKERS).await?;
    let nsfw_markers = configured_text_markers(state, "TEXT_MARKERS_NSFW", NSFW_TEXT_MARKERS).await?;
    Ok(text_verdict_from_raw_event_with_markers(
        event_id,
        raw_event,
        &csam_markers,
        &nsfw_markers,
    ))
}

fn text_verdict_from_raw_event_with_markers(
    event_id: &str,
    raw_event: &Value,
    csam_markers: &[String],
    nsfw_markers: &[String],
) -> Option<Verdict> {
    let tokens = text_review_tokens(raw_event);
    let csam_matches = matched_terms(&tokens, csam_markers);
    if !csam_matches.is_empty() {
        return Some(text_verdict(
            event_id,
            VerdictStatus::Block,
            vec!["csam-suspected".to_string()],
            0.95,
            format!("high-risk text/tag marker(s): {}", csam_matches.join(", ")),
        ));
    }

    let nsfw_matches = matched_terms(&tokens, nsfw_markers);
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

async fn configured_text_markers(
    state: &AppState,
    key: &str,
    defaults: &[&str],
) -> Result<Vec<String>, ApiError> {
    let configured = state
        .store
        .admin_setting_value(key)
        .await
        .map_err(anyhow::Error::from)?
        .unwrap_or_default();
    let markers = parse_marker_setting(&configured);
    if markers.is_empty() {
        Ok(defaults.iter().map(|marker| (*marker).to_string()).collect())
    } else {
        Ok(markers)
    }
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
    "teen",
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

fn parse_marker_setting(value: &str) -> Vec<String> {
    let mut markers = value
        .split(|ch: char| ch == ',' || ch == '\n' || ch == '\r' || ch.is_whitespace())
        .map(normalize_text_marker)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    markers.sort();
    markers.dedup();
    markers
}

fn matched_terms(tokens: &[String], markers: &[String]) -> Vec<String> {
    tokens
        .iter()
        .filter(|token| markers.contains(token))
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

fn verified_raw_event_identity(expected_event_id: Option<&str>, raw_event: &Value) -> Result<(String, String), ApiError> {
    let event = Event::from_json(serde_json::to_string(raw_event).map_err(anyhow::Error::from)?)
        .map_err(|_| ApiError::bad_request("raw_event is not a valid Nostr event".to_string()))?;
    event
        .verify()
        .map_err(|_| ApiError::bad_request("raw_event signature is invalid".to_string()))?;
    let event_id = event.id.to_string();
    if let Some(expected) = expected_event_id {
        let expected = parse_event_reference(expected)?.event_id.to_string();
        if expected != event_id {
            return Err(ApiError::bad_request("event_id does not match signed raw_event id".to_string()));
        }
    }
    Ok((event_id, event.pubkey.to_string()))
}

fn verified_raw_event_pubkey(expected_event_id: &str, raw_event: &Value) -> Option<String> {
    let event = Event::from_json(serde_json::to_string(raw_event).ok()?).ok()?;
    event.verify().ok()?;
    (event.id.to_string() == expected_event_id).then(|| event.pubkey.to_string())
}

fn ensure_urls_belong_to_event(media_type: &str, supplied: &[String], extracted: &[String]) -> Result<(), ApiError> {
    let normalized_extracted = extracted
        .iter()
        .map(|url| normalize_media_url(media_type, url))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    for url in supplied {
        let normalized = normalize_media_url(media_type, url).map_err(|err| ApiError::bad_request(err.to_string()))?;
        if !normalized_extracted.iter().any(|known| known == &normalized) {
            return Err(ApiError::bad_request(format!(
                "{media_type}_urls must be present in the signed raw_event"
            )));
        }
    }
    Ok(())
}

fn normalize_media_url(media_type: &str, url: &str) -> Result<String, anyhow::Error> {
    if media_type == "video" {
        normalize_video_url(url)
    } else {
        normalize_image_url(url)
    }
}

async fn fetch_raw_event_from_relays(state: &AppState, event_id: &str) -> Result<Value, ApiError> {
    let event_reference = parse_event_reference(event_id)?;
    let relays = current_nostr_relays(state, &event_reference.relays).await?;
    if relays.is_empty() {
        return Err(ApiError::bad_request("no NOSTR_RELAYS configured for event lookup".to_string()));
    }

    let client = Client::default();
    for relay in &relays {
        client.add_relay(relay).await.map_err(anyhow::Error::from)?;
    }
    client.connect_with_timeout(Duration::from_secs(5)).await;
    let connected_count = client
        .relays()
        .await
        .values()
        .filter(|relay| relay.is_connected())
        .count();
    if connected_count == 0 {
        return Err(ApiError::bad_request(format!(
            "could not fetch signed event {} because no configured relays connected; tried: {}",
            event_reference.event_id,
            relays.join(", ")
        )));
    }

    let event = fetch_event_with_grace_window(&client, &relays, event_reference.event_id).await?;
    let raw_event = serde_json::from_str::<Value>(&event.as_json()).map_err(anyhow::Error::from)?;
    verified_raw_event_identity(Some(&event_reference.event_id.to_string()), &raw_event).map_err(|err| {
        ApiError::bad_request(format!(
            "fetched event {event_id} failed signed event verification: {err}"
        ))
    })?;
    Ok(raw_event)
}

async fn fetch_event_with_grace_window(client: &Client, relays: &[String], event_id: EventId) -> Result<Event, ApiError> {
    let mut notifications = client.notifications();
    let filter = Filter::new().id(event_id).limit(1);
    let options = SubscribeAutoCloseOptions::default()
        .filter(FilterOptions::WaitDurationAfterEOSE(Duration::from_secs(3)))
        .timeout(Some(Duration::from_secs(10)));
    let subscription = client
        .subscribe_to(relays.to_vec(), vec![filter], Some(options))
        .await
        .map_err(|err| {
            ApiError::bad_request(format!(
                "could not fetch signed event {event_id} from configured relays: {err}; tried: {}",
                relays.join(", ")
            ))
        })?;
    let subscription_id = subscription.val;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event {
                subscription_id: received_subscription_id,
                event,
                ..
            } = notification
            {
                if received_subscription_id == subscription_id && event.id == event_id {
                    return Some(*event);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    client.unsubscribe(subscription_id).await;
    result.ok_or_else(|| {
        ApiError::bad_request(format!(
            "could not fetch signed event {event_id} from configured relays after waiting 10s; tried: {}. If your client has the full signed event, send it as raw_event so Aedos can verify it directly.",
            relays.join(", ")
        ))
    })
}

struct EventReference {
    event_id: EventId,
    relays: Vec<String>,
}

fn parse_event_reference(event_id: &str) -> Result<EventReference, ApiError> {
    if let Ok(event_id) = EventId::parse(event_id) {
        return Ok(EventReference {
            event_id,
            relays: Vec::new(),
        });
    }
    if let Ok(nevent) = Nip19Event::from_bech32(event_id) {
        return Ok(EventReference {
            event_id: nevent.event_id,
            relays: nevent.relays,
        });
    }
    Err(ApiError::bad_request(format!(
        "event_id {event_id} is not a valid Nostr event id, note, or nevent"
    )))
}

async fn current_nostr_relays(state: &AppState, relay_hints: &[String]) -> Result<Vec<String>, ApiError> {
    let configured = state
        .store
        .admin_setting_value("NOSTR_RELAYS")
        .await
        .map_err(anyhow::Error::from)?
        .map(|value| split_csv_setting(&value))
        .filter(|relays| !relays.is_empty())
        .unwrap_or_else(|| state.config.nostr_relays.clone());
    let mut seen = HashSet::new();
    Ok(relay_hints
        .iter()
        .cloned()
        .chain(configured)
        .into_iter()
        .filter(|relay| seen.insert(relay.clone()))
        .collect())
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
    fn from(_value: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".to_string(),
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
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use nostr_sdk::prelude::{EventBuilder, Keys};
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

    fn signed_note(content: &str) -> (String, Value) {
        let keys = Keys::generate();
        let event = EventBuilder::text_note(content)
            .sign_with_keys(&keys)
            .unwrap();
        (event.id.to_string(), serde_json::from_str(&event.as_json()).unwrap())
    }

    #[tokio::test]
    async fn check_returns_unknown_and_queues_for_valid_unknown_image() {
        let (event_id, raw_event) = signed_note("https://example.com/a.png");
        let app = router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "event_id": event_id,
                            "image_urls": ["https://example.com/a.png"],
                            "raw_event": raw_event
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn check_blocks_ssrf_urls() {
        let (event_id, raw_event) = signed_note("http://127.0.0.1/a.png");
        let app = router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "event_id": event_id,
                            "image_urls": ["http://127.0.0.1/a.png"],
                            "raw_event": raw_event
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn check_rejects_media_url_not_present_in_signed_event() {
        let (event_id, raw_event) = signed_note("https://example.com/innocent.png");
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "event_id": event_id,
                            "image_urls": ["https://example.com/bad.png"],
                            "raw_event": raw_event
                        })
                        .to_string(),
                    ))
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
    async fn firehose_requires_configured_api_keys() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/ws/firehose")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_key_allows_public_request() {
        let (event_id, raw_event) = signed_note("hello from aedos");
        let response = router(test_state_with_api_key())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .header("x-api-key", "secret-test-key")
                    .body(Body::from(json!({ "event_id": event_id, "raw_event": raw_event }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn author_list_accepts_min_events_filter() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/npubs/nsfw?min_events=2&limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["label"], "nsfw");
        assert_eq!(value["min_events"], 2);
        assert_eq!(value["authors"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn constant_time_key_compare_matches_only_equal_bytes() {
        assert!(constant_time_eq(b"secret-test-key", b"secret-test-key"));
        assert!(!constant_time_eq(b"secret-test-key", b"secret-test-kex"));
        assert!(!constant_time_eq(b"secret-test-key", b"secret-test-key-extra"));
        assert!(!constant_time_eq(b"secret-test-key", b""));
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
                raw_event: None,
            },
        )
        .await
        .unwrap();

        assert!(verdict.cache);
        assert_eq!(verdict.status, crate::types::VerdictStatus::Safe);
    }

    #[tokio::test]
    async fn check_nevent_returns_cached_event_verdict_without_raw_event() {
        let state = test_state();
        let event_id = EventId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let nevent = Nip19Event::new(event_id, ["wss://relay.example"]).to_bech32().unwrap();
        state
            .store
            .store_verdict(&Verdict::safe(TargetType::Event, event_id.to_string(), "test"))
            .await
            .unwrap();

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "event_id": nevent }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["event_id"], event_id.to_string());
        assert_eq!(value["status"], "safe");
        assert_eq!(value["cache"], true);
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

        let verdict = text_verdict_from_raw_event_with_markers(
            "text-event",
            &raw_event,
            &default_marker_vec(CSAM_TEXT_MARKERS),
            &default_marker_vec(NSFW_TEXT_MARKERS),
        )
        .unwrap();

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

        let verdict = text_verdict_from_raw_event_with_markers(
            "text-event",
            &raw_event,
            &default_marker_vec(CSAM_TEXT_MARKERS),
            &default_marker_vec(NSFW_TEXT_MARKERS),
        )
        .unwrap();

        assert_eq!(verdict.status, VerdictStatus::Warn);
        assert!(verdict.labels.contains(&"nsfw".to_string()));
    }

    #[test]
    fn text_detector_warns_teen_hashtag_by_default() {
        let raw_event = json!({
            "id": "text-event",
            "content": "look #teen",
            "tags": []
        });

        let verdict = text_verdict_from_raw_event_with_markers(
            "text-event",
            &raw_event,
            &default_marker_vec(CSAM_TEXT_MARKERS),
            &default_marker_vec(NSFW_TEXT_MARKERS),
        )
        .unwrap();

        assert_eq!(verdict.status, VerdictStatus::Warn);
        assert!(verdict.explanation.unwrap().contains("teen"));
    }

    #[test]
    fn text_detector_does_not_match_plain_words_without_hashtag_or_tag() {
        let raw_event = json!({
            "id": "text-event",
            "content": "I hate all the nudity on nostr and saw a normal toddler milestone note",
            "tags": []
        });

        assert!(text_verdict_from_raw_event_with_markers(
            "text-event",
            &raw_event,
            &default_marker_vec(CSAM_TEXT_MARKERS),
            &default_marker_vec(NSFW_TEXT_MARKERS),
        )
        .is_none());
    }

    #[test]
    fn internal_api_errors_do_not_expose_source_message() {
        let error = ApiError::from(anyhow::anyhow!("sensitive internal detail"));

        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.message, "internal server error");
    }

    fn default_marker_vec(markers: &[&str]) -> Vec<String> {
        markers.iter().map(|marker| (*marker).to_string()).collect()
    }

    #[test]
    fn verified_raw_event_pubkey_accepts_signed_matching_event() {
        let keys = Keys::generate();
        let event = EventBuilder::text_note("hello from aedos")
            .sign_with_keys(&keys)
            .unwrap();
        let raw_event: Value = serde_json::from_str(&event.as_json()).unwrap();

        assert_eq!(
            verified_raw_event_pubkey(&event.id.to_string(), &raw_event),
            Some(keys.public_key().to_string())
        );
    }

    #[test]
    fn verified_raw_event_pubkey_rejects_forged_or_mismatched_event() {
        let keys = Keys::generate();
        let event = EventBuilder::text_note("hello from aedos")
            .sign_with_keys(&keys)
            .unwrap();
        let mut raw_event: Value = serde_json::from_str(&event.as_json()).unwrap();
        raw_event["pubkey"] = json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

        assert_eq!(verified_raw_event_pubkey(&event.id.to_string(), &raw_event), None);
        assert_eq!(verified_raw_event_pubkey("not-the-real-event-id", &serde_json::from_str(&event.as_json()).unwrap()), None);
    }

    #[test]
    fn parse_event_reference_accepts_nevent_and_relay_hints() {
        let event_id = EventId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let nevent = Nip19Event::new(event_id, ["wss://relay.example"]).to_bech32().unwrap();

        let parsed = parse_event_reference(&nevent).unwrap();

        assert_eq!(parsed.event_id.to_string(), event_id.to_string());
        assert_eq!(parsed.relays, vec!["wss://relay.example".to_string()]);
    }

    #[test]
    fn verified_raw_event_identity_accepts_nevent_expected_id() {
        let (event_id, raw_event) = signed_note("hello from aedos");
        let nevent = Nip19Event::new(EventId::parse(&event_id).unwrap(), ["wss://relay.example"])
            .to_bech32()
            .unwrap();

        let (verified_event_id, _) = verified_raw_event_identity(Some(&nevent), &raw_event).unwrap();

        assert_eq!(verified_event_id, event_id);
    }

    #[tokio::test]
    async fn submit_raw_event_stores_text_verdict() {
        let state = test_state();
        let store = state.store.clone();
        let app = router(state);
        let (event_id, raw_event) = signed_note("hello #csam");

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/submit")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "raw_event": raw_event }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let verdict = store
            .latest_verdict(TargetType::Event, &event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(verdict.status, VerdictStatus::Block);
        assert_eq!(verdict.labels, vec!["csam-suspected"]);
    }

    #[tokio::test]
    async fn check_accepts_optional_npub_without_media() {
        let (event_id, raw_event) = signed_note("author-only");
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/check")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "event_id": event_id, "raw_event": raw_event }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
