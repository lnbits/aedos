use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use password_hash::rand_core::OsRng;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use uuid::Uuid;

use crate::{api::AppState, db::status_str, types::{TargetType, Verdict, VerdictStatus}};
use crate::queue::{AnalysisJob, DEFAULT_STREAM_MAXLEN};

const SESSION_COOKIE: &str = "aedos_session";
const SESSION_TTL_SECONDS: i64 = 60 * 60 * 24 * 7;
const SECRET_MASK: &str = "********";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/setup", get(setup_status).post(setup))
        .route("/admin/api/login", post(login))
        .route("/admin/api/logout", post(logout))
        .route("/admin/api/session", get(session))
        .route("/admin/api/overview", get(overview))
        .route("/admin/api/images", get(images))
        .route("/admin/api/images/:sha256/verdict", post(change_image_verdict))
        .route("/admin/api/images/:sha256/recheck", post(recheck_image))
        .route("/admin/api/videos/:sha256/verdict", post(change_video_verdict))
        .route("/admin/api/videos/:sha256/recheck", post(recheck_video))
        .route("/admin/api/settings", get(settings).post(save_settings))
}

#[derive(Debug, Serialize)]
struct SetupStatus {
    needs_setup: bool,
}

#[derive(Debug, Deserialize)]
struct SetupRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct SessionResponse {
    authenticated: bool,
    username: Option<String>,
    needs_setup: bool,
}

#[derive(Debug, Deserialize)]
struct ImagesQuery {
    q: Option<String>,
    page: Option<i64>,
    per_page: Option<i64>,
}

#[derive(Debug, Serialize)]
struct Overview {
    total_processed: i64,
    processed_today: i64,
    average_processed_per_day: f64,
    queued_jobs: i64,
    retry_jobs: i64,
    dead_letter_jobs: i64,
    status_counts: HashMap<String, i64>,
    relays: Vec<RelayStatus>,
}

#[derive(Debug, Serialize)]
struct RelayStatus {
    url: String,
    online: bool,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImageRow {
    media_type: String,
    sha256: Option<String>,
    url: String,
    mime_type: Option<String>,
    width: Option<i32>,
    height: Option<i32>,
    bytes: Option<i32>,
    first_seen_at: chrono::DateTime<chrono::Utc>,
    status: Option<String>,
    labels: Vec<String>,
    confidence: Option<f32>,
    source: Option<String>,
    model_version: Option<String>,
    explanation: Option<String>,
    provider_response: Option<serde_json::Value>,
    verdict_created_at: Option<chrono::DateTime<chrono::Utc>>,
    job_status: Option<String>,
    job_error: Option<String>,
    job_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    event_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ImagesResponse {
    items: Vec<ImageRow>,
    total: i64,
    page: i64,
    per_page: i64,
}

#[derive(Debug, Deserialize)]
struct ChangeVerdictRequest {
    status: VerdictStatus,
    labels: Vec<String>,
    confidence: Option<f32>,
    explanation: Option<String>,
}

#[derive(Debug, Serialize)]
struct Setting {
    key: String,
    value: String,
    secret: bool,
}

#[derive(Debug, Deserialize)]
struct SaveSettingsRequest {
    settings: HashMap<String, String>,
}

#[derive(Debug)]
pub struct AdminError {
    pub status: StatusCode,
    pub message: String,
}

impl AdminError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: message.into() }
    }

    pub fn unauthorized() -> Self {
        Self { status: StatusCode::UNAUTHORIZED, message: "unauthorized".to_string() }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self { status: StatusCode::FORBIDDEN, message: message.into() }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: message.into() }
    }

    fn unavailable() -> Self {
        Self { status: StatusCode::SERVICE_UNAVAILABLE, message: "admin dashboard requires DATABASE_URL".to_string() }
    }
}

impl From<anyhow::Error> for AdminError {
    fn from(value: anyhow::Error) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: value.to_string() }
    }
}

impl From<sqlx::Error> for AdminError {
    fn from(value: sqlx::Error) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: value.to_string() }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

async fn setup_status(State(state): State<AppState>) -> Result<Json<SetupStatus>, AdminError> {
    let pool = pool(&state)?;
    ensure_admin_schema(pool).await?;
    Ok(Json(SetupStatus { needs_setup: !has_admin_user(pool).await? }))
}

