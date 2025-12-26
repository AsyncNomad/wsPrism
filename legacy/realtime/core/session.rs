use axum::extract::ws::Message;
use bytes::Bytes;
use dashmap::DashMap;
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::{sync::mpsc, time::timeout};

use crate::{
    error::{AppError, Result},
    realtime::protocol::{
        envelope::{Outgoing, Payload},
        qos::QoS,
        types::{RoomId, SessionId, UserId},
    },
};

use super::presence::Presence;

#[derive(Clone)]
pub struct Connection {
    pub session_id: SessionId,
    pub user_id: UserId,
    pub tx: mpsc::Sender<Message>,
}

pub struct RealtimeCore {
    connections: DashMap<UserId, Connection>,
    presence: Presence,
}

impl RealtimeCore {
    pub fn new() -> Self { Self { connections: DashMap::new(), presence: Presence::default() } }

    pub fn register(&self, conn: Connection) -> Result<()> {
        if self.connections.contains_key(&conn.user_id) {
            return Err(AppError::BadRequest("user already connected".into()));
        }
        self.connections.insert(conn.user_id.clone(), conn);
        Ok(())
    }

    pub fn unregister(&self, user: &UserId) {
        self.connections.remove(user);
        self.presence.cleanup_user(user);
    }

    pub fn join_room(&self, user: &UserId, room: &RoomId) { self.presence.join(user, room); }
    pub fn leave_room(&self, user: &UserId, room: &RoomId) { self.presence.leave(user, room); }

    pub async fn send_to_user(&self, user: &UserId, msg: Outgoing) -> Result<()> {
        let conn = self.connections.get(user).ok_or_else(|| AppError::BadRequest("user not connected".into()))?;
        let prepared = PreparedMsg::try_from(msg.clone())?;
        send_prepared(&conn.tx, prepared, msg.qos, &conn.user_id, self).await
    }

    pub fn publish_room_lossy(&self, room: &RoomId, msg: Outgoing) -> Result<()> {
        let users = self.presence.users_in(room);
        let prepared = PreparedMsg::try_from(msg)?;
        for u in users {
            if let Some(conn) = self.connections.get(&u) {
                let _ = conn.tx.try_send(prepared.clone().into_message());
            }
        }
        Ok(())
    }

    pub async fn publish_room_reliable(&self, room: &RoomId, msg: Outgoing) -> Result<()> {
        let users = self.presence.users_in(room);
        let prepared = PreparedMsg::try_from(msg.clone())?;

        let mut futs = FuturesUnordered::new();
        for u in users {
            if let Some(conn) = self.connections.get(&u) {
                let tx = conn.tx.clone();
                let prepared_clone = prepared.clone();
                let qos = msg.qos;

                futs.push(async move {
                    if tx.try_send(prepared_clone.clone().into_message()).is_ok() {
                        return Ok::<(), AppError>(());
                    }
                    let dur = match qos {
                        QoS::Reliable{timeout_ms} => std::time::Duration::from_millis(timeout_ms),
                        _ => std::time::Duration::from_millis(20),
                    };
                    timeout(dur, tx.send(prepared_clone.into_message()))
                        .await
                        .map_err(|_| AppError::Timeout)?
                        .map_err(|e| AppError::WebSocket(e.to_string()))?;
                    Ok(())
                });
            }
        }

        while let Some(_res) = futs.next().await {}
        Ok(())
    }
}

#[derive(Clone)]
enum PreparedMsg {
    Text(String),
    Binary(Bytes),
    Utf8(Bytes),
}

impl PreparedMsg {
    fn into_message(self) -> Message {
        match self {
            PreparedMsg::Text(s) => Message::Text(s),
            PreparedMsg::Binary(b) => Message::Binary(b.to_vec()),
            PreparedMsg::Utf8(b) => Message::Binary(b.to_vec()),
        }
    }
}

impl TryFrom<Outgoing> for PreparedMsg {
    type Error = AppError;

    fn try_from(msg: Outgoing) -> std::result::Result<Self, Self::Error> {
        match msg.payload {
            Payload::TextJson(t) => {
                let s = serde_json::to_string(&t).map_err(|e| AppError::Internal(e.to_string()))?;
                Ok(PreparedMsg::Text(s))
            }
            Payload::Binary(b) => Ok(PreparedMsg::Binary(b)),
            Payload::Utf8Bytes(b) => Ok(PreparedMsg::Utf8(b)),
        }
    }
}

async fn send_prepared(
    tx: &mpsc::Sender<Message>,
    prepared: PreparedMsg,
    qos: QoS,
    user: &UserId,
    core: &RealtimeCore,
) -> Result<()> {
    match qos {
        QoS::Lossy => {
            let _ = tx.try_send(prepared.into_message());
            Ok(())
        }
        QoS::Reliable { timeout_ms } => {
            if tx.try_send(prepared.clone().into_message()).is_ok() {
                return Ok(());
            }
            let dur = std::time::Duration::from_millis(timeout_ms);
            let res = timeout(dur, tx.send(prepared.into_message())).await;
            match res {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(AppError::WebSocket(e.to_string())),
                Err(_) => {
                    core.unregister(user);
                    Err(AppError::Timeout)
                }
            }
        }
    }
}
