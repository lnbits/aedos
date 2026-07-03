use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::types::{TargetType, Verdict, VerdictStatus, SUPPORTED_LABELS};

pub const NIP32_LABEL_KIND: u64 = 1985;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventDraft {
    pub kind: u64,
    pub tags: Vec<Vec<String>>,
    pub content: String,
}

pub fn labels_for_verdict(verdict: &Verdict) -> Vec<String> {
    if verdict.labels.is_empty() {
        return vec![match verdict.status {
            VerdictStatus::Safe => "safe",
            VerdictStatus::Warn => "unknown",
            VerdictStatus::Block => "unknown",
            VerdictStatus::Unknown => "unknown",
            VerdictStatus::Error => "unknown",
        }
        .to_string()];
    }
    verdict
        .labels
        .iter()
        .filter(|label| SUPPORTED_LABELS.contains(&label.as_str()))
        .cloned()
        .collect()
}

pub fn build_nip32_label(
    namespace: &str,
    target_type: TargetType,
    target_id: &str,
    verdict: &Verdict,
) -> EventDraft {
    let mut tags = vec![vec!["L".to_string(), namespace.to_string()]];
    for label in labels_for_verdict(verdict) {
        tags.push(vec!["l".to_string(), label, namespace.to_string()]);
    }

    match target_type {
        TargetType::Event => tags.push(vec!["e".to_string(), target_id.to_string()]),
        TargetType::Pubkey => tags.push(vec!["p".to_string(), target_id.to_string()]),
        TargetType::Url => tags.push(vec!["r".to_string(), target_id.to_string()]),
        TargetType::Image | TargetType::Video => tags.push(vec!["x".to_string(), target_id.to_string()]),
    }

    EventDraft {
        kind: NIP32_LABEL_KIND,
        tags,
        content: serde_json::to_string(&json!({
            "status": verdict.status,
            "confidence": verdict.confidence,
            "source": verdict.source,
            "explanation": verdict.explanation,
        }))
        .expect("static JSON serializes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{TargetType, Verdict};

    #[test]
    fn builds_nip32_event_label_with_matching_namespace_marks() {
        let mut verdict = Verdict::safe(TargetType::Event, "e".repeat(64), "test");
        verdict.labels = vec!["graphic".to_string(), "violence".to_string()];
        let draft = build_nip32_label(
            "nostr.com/moderation",
            TargetType::Event,
            &"e".repeat(64),
            &verdict,
        );

        assert_eq!(draft.kind, 1985);
        assert!(draft
            .tags
            .contains(&vec!["L".into(), "nostr.com/moderation".into()]));
        assert!(draft.tags.contains(&vec![
            "l".into(),
            "graphic".into(),
            "nostr.com/moderation".into()
        ]));
        assert!(draft.tags.contains(&vec![
            "l".into(),
            "violence".into(),
            "nostr.com/moderation".into()
        ]));
        assert!(draft.tags.contains(&vec!["e".into(), "e".repeat(64)]));
    }

    #[test]
    fn image_targets_use_x_tag_for_sha256() {
        let verdict = Verdict::safe(TargetType::Image, "a".repeat(64), "test");
        let draft = build_nip32_label(
            "nostr.com/moderation",
            TargetType::Image,
            &"a".repeat(64),
            &verdict,
        );
        assert!(draft.tags.contains(&vec!["x".into(), "a".repeat(64)]));
    }

    #[test]
    fn preserves_emergency_csam_label() {
        let verdict = Verdict::csam_suspected(TargetType::Event, "e".repeat(64), "test");
        let draft = build_nip32_label(
            "nostr.com/moderation",
            TargetType::Event,
            &"e".repeat(64),
            &verdict,
        );

        assert!(verdict.requires_emergency_escalation());
        assert!(draft.tags.contains(&vec![
            "l".into(),
            "csam-suspected".into(),
            "nostr.com/moderation".into()
        ]));
    }
}