async fn setup(State(state): State<AppState>, Json(req): Json<SetupRequest>) -> Result<Response, AdminError> {
    validate_username_password(&req.username, &req.password)?;
    let pool = pool(&state)?;
    ensure_admin_schema(pool).await?;
    if has_admin_user(pool).await? {
        return Err(AdminError::forbidden("admin user already exists"));
    }

    let password_hash = hash_password(&req.password)?;
    sqlx::query(
        "insert into admin_users (id, username, password_hash) values ($1, $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(req.username.trim())
    .bind(password_hash)
    .execute(pool)
    .await?;

    create_session_response(pool, req.username.trim()).await
}

async fn login(State(state): State<AppState>, Json(req): Json<LoginRequest>) -> Result<Response, AdminError> {
    let pool = pool(&state)?;
    ensure_admin_schema(pool).await?;
    let Some(row) = sqlx::query("select username, password_hash from admin_users where username = $1")
        .bind(req.username.trim())
        .fetch_optional(pool)
        .await?
    else {
        return Err(AdminError::unauthorized());
    };

    let password_hash: String = row.try_get("password_hash")?;
    if !verify_password(&password_hash, &req.password)? {
        return Err(AdminError::unauthorized());
    }

    create_session_response(pool, row.try_get::<String, _>("username")?.as_str()).await
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AdminError> {
    if let Some(pool) = state.store.pool() {
        ensure_admin_schema(pool).await?;
        if let Some(token) = session_cookie(&headers) {
            let token_hash = hash_token(&token);
            sqlx::query("delete from admin_sessions where token_hash = $1")
                .bind(token_hash)
                .execute(pool)
                .await?;
        }
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    clear_cookie(response.headers_mut());
    Ok(response)
}

async fn session(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<SessionResponse>, AdminError> {
    let pool = pool(&state)?;
    ensure_admin_schema(pool).await?;
    let needs_setup = !has_admin_user(pool).await?;
    let username = current_username(pool, &headers).await?;
    Ok(Json(SessionResponse { authenticated: username.is_some(), username, needs_setup }))
}

async fn overview(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Overview>, AdminError> {
    let pool = authed_pool(&state, &headers).await?;
    let total_processed: i64 = sqlx::query_scalar(
        "select count(distinct target_id) from verdicts where target_type = 'image' and status <> 'unknown'",
    )
    .fetch_one(pool)
    .await?;
    let processed_today: i64 = sqlx::query_scalar(
        "select count(*) from verdicts where target_type = 'image' and created_at >= date_trunc('day', now())",
    )
    .fetch_one(pool)
    .await?;
    let first_seen: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "select min(created_at) from verdicts where target_type = 'image'",
    )
    .fetch_one(pool)
    .await?;
    let days = first_seen
        .map(|first| (chrono::Utc::now() - first).num_days().max(1) as f64)
        .unwrap_or(1.0);
    let status_rows = sqlx::query(
        "select status, count(*)::bigint as count from verdicts where target_type = 'image' group by status",
    )
    .fetch_all(pool)
    .await?;
    let mut status_counts = HashMap::new();
    for row in status_rows {
        status_counts.insert(row.try_get("status")?, row.try_get("count")?);
    }
    let (queued_jobs, retry_jobs, dead_letter_jobs) = queue_counts(&state).await;
    let relays = relay_statuses(pool, &state).await;
    Ok(Json(Overview {
        total_processed,
        processed_today,
        average_processed_per_day: (total_processed as f64 / days * 10.0).round() / 10.0,
        queued_jobs,
        retry_jobs,
        dead_letter_jobs,
        status_counts,
        relays,
    }))
}

async fn images(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ImagesQuery>,
) -> Result<Json<ImagesResponse>, AdminError> {
    let pool = authed_pool(&state, &headers).await?;
    ensure_admin_schema(pool).await?;
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(25).clamp(5, 100);
    let offset = (page - 1) * per_page;
    let search = query.q.unwrap_or_default();
    let search_like = format!("%{}%", search.trim());

    let total: i64 = sqlx::query_scalar(
        r#"
        with rows as (
            select i.sha256 as row_id
            from images i
            left join event_images ei on ei.image_id = i.id
            where $1 = '' or i.sha256 ilike $2 or i.url ilike $2 or ei.event_id ilike $2
            group by i.sha256
            union all
            select v.sha256 as row_id
            from videos v
            left join event_videos ev on ev.video_id = v.id
            where $1 = '' or v.sha256 ilike $2 or v.url ilike $2 or ev.event_id ilike $2
            group by v.sha256
            union all
            select aj.job_key as row_id
            from analysis_jobs aj
            where aj.image_sha256 is null
              and ($1 = '' or aj.job_key ilike $2 or aj.url ilike $2 or aj.event_id ilike $2)
        )
        select count(*) from rows
        "#,
    )
    .bind(search.trim())
    .bind(&search_like)
    .fetch_one(pool)
    .await?;

    let rows = sqlx::query(
        r#"
        with rows as (
            select 'image'::text as media_type, i.sha256, i.url, i.mime_type, i.width, i.height, i.bytes, i.first_seen_at,
                   v.status, v.labels, v.confidence, v.source, v.model_version, v.explanation, v.provider_response,
                   v.created_at as verdict_created_at,
                   ij.status as job_status, ij.last_error as job_error, ij.updated_at as job_updated_at,
                   coalesce(array_agg(distinct ei.event_id) filter (where ei.event_id is not null), '{}') as event_ids,
                   coalesce(ij.updated_at, v.created_at, i.first_seen_at) as sort_at
            from images i
            left join event_images ei on ei.image_id = i.id
            left join image_jobs ij on ij.sha256 = i.sha256
            left join lateral (
                select status, labels, confidence, source, model_version, explanation, provider_response, created_at
                from verdicts
                where target_type = 'image' and target_id = i.sha256
                order by created_at desc
                limit 1
            ) v on true
            where $1 = '' or i.sha256 ilike $2 or i.url ilike $2 or ei.event_id ilike $2
            group by i.id, ij.status, ij.last_error, ij.updated_at, v.status, v.labels, v.confidence, v.source, v.model_version, v.explanation, v.provider_response, v.created_at
            union all
            select 'video'::text as media_type, vid.sha256, vid.url, vid.mime_type, null::integer as width, null::integer as height,
                   vid.bytes, vid.first_seen_at,
                   vv.status, vv.labels, vv.confidence, vv.source, vv.model_version, vv.explanation, vv.provider_response,
                   vv.created_at as verdict_created_at,
                   aj.status as job_status, aj.last_error as job_error, aj.updated_at as job_updated_at,
                   coalesce(array_agg(distinct ev.event_id) filter (where ev.event_id is not null), '{}') as event_ids,
                   coalesce(aj.updated_at, vv.created_at, vid.first_seen_at) as sort_at
            from videos vid
            left join event_videos ev on ev.video_id = vid.id
            left join analysis_jobs aj on aj.image_sha256 = vid.sha256 and aj.media_type = 'video'
            left join lateral (
                select status, labels, confidence, source, model_version, explanation, provider_response, created_at
                from verdicts
                where target_type = 'video' and target_id = vid.sha256
                order by created_at desc
                limit 1
            ) vv on true
            where $1 = '' or vid.sha256 ilike $2 or vid.url ilike $2 or ev.event_id ilike $2
            group by vid.id, aj.status, aj.last_error, aj.updated_at, vv.status, vv.labels, vv.confidence, vv.source, vv.model_version, vv.explanation, vv.provider_response, vv.created_at
            union all
            select coalesce(aj.media_type, 'image') as media_type, null::text as sha256, aj.url, null::text as mime_type, null::integer as width, null::integer as height,
                   null::integer as bytes, aj.queued_at as first_seen_at,
                   null::text as status, null::jsonb as labels, null::real as confidence, null::text as source,
                   null::text as model_version, null::text as explanation, null::jsonb as provider_response, null::timestamptz as verdict_created_at,
                   aj.status as job_status, aj.last_error as job_error, aj.updated_at as job_updated_at,
                   array[aj.event_id] as event_ids, aj.updated_at as sort_at
            from analysis_jobs aj
            where aj.image_sha256 is null
              and ($1 = '' or aj.job_key ilike $2 or aj.url ilike $2 or aj.event_id ilike $2)
        )
        select media_type, sha256, url, mime_type, width, height, bytes, first_seen_at,
               status, labels, confidence, source, model_version, explanation, provider_response,
               verdict_created_at, job_status, job_error, job_updated_at, event_ids
        from rows
        order by sort_at desc
        limit $3 offset $4
        "#,
    )
    .bind(search.trim())
    .bind(&search_like)
    .bind(per_page)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let labels_value: Option<serde_json::Value> = row.try_get("labels")?;
        let labels = labels_value.and_then(|value| serde_json::from_value(value).ok()).unwrap_or_default();
        items.push(ImageRow {
            media_type: row.try_get("media_type")?,
            sha256: row.try_get("sha256")?,
            url: row.try_get("url")?,
            mime_type: row.try_get("mime_type")?,
            width: row.try_get("width")?,
            height: row.try_get("height")?,
            bytes: row.try_get("bytes")?,
            first_seen_at: row.try_get("first_seen_at")?,
            status: row.try_get("status")?,
            labels,
            confidence: row.try_get("confidence")?,
            source: row.try_get("source")?,
            model_version: row.try_get("model_version")?,
            explanation: row.try_get("explanation")?,
            provider_response: row.try_get("provider_response")?,
            verdict_created_at: row.try_get("verdict_created_at")?,
            job_status: row.try_get("job_status")?,
            job_error: row.try_get("job_error")?,
            job_updated_at: row.try_get("job_updated_at")?,
            event_ids: row.try_get("event_ids")?,
        });
    }

    Ok(Json(ImagesResponse { items, total, page, per_page }))
}

async fn change_image_verdict(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(sha256): Path<String>,
    Json(req): Json<ChangeVerdictRequest>,
) -> Result<Json<serde_json::Value>, AdminError> {
    change_media_verdict(state, headers, sha256, req, TargetType::Image).await
}

async fn change_video_verdict(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(sha256): Path<String>,
    Json(req): Json<ChangeVerdictRequest>,
) -> Result<Json<serde_json::Value>, AdminError> {
    change_media_verdict(state, headers, sha256, req, TargetType::Video).await
}

async fn change_media_verdict(
    state: AppState,
    headers: HeaderMap,
    sha256: String,
    req: ChangeVerdictRequest,
    target_type: TargetType,
) -> Result<Json<serde_json::Value>, AdminError> {
    let pool = authed_pool(&state, &headers).await?;
    ensure_admin_schema(pool).await?;
    validate_sha256(&sha256)?;
    if req.labels.is_empty() || req.labels.iter().any(|label| label.len() > 64) {
        return Err(AdminError::bad_request("labels are required and must be short"));
    }
    let verdict = verdict_from_review(sha256, req, target_type);
    sqlx::query(
        r#"
        insert into verdicts
        (id, target_type, target_id, status, safe, warn, block, unknown, error, labels,
         confidence, source, cache, model_version, explanation, created_at)
        values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,now())
        "#,
    )
    .bind(verdict.id)
    .bind(verdict.target_type.as_str())
    .bind(&verdict.target_id)
    .bind(status_str(&verdict.status))
    .bind(verdict.safe)
    .bind(verdict.warn)
    .bind(verdict.block)
    .bind(verdict.unknown)
    .bind(verdict.error)
    .bind(json!(verdict.labels))
    .bind(verdict.confidence)
    .bind(&verdict.source)
    .bind(verdict.cache)
    .bind(&verdict.model_version)
    .bind(&verdict.explanation)
    .execute(pool)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn recheck_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(sha256): Path<String>,
) -> Result<Json<serde_json::Value>, AdminError> {
    let pool = authed_pool(&state, &headers).await?;
    validate_sha256(&sha256)?;
    let Some(url): Option<String> = sqlx::query_scalar("select url from images where sha256 = $1")
        .bind(&sha256)
        .fetch_optional(pool)
        .await?
    else {
        return Err(AdminError::not_found("image not found"));
    };

    let event_id = format!("admin-recheck:{}:{}", &sha256[..12], now_unix_seconds());
    sqlx::query(
        r#"
        insert into image_jobs (sha256, status, last_error, queued_at, started_at, finished_at, updated_at)
        values ($1, 'queued', null, now(), null, null, now())
        on conflict (sha256) do update set
          status = excluded.status,
          last_error = null,
          queued_at = now(),
          started_at = null,
          finished_at = null,
          updated_at = now()
        "#,
    )
    .bind(&sha256)
    .execute(pool)
    .await?;
    state
        .queue
        .enqueue(
            &AnalysisJob {
                event_id,
                pubkey: None,
                image_urls: vec![url],
                video_urls: vec![],
                image_sha256: Some(sha256),
                force_recheck: true,
                image_only: true,
            },
            admin_queue_stream_maxlen(&state).await,
        )
        .await
        .map_err(anyhow::Error::from)?;

    Ok(Json(json!({ "ok": true })))
}

async fn recheck_video(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(sha256): Path<String>,
) -> Result<Json<serde_json::Value>, AdminError> {
    let pool = authed_pool(&state, &headers).await?;
    validate_sha256(&sha256)?;
    let Some(url): Option<String> = sqlx::query_scalar("select url from videos where sha256 = $1")
        .bind(&sha256)
        .fetch_optional(pool)
        .await?
    else {
        return Err(AdminError::not_found("video not found"));
    };

    let event_id = format!("admin-recheck-video:{}:{}", &sha256[..12], now_unix_seconds());
    sqlx::query(
        r#"
        insert into analysis_jobs
          (job_key, event_id, url, media_type, image_sha256, status, last_error, queued_at, started_at, finished_at, updated_at)
        values ($1, $2, $3, 'video', $4, 'queued', null, now(), null, null, now())
        on conflict (job_key) do update set
          media_type = 'video',
          image_sha256 = excluded.image_sha256,
          status = excluded.status,
          last_error = null,
          queued_at = now(),
          started_at = null,
          finished_at = null,
          updated_at = now()
        "#,
    )
    .bind(analysis_job_key(&event_id, &url))
    .bind(&event_id)
    .bind(&url)
    .bind(&sha256)
    .execute(pool)
    .await?;
    state
        .queue
        .enqueue(
            &AnalysisJob {
                event_id,
                pubkey: None,
                image_urls: vec![],
                video_urls: vec![url],
                image_sha256: None,
                force_recheck: true,
                image_only: true,
            },
            admin_queue_stream_maxlen(&state).await,
        )
        .await
        .map_err(anyhow::Error::from)?;

    Ok(Json(json!({ "ok": true })))
}

async fn settings(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Vec<Setting>>, AdminError> {
    let pool = authed_pool(&state, &headers).await?;
    ensure_default_settings(pool, &state).await?;
    let rows = sqlx::query("select key, value, secret from admin_settings order by key")
        .fetch_all(pool)
        .await?;
    let mut settings = Vec::with_capacity(rows.len());
    for row in rows {
        let secret: bool = row.try_get("secret")?;
        let value: String = row.try_get("value")?;
        settings.push(Setting {
            key: row.try_get("key")?,
            value: if secret && !value.is_empty() { SECRET_MASK.to_string() } else { value },
            secret,
        });
    }
    Ok(Json(settings))
}

async fn save_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SaveSettingsRequest>,
) -> Result<Json<serde_json::Value>, AdminError> {
    let pool = authed_pool(&state, &headers).await?;
    ensure_default_settings(pool, &state).await?;
    let allowed = setting_specs(&state);
    let mut merged_settings = current_settings(pool).await?;
    let mut updates = Vec::new();

    for (key, value) in req.settings {
        let Some(spec) = allowed.iter().find(|spec| spec.key == key) else {
            return Err(AdminError::bad_request(format!("setting {key} is not editable")));
        };
        if spec.secret && value == SECRET_MASK {
            continue;
        }
        validate_setting_value(spec.key, &value)?;
        merged_settings.insert(key.clone(), value.clone());
        updates.push((spec.key, value, spec.secret));
    }

    if merged_settings
        .get("MODERATION_PROVIDER")
        .map(|value| value == "openai")
        .unwrap_or(false)
        && merged_settings
            .get("OPENAI_API_KEY")
            .map(|value| value.trim().is_empty())
            .unwrap_or(true)
    {
        return Err(AdminError::bad_request("OPENAI_API_KEY is required when MODERATION_PROVIDER is openai"));
    }

    for (key, value, secret) in updates {
        sqlx::query(
            r#"
            insert into admin_settings (key, value, secret, updated_at)
            values ($1, $2, $3, now())
            on conflict (key) do update set value = excluded.value, secret = excluded.secret, updated_at = now()
            "#,
        )
        .bind(key)
        .bind(value)
        .bind(secret)
        .execute(pool)
        .await?;
    }
    Ok(Json(json!({ "ok": true })))
}

fn pool(state: &AppState) -> Result<&PgPool, AdminError> {
    state.store.pool().ok_or_else(AdminError::unavailable)
}

async fn authed_pool<'a>(state: &'a AppState, headers: &HeaderMap) -> Result<&'a PgPool, AdminError> {
    let pool = pool(state)?;
    ensure_admin_schema(pool).await?;
    if current_username(pool, headers).await?.is_none() {
        return Err(AdminError::unauthorized());
    }
    Ok(pool)
}

