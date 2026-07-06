use std::{collections::HashSet, sync::atomic::Ordering, time::Duration};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use sqlx::postgres::PgListener;
use tokio::sync::mpsc;

use crate::{
    api::{
        author_list, cached_completed_event_verdict, check_or_enqueue, normalize_event_id_reference,
        prepare_signed_event, AppState,
    },
    types::{BatchEvent, TargetType, Verdict, VerdictResponse, VerdictStatus},
};

const VERDICT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const VERDICT_NOTIFICATION_CHANNEL: &str = "aedos_verdicts";

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "check")]
    Check {
        #[serde(default)]
        event_id: Option<String>,
        #[serde(default, alias = "npub")]
        pubkey: Option<String>,
        #[serde(default)]
        image_urls: Vec<String>,
        #[serde(default)]
        video_urls: Vec<String>,
        #[serde(default)]
        raw_event: Option<Value>,
    },
    #[serde(rename = "check_batch")]
    CheckBatch { events: Vec<BatchEvent> },
    #[serde(rename = "subscribe")]
    Subscribe { event_ids: Vec<String> },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { event_ids: Vec<String> },
    #[serde(rename = "author_list")]
    AuthorList {
        list: AuthorListKind,
        limit: Option<i64>,
        min_events: Option<i64>,
    },
}

struct IncomingEvent {
    event_id: Option<String>,
    pubkey: Option<String>,
    image_urls: Vec<String>,
    video_urls: Vec<String>,
    raw_event: Option<Value>,
}

