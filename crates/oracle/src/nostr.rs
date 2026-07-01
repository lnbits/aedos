use serde_json::json;

use crate::{labels::EventDraft, types::Verdict};

pub fn build_realtime_verdict_event(kind: u64, verdict: &Verdict, sha256: Option<&str>) -> EventDraft {
    let mut tags = vec![
        vec!["d".to_string(), format!("{}:{}", verdict.target_type.as_str(), verdict.target_id)],
        vec![target_tag_name(verdict.target_type.as_str()).to_string(), verdict.target_id.clone()],
    ];
    if let Some(sha256) = sha256 {
        tags.push(vec!["x".to_string(), sha256.to_string()]);
    }

    EventDraft {
        kind,
        tags,
        content: serde_json::to_string(&json!({
            "event_id": verdict.target_id,
            "status": verdict.status,
            "labels": verdict.labels,
            "confidence": verdict.confidence,
            "sha256": sha256,
        }))
        .expect("static JSON serializes"),
    }
}

pub async fn publish_draft(_draft: &EventDraft) -> anyhow::Result<Option<String>> {
    // The production publisher is intentionally isolated here. It will use nostr-sdk
    // keys and clients once relay credentials are configured; tests exercise the
    // NIP-shaped drafts without needing external relays.
    let _sdk_marker = std::any::type_name::<nostr_sdk::Client>();
    Ok(None)
}

fn target_tag_name(target_type: &str) -> &'static str {
    match target_type {
        "event" => "e",
        "pubkey" => "p",
        "url" => "r",
        "image" => "x",
        _ => "e",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::types::{TargetType, Verdict};

    #[test]
    fn realtime_event_uses_configured_kind_and_json_content() {
        let verdict = Verdict::safe(TargetType::Event, "f".repeat(64), "test");
        let draft = build_realtime_verdict_event(31494, &verdict, Some(&"a".repeat(64)));
        let content: Value = serde_json::from_str(&draft.content).unwrap();

        assert_eq!(draft.kind, 31494);
        assert_eq!(content["event_id"], "f".repeat(64));
        assert_eq!(content["status"], "safe");
        assert!(draft.tags.contains(&vec!["e".into(), "f".repeat(64)]));
        assert!(draft.tags.contains(&vec!["x".into(), "a".repeat(64)]));
    }

    #[tokio::test]
    async fn publisher_stub_is_callable_without_relays() {
        let verdict = Verdict::safe(TargetType::Event, "f".repeat(64), "test");
        let draft = build_realtime_verdict_event(31494, &verdict, None);
        assert!(publish_draft(&draft).await.unwrap().is_none());
    }
}