async fn ensure_admin_schema(pool: &PgPool) -> Result<()> {
    sqlx::query("alter table if exists verdicts add column if not exists provider_response jsonb")
        .execute(pool)
        .await?;
    for statement in [
        r#"
        create table if not exists admin_users (
          id uuid primary key,
          username text not null unique,
          password_hash text not null,
          created_at timestamptz not null default now()
        )
        "#,
        r#"
        create table if not exists admin_sessions (
          token_hash text primary key,
          username text not null references admin_users(username) on delete cascade,
          expires_at timestamptz not null,
          created_at timestamptz not null default now()
        )
        "#,
        r#"
        create table if not exists admin_settings (
          key text primary key,
          value text not null,
          secret boolean not null default false,
          updated_at timestamptz not null default now()
        )
        "#,
        r#"
        create table if not exists admin_rate_limits (
          key text primary key,
          window_start bigint not null,
          count integer not null default 0
        )
        "#,
        r#"
        create table if not exists image_jobs (
          sha256 text primary key,
          status text not null,
          last_error text,
          queued_at timestamptz not null default now(),
          started_at timestamptz,
          finished_at timestamptz,
          updated_at timestamptz not null default now()
        )
        "#,
        r#"
        create table if not exists videos (
          id uuid primary key,
          url text not null,
          normalized_url text not null,
          sha256 text unique,
          mime_type text,
          bytes integer,
          first_seen_at timestamptz not null default now()
        )
        "#,
        r#"
        create table if not exists event_videos (
          event_id text not null references events(id) on delete cascade,
          video_id uuid not null references videos(id) on delete cascade,
          primary key (event_id, video_id)
        )
        "#,
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
    ] {
        sqlx::query(statement).execute(pool).await?;
    }
    sqlx::query("alter table analysis_jobs add column if not exists media_type text not null default 'image'")
        .execute(pool)
        .await?;
    Ok(())
}

