use std::sync::atomic::Ordering;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;

use crate::{
    api::{check_or_enqueue, AppState},
    types::{BatchCheckRequest, BatchEvent, CheckRequest, VerdictResponse},
};

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "check")]
    Check {
        event_id: String,
        #[serde(default)]
        image_urls: Vec<String>,
    },
    #[serde(rename = "check_batch")]
    CheckBatch { events: Vec<BatchEvent> },
}

pub async fn ws_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

async fn handle_socket(state: AppState, socket: WebSocket) {
    state.metrics.connected_clients.fetch_add(1, Ordering::Relaxed);
    let (mut sender, mut receiver) = socket.split();

    while let Some(Ok(message)) = receiver.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let responses = match serde_json::from_str::<ClientMessage>(&text) {
            Ok(ClientMessage::Check { event_id, image_urls }) => {
                let req = CheckRequest { event_id, image_urls };
                respond_to_events(&state, vec![BatchEvent { event_id: req.event_id, image_urls: req.image_urls }]).await
            }
            Ok(ClientMessage::CheckBatch { events }) => respond_to_events(&state, events).await,
            Err(err) => vec![serde_json::json!({ "type": "error", "error": err.to_string() })],
        };

        for response in responses {
            if sender.send(Message::Text(response.to_string())).await.is_err() {
                state.metrics.connected_clients.fetch_sub(1, Ordering::Relaxed);
                return;
            }
        }
    }

    state.metrics.connected_clients.fetch_sub(1, Ordering::Relaxed);
}

async fn respond_to_events(state: &AppState, events: Vec<BatchEvent>) -> Vec<serde_json::Value> {
    let mut responses = Vec::with_capacity(events.len());
    let _batch = BatchCheckRequest { events: events.clone() };
    for event in events {
        match check_or_enqueue(state, &event).await {
            Ok(verdict) => responses.push(serde_json::to_value(VerdictResponse::from_verdict(event.event_id, &verdict)).unwrap()),
            Err(err) => responses.push(serde_json::json!({ "type": "error", "error": err.to_string() })),
        }
    }
    responses
}
