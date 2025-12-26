use axum::extract::ws::Message;
use bytes::Bytes;

use crate::{
    error::{AppError, Result},
    realtime::protocol::{envelope::Envelope, inbound::{BinaryFrame, Inbound}},
};

pub fn decode(msg: Message) -> Result<Option<Inbound>> {
    match msg {
        Message::Text(t) => {
            // Lazy parse: Envelope.data is RawValue (no Value-tree allocation).
            let env: Envelope =
                serde_json::from_str(&t).map_err(|_| AppError::BadRequest("invalid envelope json".into()))?;
            Ok(Some(Inbound::Text(env)))
        }
        Message::Binary(b) => {
            let bytes = Bytes::from(b);
            let bin = BinaryFrame::parse(bytes)?;
            Ok(Some(Inbound::Binary(bin)))
        }
        Message::Ping(_) | Message::Pong(_) => Ok(None),
        Message::Close(_) => Ok(None),
    }
}