impl From<BatchEvent> for IncomingEvent {
    fn from(event: BatchEvent) -> Self {
        Self {
            event_id: Some(event.event_id),
            pubkey: event.pubkey,
            image_urls: event.image_urls,
            video_urls: event.video_urls,
            raw_event: event.raw_event,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AuthorListKind {
    Nsfw,
    Csam,
}

struct WsResponse {
    event_id: String,
    value: serde_json::Value,
    watch: bool,
}

pub async fn ws_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

pub async fn firehose_ws_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_firehose_socket(state, socket))
}

async fn handle_socket(state: AppState, socket: WebSocket) {
    state.metrics.connected_clients.fetch_add(1, Ordering::Relaxed);
    let (mut sender, mut receiver) = socket.split();
    let mut watched_event_ids = HashSet::<String>::new();
    let mut poll = tokio::time::interval(VERDICT_POLL_INTERVAL);
    let mut verdict_notifications = spawn_verdict_listener(&state);
    let mut verdict_notifications_closed = false;

    loop {
        tokio::select! {
            maybe_message = receiver.next() => {
                let Some(Ok(message)) = maybe_message else {
                    break;
                };
                let Message::Text(text) = message else {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                    continue;
                };

                let responses = match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(ClientMessage::Check { event_id, pubkey, image_urls, video_urls, raw_event }) => {
                        respond_to_events(
                            &state,
                            vec![IncomingEvent {
                                event_id,
                                pubkey,
                                image_urls,
                                video_urls,
                                raw_event,
                            }],
                        )
                        .await
                    }
                    Ok(ClientMessage::CheckBatch { events }) => respond_to_events(&state, events.into_iter().map(IncomingEvent::from).collect()).await,
                    Ok(ClientMessage::Subscribe { event_ids }) => subscribe_to_events(&state, event_ids).await,
                    Ok(ClientMessage::AuthorList { list, limit, min_events }) => author_list_response(&state, list, limit, min_events).await,
                    Ok(ClientMessage::Unsubscribe { event_ids }) => {
                        for event_id in event_ids {
                            let event_id = normalize_event_id_reference(&event_id).unwrap_or(event_id);
                            watched_event_ids.remove(&event_id);
                        }
                        vec![WsResponse {
                            event_id: String::new(),
                            value: serde_json::json!({ "type": "unsubscribed" }),
                            watch: false,
                        }]
                    }
                    Err(err) => vec![WsResponse {
                        event_id: String::new(),
                        value: serde_json::json!({ "type": "error", "error": err.to_string() }),
                        watch: false,
                    }],
                };

                for response in responses {
                    if response.watch {
                        watched_event_ids.insert(response.event_id);
                    }
                    if sender.send(Message::Text(response.value.to_string())).await.is_err() {
                        state.metrics.connected_clients.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                }
            }
            _ = poll.tick(), if !watched_event_ids.is_empty() => {
                let updates = poll_watched_events(&state, &watched_event_ids).await;
                for response in updates {
                    if !response.watch {
                        watched_event_ids.remove(&response.event_id);
                    }
                    if sender.send(Message::Text(response.value.to_string())).await.is_err() {
                        state.metrics.connected_clients.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                }
            }
            maybe_event_id = verdict_notifications.recv(), if !watched_event_ids.is_empty() && !verdict_notifications_closed => {
                let Some(event_id) = maybe_event_id else {
                    verdict_notifications_closed = true;
                    continue;
                };
                if !watched_event_ids.contains(&event_id) {
                    continue;
                }
                let updates = final_verdict_response(&state, &event_id).await;
                for response in updates {
                    watched_event_ids.remove(&response.event_id);
                    if sender.send(Message::Text(response.value.to_string())).await.is_err() {
                        state.metrics.connected_clients.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                }
            }
        }
    }

    state.metrics.connected_clients.fetch_sub(1, Ordering::Relaxed);
}

async fn handle_firehose_socket(state: AppState, socket: WebSocket) {
    state.metrics.connected_clients.fetch_add(1, Ordering::Relaxed);
    let (mut sender, mut receiver) = socket.split();
    let mut verdict_notifications = spawn_verdict_listener(&state);
    let mut local_verdicts = state.queue.subscribe_verdicts();

    let _ = sender
        .send(Message::Text(
            serde_json::json!({
                "type": "firehose_ready",
                "scope": "event_verdicts"
            })
            .to_string(),
        ))
        .await;

    loop {
        tokio::select! {
            maybe_message = receiver.next() => {
                let Some(Ok(message)) = maybe_message else {
                    break;
                };
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
            maybe_event_id = verdict_notifications.recv() => {
                let Some(event_id) = maybe_event_id else {
                    continue;
                };
                if send_firehose_event(&state, &mut sender, &event_id).await.is_err() {
                    break;
                }
            }
            maybe_verdict = local_verdicts.recv() => {
                let Ok(verdict) = maybe_verdict else {
                    continue;
                };
                if verdict.target_type == TargetType::Event && verdict.status != VerdictStatus::Unknown {
                    let value = firehose_verdict_value(&verdict);
                    if sender.send(Message::Text(value.to_string())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    state.metrics.connected_clients.fetch_sub(1, Ordering::Relaxed);
}

async fn send_firehose_event(
    state: &AppState,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    event_id: &str,
) -> Result<(), ()> {
    let verdict = state
        .store
        .latest_verdict(TargetType::Event, event_id)
        .await
        .ok()
        .flatten()
        .filter(|verdict| verdict.status != VerdictStatus::Unknown);
    let Some(verdict) = verdict else {
        return Ok(());
    };
    sender
        .send(Message::Text(firehose_verdict_value(&verdict).to_string()))
        .await
        .map_err(|_| ())
}

fn firehose_verdict_value(verdict: &Verdict) -> serde_json::Value {
    serde_json::json!({
        "type": "firehose_verdict",
        "target_type": verdict.target_type,
        "target_id": verdict.target_id,
        "verdict": VerdictResponse::from_verdict(verdict.target_id.clone(), verdict),
    })
}

async fn author_list_response(
    state: &AppState,
    list: AuthorListKind,
    limit: Option<i64>,
    min_events: Option<i64>,
) -> Vec<WsResponse> {
    let (name, labels) = match list {
        AuthorListKind::Nsfw => ("nsfw", &["nsfw", "nudity", "sexual", "sexualised"][..]),
        AuthorListKind::Csam => ("csam", &["csam-suspected"][..]),
    };
    match author_list(state, name, labels, limit, min_events).await {
        Ok(response) => vec![WsResponse {
            event_id: String::new(),
            value: serde_json::json!({
                "type": "author_list",
                "list": response.label,
                "min_events": response.min_events,
                "authors": response.authors,
            }),
            watch: false,
        }],
        Err(err) => vec![WsResponse {
            event_id: String::new(),
            value: serde_json::json!({ "type": "error", "error": err.to_string() }),
            watch: false,
        }],
    }
}

fn spawn_verdict_listener(state: &AppState) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(128);
    let Some(pool) = state.store.pool().cloned() else {
        return rx;
    };

    tokio::spawn(async move {
        let Ok(mut listener) = PgListener::connect_with(&pool).await else {
            return;
        };
        if listener.listen(VERDICT_NOTIFICATION_CHANNEL).await.is_err() {
            return;
        }
        while let Ok(notification) = listener.recv().await {
            if tx.send(notification.payload().to_string()).await.is_err() {
                break;
            }
        }
    });

    rx
}

async fn respond_to_events(state: &AppState, events: Vec<IncomingEvent>) -> Vec<WsResponse> {
    let mut responses = Vec::with_capacity(events.len());
    for event in events {
        let fallback_event_id = event.event_id.clone().unwrap_or_default();
        match cached_completed_event_verdict(state, event.event_id.as_deref()).await {
            Ok(Some((event_id, verdict))) => {
                responses.push(verdict_ws_response(event_id, &verdict, false));
                continue;
            }
            Ok(None) => {}
            Err(err) => {
                responses.push(WsResponse {
                    event_id: fallback_event_id,
                    value: serde_json::json!({ "type": "error", "error": err.to_string() }),
                    watch: false,
                });
                continue;
            }
        }
        let event = match prepare_signed_event(
            state,
            event.event_id,
            event.pubkey,
            event.image_urls,
            event.video_urls,
            event.raw_event,
        )
        .await
        {
            Ok(event) => event,
            Err(err) => {
                responses.push(WsResponse {
                    event_id: fallback_event_id,
                    value: serde_json::json!({ "type": "error", "error": err.to_string() }),
                    watch: false,
                });
                continue;
            }
        };
        let has_media = !event.image_urls.is_empty() || !event.video_urls.is_empty();
        match check_or_enqueue(state, &event).await {
            Ok(verdict) => responses.push(verdict_ws_response(event.event_id, &verdict, has_media)),
            Err(err) => responses.push(WsResponse {
                event_id: event.event_id,
                value: serde_json::json!({ "type": "error", "error": err.to_string() }),
                watch: false,
            }),
        }
    }
    responses
}

async fn subscribe_to_events(state: &AppState, event_ids: Vec<String>) -> Vec<WsResponse> {
    let mut responses = Vec::with_capacity(event_ids.len());
    for event_id in event_ids {
        let event_id = match normalize_event_id_reference(&event_id) {
            Ok(event_id) => event_id,
            Err(err) => {
                responses.push(WsResponse {
                    event_id,
                    value: serde_json::json!({ "type": "error", "error": err.to_string() }),
                    watch: false,
                });
                continue;
            }
        };
        let verdict = state
            .store
            .latest_verdict(TargetType::Event, &event_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| Verdict::unknown(TargetType::Event, event_id.clone()));
        responses.push(verdict_ws_response(event_id, &verdict, true));
    }
    responses
}

async fn poll_watched_events(state: &AppState, watched_event_ids: &HashSet<String>) -> Vec<WsResponse> {
    let mut responses = Vec::new();
    for event_id in watched_event_ids {
        responses.extend(final_verdict_response(state, event_id).await);
    }
    responses
}

async fn final_verdict_response(state: &AppState, event_id: &str) -> Vec<WsResponse> {
    let Ok(Some(verdict)) = state.store.latest_verdict(TargetType::Event, event_id).await else {
        return Vec::new();
    };
    if verdict.status == VerdictStatus::Unknown {
        return Vec::new();
    }
    vec![verdict_ws_response(event_id.to_string(), &verdict, false)]
}

fn verdict_ws_response(event_id: String, verdict: &Verdict, watch_unknown: bool) -> WsResponse {
    WsResponse {
        event_id: event_id.clone(),
        value: serde_json::to_value(VerdictResponse::from_verdict(event_id, verdict)).unwrap(),
        watch: watch_unknown && verdict.status == VerdictStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::Store, metrics::Metrics, queue::Queue};
    use nostr_sdk::prelude::{EventBuilder, EventId, JsonUtil, Keys, Nip19Event, ToBech32};
    use std::sync::Arc;

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

    fn signed_note(content: &str) -> (String, Value) {
        let keys = Keys::generate();
        let event = EventBuilder::text_note(content)
            .sign_with_keys(&keys)
            .unwrap();
        (event.id.to_string(), serde_json::from_str(&event.as_json()).unwrap())
    }

    #[tokio::test]
    async fn websocket_check_accepts_full_signed_event_without_event_id() {
        let state = test_state();
        let (event_id, raw_event) = signed_note("hello over websocket");

        let responses = respond_to_events(
            &state,
            vec![IncomingEvent {
                event_id: None,
                pubkey: None,
                image_urls: vec![],
                video_urls: vec![],
                raw_event: Some(raw_event),
            }],
        )
        .await;

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].event_id, event_id);
        assert_eq!(responses[0].value["type"], "verdict");
        assert_eq!(responses[0].value["event_id"], event_id);
        assert_eq!(responses[0].value["status"], "unknown");
    }

    #[tokio::test]
    async fn websocket_check_nevent_returns_cached_verdict_without_relay_fetch() {
        let state = test_state();
        let event_id = EventId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let nevent = Nip19Event::new(event_id, ["wss://relay.example"]).to_bech32().unwrap();
        state
            .store
            .store_verdict(&Verdict::safe(TargetType::Event, event_id.to_string(), "test"))
            .await
            .unwrap();

        let responses = respond_to_events(
            &state,
            vec![IncomingEvent {
                event_id: Some(nevent),
                pubkey: None,
                image_urls: vec![],
                video_urls: vec![],
                raw_event: None,
            }],
        )
        .await;

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].event_id, event_id.to_string());
        assert_eq!(responses[0].value["status"], "safe");
        assert_eq!(responses[0].value["cache"], true);
        assert!(!responses[0].watch);
    }

    #[tokio::test]
    async fn websocket_subscribe_normalizes_hex_note_and_nevent_ids() {
        let state = test_state();
        let hex_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let event_id = EventId::parse(hex_id).unwrap();
        let note = event_id.to_bech32().unwrap();
        let nevent = Nip19Event::new(event_id, ["wss://relay.example"]).to_bech32().unwrap();
        state
            .store
            .store_verdict(&Verdict::safe(TargetType::Event, hex_id, "test"))
            .await
            .unwrap();

        let responses = subscribe_to_events(&state, vec![hex_id.to_string(), note, nevent]).await;

        assert_eq!(responses.len(), 3);
        for response in responses {
            assert_eq!(response.event_id, hex_id);
            assert_eq!(response.value["event_id"], hex_id);
            assert_eq!(response.value["status"], "safe");
        }
    }

    #[tokio::test]
    async fn websocket_subscribe_reports_invalid_event_id() {
        let responses = subscribe_to_events(&test_state(), vec!["not-an-event".to_string()]).await;

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].value["type"], "error");
        assert!(responses[0].value["error"]
            .as_str()
            .unwrap()
            .contains("not a valid Nostr event id"));
    }
}
