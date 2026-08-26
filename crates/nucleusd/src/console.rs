use crate::state::{AppState, LogEvent};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

/// Live console: replays recent history then streams output; client messages are stdin lines.
pub async fn console_ws(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let rt = match state.get(&id) {
        Ok(rt) => rt,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    ws.on_upgrade(move |socket| async move {
        handle_socket(socket, rt).await;
    })
}

async fn handle_socket(socket: WebSocket, rt: Arc<crate::state::ServerRuntime>) {
    use tokio::io::AsyncWriteExt;

    let (mut tx, mut rx) = socket.split();
    for line in rt.recent_logs(200) {
        if tx.send(Message::Text(line.into())).await.is_err() {
            return;
        }
    }

    let mut events = rt.log_tx.subscribe();

    let send_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    let text = match ev {
                        LogEvent::Data(s) => s,
                        LogEvent::Exit(c) => format!("[nucleus] container exited with code {c}"),
                    };
                    if tx.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        let _ = tx.send(Message::Close(None)).await;
    });

    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(text) => {
                let line: String = text.trim_end().to_string();
                let mut taken = { rt.stdin.lock().unwrap().take() };
                if let Some(writer) = taken.as_mut() {
                    if writer
                        .write_all(format!("{line}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    let _ = writer.flush().await;
                    *rt.stdin.lock().unwrap() = taken;
                }
            }
            Message::Close(_) | Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
        }
    }
    send_task.abort();
}
