use std::{env, net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use serde_json::json;

const DEFAULT_NOSTR_RELAYS: &[&str] = &[
    "wss://relay.nostr.com",
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://nostr.bitcoiner.social",
    "wss://nostr.mom",
    "wss://relay.snort.social",
];

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: Option<String>,
    pub redis_url: Option<String>,
    pub nostr_private_key: Option<String>,
    pub nostr_relays: Vec<String>,
    pub public_base_url: Option<String>,
    pub label_namespace: String,
    pub default_policy: String,
    pub enable_escalation: bool,
    pub max_image_bytes: usize,
    pub image_fetch_timeout: Duration,
    pub worker_concurrency: usize,
    pub http_bind: SocketAddr,
    pub oracle_verdict_kind: u64,
    pub api_keys: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub secure_cookies: bool,
    pub enable_label_publisher: bool,
    pub label_publish_interval_seconds: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let http_bind = env::var("HTTP_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse()
            .context("HTTP_BIND must be host:port")?;

        let public_base_url = optional_env("PUBLIC_BASE_URL");
        let label_namespace = optional_env("LABEL_NAMESPACE")
            .unwrap_or_else(|| default_label_namespace(public_base_url.as_deref()));

        Ok(Self {
            database_url: optional_env("DATABASE_URL"),
            redis_url: optional_env("REDIS_URL"),
            nostr_private_key: optional_env("NOSTR_PRIVATE_KEY"),
            nostr_relays: csv_env_with_default("NOSTR_RELAYS", DEFAULT_NOSTR_RELAYS),
            public_base_url,
            label_namespace,
            default_policy: env::var("DEFAULT_POLICY").unwrap_or_else(|_| "blur_unknown".to_string()),
            enable_escalation: bool_env("ENABLE_ESCALATION", false),
            max_image_bytes: env_usize("MAX_IMAGE_BYTES", 10_000_000),
            image_fetch_timeout: Duration::from_secs(env_u64("IMAGE_FETCH_TIMEOUT_SECONDS", 10)),
            worker_concurrency: env_usize("WORKER_CONCURRENCY", 4),
            http_bind,
            oracle_verdict_kind: env_u64("ORACLE_VERDICT_KIND", 31494),
            api_keys: csv_env("API_KEYS"),
            allowed_origins: csv_env("ALLOWED_ORIGINS"),
            secure_cookies: bool_env("SECURE_COOKIES", false),
            enable_label_publisher: bool_env("ENABLE_LABEL_PUBLISHER", true),
            label_publish_interval_seconds: env_u64("LABEL_PUBLISH_INTERVAL_SECONDS", 10),
        })
    }

    pub fn public_summary(&self) -> serde_json::Value {
        json!({
            "database_configured": self.database_url.is_some(),
            "redis_configured": self.redis_url.is_some(),
            "nostr_private_key_configured": self.nostr_private_key.is_some(),
            "nostr_relays": self.nostr_relays,
            "public_base_url": self.public_base_url,
            "label_namespace": self.label_namespace,
            "default_policy": self.default_policy,
            "enable_escalation": self.enable_escalation,
            "max_image_bytes": self.max_image_bytes,
            "image_fetch_timeout_seconds": self.image_fetch_timeout.as_secs(),
            "worker_concurrency": self.worker_concurrency,
            "http_bind": self.http_bind.to_string(),
            "oracle_verdict_kind": self.oracle_verdict_kind,
            "api_keys_configured": !self.api_keys.is_empty(),
            "allowed_origins": self.allowed_origins,
            "secure_cookies": self.secure_cookies,
            "enable_label_publisher": self.enable_label_publisher,
            "label_publish_interval_seconds": self.label_publish_interval_seconds,
        })
    }
}

fn default_label_namespace(public_base_url: Option<&str>) -> String {
    public_base_url
        .map(|url| url.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "http://localhost:8080".to_string())
}

fn optional_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn csv_env(key: &str) -> Vec<String> {
    env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn csv_env_with_default(key: &str, default: &[&str]) -> Vec<String> {
    let values = csv_env(key);
    if values.is_empty() {
        default.iter().map(|value| (*value).to_string()).collect()
    } else {
        values
    }
}

fn bool_env(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::{csv_env_with_default, default_label_namespace, DEFAULT_NOSTR_RELAYS};

    #[test]
    fn label_namespace_defaults_to_public_base_url() {
        assert_eq!(
            default_label_namespace(Some("https://aedos.example/")),
            "https://aedos.example"
        );
    }

    #[test]
    fn nostr_relays_have_production_defaults() {
        let relays = csv_env_with_default("AEDOS_TEST_MISSING_NOSTR_RELAYS", DEFAULT_NOSTR_RELAYS);
        assert_eq!(
            relays,
            vec![
                "wss://relay.nostr.com",
                "wss://relay.damus.io",
                "wss://nos.lol",
                "wss://nostr.bitcoiner.social",
                "wss://nostr.mom",
                "wss://relay.snort.social",
            ]
        );
    }
}