async fn has_admin_user(pool: &PgPool) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("select count(*) from admin_users").fetch_one(pool).await?;
    Ok(count > 0)
}

async fn current_username(pool: &PgPool, headers: &HeaderMap) -> Result<Option<String>> {
    let Some(token) = session_cookie(headers) else {
        return Ok(None);
    };
    let token_hash = hash_token(&token);
    let row = sqlx::query(
        "select username from admin_sessions where token_hash = $1 and expires_at > now()",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| row.try_get("username")).transpose()?)
}

async fn create_session_response(pool: &PgPool, username: &str) -> Result<Response, AdminError> {
    let token = format!("{}.{}", Uuid::new_v4(), Uuid::new_v4());
    let token_hash = hash_token(&token);
    sqlx::query(
        "insert into admin_sessions (token_hash, username, expires_at) values ($1, $2, now() + interval '7 days')",
    )
    .bind(token_hash)
    .bind(username)
    .execute(pool)
    .await?;

    let mut response = Json(json!({ "ok": true, "username": username })).into_response();
    set_session_cookie(response.headers_mut(), &token)?;
    Ok(response)
}

fn validate_username_password(username: &str, password: &str) -> Result<(), AdminError> {
    let username = username.trim();
    if username.len() < 3 || username.len() > 64 || !username.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
        return Err(AdminError::bad_request("username must be 3-64 characters using letters, numbers, hyphen, or underscore"));
    }
    if password.len() < 12 {
        return Err(AdminError::bad_request("password must be at least 12 characters"));
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

fn verify_password(password_hash: &str, password: &str) -> Result<bool> {
    let parsed_hash = PasswordHash::new(password_hash).map_err(|err| anyhow::anyhow!(err.to_string()))?;
    Ok(Argon2::default().verify_password(password.as_bytes(), &parsed_hash).is_ok())
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.to_string())
    })
}

