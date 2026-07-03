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
use sqlx::postgres::PgListener;
use tokio::sync::mpsc;

use crate::{
    api::{check_or_enqueue, AppState},
    types::{BatchEvent, TargetType, Verdict, VerdictResponse, VerdictStatus},
};

const VERDICT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const VERDICT_NOTIFICATION_CHANNEL: &str = "aedos_verdicts";

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "check")]
    Check {
        event_id: String,
        #[serde(default, alias = "npub")]
        pubkey: Option<String>,
        #[serde(default)]
        image_urls: Vec<String>,
        #[serde(default)]
        video_urls: Vec<String>,
    },
    #[serde(rename = "check_batch")]
    CheckBatch { events: Vec<BatchEvent> },
    #[serde(rename = "subscribe")]
    Subscribe { event_ids: Vec<String> },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { event_ids: Vec<String> },
}

struct WsResponse {
    event_id: String,
    value: serde_json::Value,
    watch: bool,
}

pub async fn ws_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
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
                    Ok(ClientMessage::Check { event_id, pubkey, image_urls, video_urls }) => {
                        respond_to_events(
                            &state,
                            vec![BatchEvent {
                                event_id,
                                pubkey,
                                image_urls,
                                video_urls,
                            }],
                        )
                        .await
                    }
                    Ok(ClientMessage::CheckBatch { events }) => respond_to_events(&state, events).await,
                    Ok(ClientMessage::Subscribe { event_ids }) => subscribe_to_events(&state, event_ids).await,
                    Ok(ClientMessage::Unsubscribe { event_ids }) => {
                        for event_id in event_ids {
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

async fn respond_to_events(state: &AppState, events: Vec<BatchEvent>) -> Vec<WsResponse> {
    let mut responses = Vec::with_capacity(events.len());
    for event in events {
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
