use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::{sync::mpsc, time::{Duration, Instant, interval}};
use uuid::Uuid;

use crate::{
    error::{AppError, Result},
    realtime::{
        core::RealtimeCtx,
        protocol::{inbound::Inbound, types::{SessionId, UserId}},
    },
    state::AppState,
    transport::ws::codec,
};

const PING_EVERY: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn handle_ws(socket: WebSocket, state: AppState) {
    if let Err(e) = handle_ws_inner(socket, state).await {
        tracing::warn!("ws ended with error: {e:?}");
    }
}

async fn handle_ws_inner(socket: WebSocket, state: AppState) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(1024);

    let session_id = SessionId::new();
    let mut user_id = UserId(format!("guest-{}", Uuid::new_v4()));

    state.core.register(crate::realtime::core::session::Connection {
        session_id: session_id.clone(),
        user_id: user_id.clone(),
        tx: out_tx.clone(),
    })?;

    let _ = out_tx
        .send(Message::Text(format!(
            r#"{{"svc":"sys","type":"welcome","data":{{"user_id":"{}"}}}}"#,
            user_id.0
        )))
        .await;

    let mut last_seen = Instant::now();
    let mut ping = interval(PING_EVERY);

    loop {
        tokio::select! {
            Some(out) = out_rx.recv() => {
                ws_tx.send(out).await.map_err(|e| AppError::WebSocket(e.to_string()))?;
            }

            _ = ping.tick() => {
                if last_seen.elapsed() > IDLE_TIMEOUT {
                    tracing::info!("idle timeout: {}", user_id.0);
                    break;
                }
                let _ = ws_tx.send(Message::Ping(vec![])).await;
            }

            incoming = ws_rx.next() => {
                match incoming {
                    Some(Ok(msg)) => {
                        last_seen = Instant::now();

                        // Control frames fast-path
                        match &msg {
                            Message::Ping(payload) => {
                                let _ = ws_tx.send(Message::Pong(payload.clone())).await;
                                continue;
                            }
                            Message::Pong(_) => continue,
                            Message::Close(_) => break,
                            _ => {}
                        }

                        // ✅ Decode ONCE (no double parsing)
                        if let Some(inb) = codec::decode(msg)? {
                            let ctx = RealtimeCtx::new(session_id.clone(), user_id.clone(), state.core.clone());

                            match inb {
                                Inbound::Text(env) => {
                                    // Auth: header check only; parse body lazily ONLY for auth.
                                    if env.svc == "auth" && env.msg_type == "ticket" {
                                        #[derive(serde::Deserialize)]
                                        struct TicketReq { ticket: String }

                                        let ticket = env.data
                                            .as_ref()
                                            .and_then(|raw| serde_json::from_str::<TicketReq>(raw.get()).ok())
                                            .map(|r| r.ticket)
                                            .unwrap_or_default();

                                        match state.ticket_store.consume_ticket(&ticket) {
                                            Ok(resolved) => {
                                                state.core.unregister(&user_id);
                                                user_id = UserId(resolved);

                                                state.core.register(crate::realtime::core::session::Connection {
                                                    session_id: session_id.clone(),
                                                    user_id: user_id.clone(),
                                                    tx: out_tx.clone(),
                                                })?;

                                                let _ = out_tx
                                                    .send(Message::Text(format!(
                                                        r#"{{"svc":"sys","type":"authed","data":{{"user_id":"{}"}}}}"#,
                                                        user_id.0
                                                    )))
                                                    .await;
                                            }
                                            Err(e) => {
                                                let _ = out_tx
                                                    .send(Message::Text(format!(
                                                        r#"{{"svc":"sys","type":"error","data":{{"code":"{}","message":"{}"}}}}"#,
                                                        e.client_code(),
                                                        e.to_string().replace('"', "'")
                                                    )))
                                                    .await;
                                            }
                                        }
                                        continue;
                                    }

                                    state.dispatcher.dispatch(ctx, env).await?;
                                }

                                Inbound::Binary(frame) => {
                                    state.dispatcher.dispatch_binary(ctx, frame).await?;
                                }
                            }
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    state.core.unregister(&user_id);
    Ok(())
}
