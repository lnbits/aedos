use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const SUPPORTED_LABELS: &[&str] = &[
    "safe",
    "nsfw",
    "nudity",
    "sexual",
    "sexualised",
    "graphic",
    "gore",
    "violence",
    "weapon",
    "self-harm",
    "hate-symbol",
    "spam",
    "scam",
    "csam-suspected",
    "unknown",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetType {
    Event,
    Image,
    Video,
    Url,
    Pubkey,
}

impl TargetType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Image => "image",
            Self::Video => "video",
            Self::Url => "url",
            Self::Pubkey => "pubkey",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerdictStatus {
    Safe,
    Warn,
    Block,
    Unknown,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Verdict {
    pub id: Uuid,
    pub target_type: TargetType,
    pub target_id: String,
    pub status: VerdictStatus,
    pub safe: bool,
    pub warn: bool,
    pub block: bool,
    pub unknown: bool,
    pub error: bool,
    pub labels: Vec<String>,
    pub confidence: f32,
    pub source: String,
    pub cache: bool,
    pub model_version: Option<String>,
    pub explanation: Option<String>,
}

impl Verdict {
    pub fn unknown(target_type: TargetType, target_id: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            target_type,
            target_id: target_id.into(),
            status: VerdictStatus::Unknown,
            safe: false,
            warn: false,
            block: false,
            unknown: true,
            error: false,
            labels: vec!["unknown".to_string()],
            confidence: 0.0,
            source: "cache_miss".to_string(),
            cache: false,
            model_version: None,
            explanation: None,
        }
    }

    pub fn safe(target_type: TargetType, target_id: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            target_type,
            target_id: target_id.into(),
            status: VerdictStatus::Safe,
            safe: true,
            warn: false,
            block: false,
            unknown: false,
            error: false,
            labels: vec!["safe".to_string()],
            confidence: 1.0,
            source: source.into(),
            cache: false,
            model_version: None,
            explanation: None,
        }
    }

    pub fn csam_suspected(
        target_type: TargetType,
        target_id: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            target_type,
            target_id: target_id.into(),
            status: VerdictStatus::Block,
            safe: false,
            warn: false,
            block: true,
            unknown: false,
            error: false,
            labels: vec!["csam-suspected".to_string()],
            confidence: 1.0,
            source: source.into(),
            cache: false,
            model_version: None,
            explanation: Some("emergency moderation label requiring operator process".to_string()),
        }
    }

    pub fn requires_emergency_escalation(&self) -> bool {
        self.labels.iter().any(|label| label == "csam-suspected")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRequest {
    pub event_id: String,
    #[serde(default, alias = "npub")]
    pub pubkey: Option<String>,
    #[serde(default)]
    pub image_urls: Vec<String>,
    #[serde(default)]
    pub video_urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitRequest {
    pub event_id: Option<String>,
    #[serde(default, alias = "npub")]
    pub pubkey: Option<String>,
    #[serde(default)]
    pub image_urls: Vec<String>,
    #[serde(default)]
    pub video_urls: Vec<String>,
    #[serde(default)]
    pub raw_event: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEvent {
    pub event_id: String,
    #[serde(default, alias = "npub")]
    pub pubkey: Option<String>,
    #[serde(default)]
    pub image_urls: Vec<String>,
    #[serde(default)]
    pub video_urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchCheckRequest {
    pub events: Vec<BatchEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictResponse {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub event_id: String,
    pub status: VerdictStatus,
    pub cache: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

impl VerdictResponse {
    pub fn from_verdict(event_id: String, verdict: &Verdict) -> Self {
        Self {
            message_type: "verdict",
            event_id,
            status: verdict.status.clone(),
            cache: verdict.cache,
            labels: verdict.labels.clone(),
            confidence: Some(verdict.confidence),
        }
    }
}
