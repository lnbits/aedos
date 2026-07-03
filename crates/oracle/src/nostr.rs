use serde_json::json;

use crate::{labels::EventDraft, types::Verdict};

#[derive(Debug, Clone)]
pub struct PublisherConfig {
    pub private_key: Option<String>,
    pub relays: Vec<String>,
}

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

pub async fn publish_draft(draft: &EventDraft, config: &PublisherConfig) -> anyhow::Result<Option<String>> {
    let Some(private_key) = config.private_key.as_deref().filter(|key| !key.trim().is_empty()) else {
        return Ok(None);
    };
    if config.relays.is_empty() {
        return Ok(None);
    }

    let event = signed_event_from_draft(draft, private_key)?;
    let event_id = event.id.to_string();
    let keys = nostr_sdk::Keys::parse(private_key)?;
    let client = nostr_sdk::Client::new(keys);
    for relay in &config.relays {
        client.add_relay(relay).await?;
    }
    client.connect_with_timeout(std::time::Duration::from_secs(5)).await;
    let output = client.send_event(event).await?;
    if output.success.is_empty() {
        anyhow::bail!("label event was not accepted by any configured relay");
    }
    Ok(Some(event_id))
}

pub fn signed_event_from_draft(draft: &EventDraft, private_key: &str) -> anyhow::Result<nostr_sdk::Event> {
    let keys = nostr_sdk::Keys::parse(private_key)?;
    let tags = draft
        .tags
        .iter()
        .map(|tag| nostr_sdk::Tag::parse(tag.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(nostr_sdk::EventBuilder::new(nostr_sdk::Kind::from_u16(draft.kind as u16), draft.content.clone())
        .tags(tags)
        .sign_with_keys(&keys)?)
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
    async fn publisher_is_noop_without_relay_config() {
        let verdict = Verdict::safe(TargetType::Event, "f".repeat(64), "test");
        let draft = build_realtime_verdict_event(31494, &verdict, None);
        assert!(publish_draft(
            &draft,
            &PublisherConfig {
                private_key: None,
                relays: vec![]
            }
        )
        .await
        .unwrap()
        .is_none());
    }
}