fn set_session_cookie(headers: &mut HeaderMap, token: &str) -> Result<(), AdminError> {
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Max-Age={SESSION_TTL_SECONDS}; Path=/; HttpOnly; SameSite=Strict"
    );
    headers.insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).map_err(|err| AdminError::bad_request(err.to_string()))?);
    Ok(())
}

fn clear_cookie(headers: &mut HeaderMap) {
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_static("aedos_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict"),
    );
}

async fn queue_counts(state: &AppState) -> (i64, i64, i64) {
    let Some(redis_url) = &state.config.redis_url else {
        return (0, 0, 0);
    };
    let Ok(client) = redis::Client::open(redis_url.as_str()) else {
        return (0, 0, 0);
    };
    let Ok(mut conn) = client.get_multiplexed_async_connection().await else {
        return (0, 0, 0);
    };
    let queued = redis::cmd("XLEN").arg("oracle:analysis").query_async(&mut conn).await.unwrap_or(0);
    let retry = conn.zcard("oracle:analysis:retry").await.unwrap_or(0);
    let dead = redis::cmd("XLEN").arg("oracle:analysis:dead").query_async(&mut conn).await.unwrap_or(0);
    (queued, retry, dead)
}

async fn admin_queue_stream_maxlen(state: &AppState) -> usize {
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

async fn relay_statuses(pool: &PgPool, state: &AppState) -> Vec<RelayStatus> {
    let relays = current_settings(pool)
        .await
        .ok()
        .and_then(|settings| settings.get("NOSTR_RELAYS").map(|value| csv_value(value)))
        .filter(|relays| !relays.is_empty())
        .unwrap_or_else(|| state.config.nostr_relays.clone());

    futures_util::future::join_all(relays.into_iter().map(check_relay)).await
}

async fn check_relay(url: String) -> RelayStatus {
    match url::Url::parse(&url) {
        Ok(parsed) if matches!(parsed.scheme(), "ws" | "wss") => {}
        Ok(_) => {
            return RelayStatus {
                url,
                online: false,
                error: Some("URL must start with ws:// or wss://".to_string()),
            };
        }
        Err(error) => {
            return RelayStatus {
                url,
                online: false,
                error: Some(error.to_string()),
            };
        }
    }

    match timeout(Duration::from_secs(3), connect_async(url.as_str())).await {
        Ok(Ok((_socket, _response))) => RelayStatus { url, online: true, error: None },
        Ok(Err(error)) => RelayStatus { url, online: false, error: Some(error.to_string()) },
        Err(_) => RelayStatus { url, online: false, error: Some("connection timed out".to_string()) },
    }
}

fn csv_value(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn validate_sha256(sha256: &str) -> Result<(), AdminError> {
    if sha256.len() != 64 || !sha256.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(AdminError::bad_request("invalid image sha256"));
    }
    Ok(())
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn analysis_job_key(event_id: &str, url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(event_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(url.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn verdict_from_review(sha256: String, req: ChangeVerdictRequest, target_type: TargetType) -> Verdict {
    let safe = matches!(req.status, VerdictStatus::Safe);
    let warn = matches!(req.status, VerdictStatus::Warn);
    let block = matches!(req.status, VerdictStatus::Block);
    let unknown = matches!(req.status, VerdictStatus::Unknown);
    let error = matches!(req.status, VerdictStatus::Error);
    Verdict {
        id: Uuid::new_v4(),
        target_type,
        target_id: sha256,
        status: req.status,
        safe,
        warn,
        block,
        unknown,
        error,
        labels: req.labels,
        confidence: req.confidence.unwrap_or(1.0).clamp(0.0, 1.0),
        source: "operator-review".to_string(),
        cache: false,
        model_version: None,
        explanation: req.explanation,
    }
}

struct SettingSpec {
    key: &'static str,
    value: String,
    secret: bool,
}

fn setting_specs(state: &AppState) -> Vec<SettingSpec> {
    vec![
        SettingSpec { key: "LABEL_NAMESPACE", value: state.config.label_namespace.clone(), secret: false },
        SettingSpec { key: "DEFAULT_POLICY", value: state.config.default_policy.clone(), secret: false },
        SettingSpec { key: "ENABLE_ESCALATION", value: state.config.enable_escalation.to_string(), secret: false },
        SettingSpec { key: "MAX_IMAGE_BYTES", value: state.config.max_image_bytes.to_string(), secret: false },
        SettingSpec { key: "MAX_VIDEO_BYTES", value: std::env::var("MAX_VIDEO_BYTES").unwrap_or_else(|_| "50000000".to_string()), secret: false },
        SettingSpec { key: "IMAGE_FETCH_TIMEOUT_SECONDS", value: state.config.image_fetch_timeout.as_secs().to_string(), secret: false },
        SettingSpec { key: "MAX_VIDEO_FRAMES", value: std::env::var("MAX_VIDEO_FRAMES").unwrap_or_else(|_| "8".to_string()), secret: false },
        SettingSpec { key: "VIDEO_FRAME_INTERVAL_SECONDS", value: std::env::var("VIDEO_FRAME_INTERVAL_SECONDS").unwrap_or_else(|_| "5".to_string()), secret: false },
        SettingSpec { key: "WORKER_CONCURRENCY", value: state.config.worker_concurrency.to_string(), secret: false },
        SettingSpec { key: "QUEUE_STREAM_MAXLEN", value: std::env::var("QUEUE_STREAM_MAXLEN").unwrap_or_else(|_| "1000000".to_string()), secret: false },
        SettingSpec { key: "QUEUE_DEAD_LETTER_MAXLEN", value: std::env::var("QUEUE_DEAD_LETTER_MAXLEN").unwrap_or_else(|_| "100000".to_string()), secret: false },
        SettingSpec { key: "RATE_LIMIT_CHECKS_PER_MINUTE", value: std::env::var("RATE_LIMIT_CHECKS_PER_MINUTE").unwrap_or_else(|_| "120".to_string()), secret: false },
        SettingSpec { key: "MODERATION_PROVIDER", value: std::env::var("MODERATION_PROVIDER").unwrap_or_else(|_| "deterministic".to_string()), secret: false },
        SettingSpec { key: "OPENAI_API_KEY", value: std::env::var("OPENAI_API_KEY").unwrap_or_default(), secret: true },
        SettingSpec { key: "OPENAI_MODERATION_MODEL", value: std::env::var("OPENAI_MODERATION_MODEL").unwrap_or_else(|_| "omni-moderation-latest".to_string()), secret: false },
        SettingSpec { key: "NOSTR_RELAYS", value: state.config.nostr_relays.join(","), secret: false },
        SettingSpec { key: "NOSTR_PRIVATE_KEY", value: state.config.nostr_private_key.clone().unwrap_or_default(), secret: true },
    ]
}

async fn ensure_default_settings(pool: &PgPool, state: &AppState) -> Result<()> {
    for spec in setting_specs(state) {
        sqlx::query(
            r#"
            insert into admin_settings (key, value, secret)
            values ($1, $2, $3)
            on conflict (key) do nothing
            "#,
        )
        .bind(spec.key)
        .bind(spec.value)
        .bind(spec.secret)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn current_settings(pool: &PgPool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query("select key, value from admin_settings")
        .fetch_all(pool)
        .await?;
    let mut settings = HashMap::new();
    for row in rows {
        settings.insert(row.try_get("key")?, row.try_get("value")?);
    }
    Ok(settings)
}

fn validate_setting_value(key: &str, value: &str) -> Result<(), AdminError> {
    if value.len() > 4096 {
        return Err(AdminError::bad_request("setting value is too long"));
    }
    match key {
        "ENABLE_ESCALATION" => value.parse::<bool>().map(|_| ()).map_err(|_| AdminError::bad_request("ENABLE_ESCALATION must be true or false")),
        "MODERATION_PROVIDER" if matches!(value, "deterministic" | "openai") => Ok(()),
        "MODERATION_PROVIDER" => Err(AdminError::bad_request("MODERATION_PROVIDER must be deterministic or openai")),
        "MAX_IMAGE_BYTES" | "MAX_VIDEO_BYTES" | "IMAGE_FETCH_TIMEOUT_SECONDS" | "MAX_VIDEO_FRAMES" | "VIDEO_FRAME_INTERVAL_SECONDS" | "WORKER_CONCURRENCY" | "QUEUE_STREAM_MAXLEN" | "QUEUE_DEAD_LETTER_MAXLEN" | "RATE_LIMIT_CHECKS_PER_MINUTE" => {
            value.parse::<usize>().map(|_| ()).map_err(|_| AdminError::bad_request(format!("{key} must be a positive number")))
        }
        _ => Ok(()),
    }
}

pub async fn check_rate_limit(state: &AppState, key: &str, limit_key: &str, default_limit: i64) -> Result<(), AdminError> {
    let Some(pool) = state.store.pool() else {
        return Ok(());
    };
    ensure_admin_schema(pool).await?;
    let limit = rate_limit_value(pool, limit_key, default_limit).await?;
    if limit <= 0 {
        return Ok(());
    }
    let window = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_secs() as i64
        / 60;
    let row = sqlx::query(
        r#"
        insert into admin_rate_limits (key, window_start, count)
        values ($1, $2, 1)
        on conflict (key) do update set
          count = case when admin_rate_limits.window_start = excluded.window_start then admin_rate_limits.count + 1 else 1 end,
          window_start = excluded.window_start
        returning count
        "#,
    )
    .bind(key)
    .bind(window)
    .fetch_one(pool)
    .await?;
    let count: i32 = row.try_get("count")?;
    if i64::from(count) > limit {
        return Err(AdminError { status: StatusCode::TOO_MANY_REQUESTS, message: "rate limit exceeded".to_string() });
    }
    Ok(())
}

async fn rate_limit_value(pool: &PgPool, key: &str, default_limit: i64) -> Result<i64> {
    let row = sqlx::query("select value from admin_settings where key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else {
        return Ok(default_limit);
    };
    Ok(row.try_get::<String, _>("value")?.parse().unwrap_or(default_limit))
}
