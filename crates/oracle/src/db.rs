use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::RwLock;

use crate::types::{TargetType, Verdict, VerdictStatus};

#[derive(Clone)]
pub struct Store {
    pool: Option<PgPool>,
    memory: Arc<RwLock<HashMap<(String, String), Verdict>>>,
}

impl Store {
    pub async fn new(database_url: Option<&str>) -> Result<Self> {
        let pool = match database_url {
            Some(url) => Some(PgPool::connect(url).await?),
            None => None,
        };
        Ok(Self {
            pool,
            memory: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn memory() -> Self {
        Self {
            pool: None,
            memory: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn latest_verdict(&self, target_type: TargetType, target_id: &str) -> Result<Option<Verdict>> {
        if let Some(pool) = &self.pool {
            if let Some(row) = sqlx::query(
                r#"
                select id, target_type, target_id, status, safe, warn, block, unknown, error,
                       labels, confidence, source, cache, model_version, explanation
                from verdicts
                where target_type = $1 and target_id = $2
                order by created_at desc
                limit 1
                "#,
            )
            .bind(target_type.as_str())
            .bind(target_id)
            .fetch_optional(pool)
            .await?
            {
                let labels: serde_json::Value = row.try_get("labels")?;
                let labels = serde_json::from_value(labels).unwrap_or_default();
                return Ok(Some(Verdict {
                    id: row.try_get("id")?,
                    target_type,
                    target_id: row.try_get("target_id")?,
                    status: parse_status(row.try_get::<String, _>("status")?.as_str()),
                    safe: row.try_get("safe")?,
                    warn: row.try_get("warn")?,
                    block: row.try_get("block")?,
                    unknown: row.try_get("unknown")?,
                    error: row.try_get("error")?,
                    labels,
                    confidence: row.try_get("confidence")?,
                    source: row.try_get("source")?,
                    cache: true,
                    model_version: row.try_get("model_version")?,
                    explanation: row.try_get("explanation")?,
                }));
            }
        }

        let key = (target_type.as_str().to_string(), target_id.to_string());
        Ok(self.memory.read().await.get(&key).cloned().map(|mut verdict| {
            verdict.cache = true;
            verdict
        }))
    }

    pub async fn store_verdict(&self, verdict: &Verdict) -> Result<()> {
        if let Some(pool) = &self.pool {
            sqlx::query(
                r#"
                insert into verdicts
                (id, target_type, target_id, status, safe, warn, block, unknown, error, labels,
                 confidence, source, cache, model_version, explanation, created_at)
                values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)
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
            .bind(Utc::now())
            .execute(pool)
            .await?;
        }

        let key = (verdict.target_type.as_str().to_string(), verdict.target_id.clone());
        self.memory.write().await.insert(key, verdict.clone());
        Ok(())
    }
}

pub fn status_str(status: &VerdictStatus) -> &'static str {
    match status {
        VerdictStatus::Safe => "safe",
        VerdictStatus::Warn => "warn",
        VerdictStatus::Block => "block",
        VerdictStatus::Unknown => "unknown",
        VerdictStatus::Error => "error",
    }
}

fn parse_status(status: &str) -> VerdictStatus {
    match status {
        "safe" => VerdictStatus::Safe,
        "warn" => VerdictStatus::Warn,
        "block" => VerdictStatus::Block,
        "error" => VerdictStatus::Error,
        _ => VerdictStatus::Unknown,
    }
}
